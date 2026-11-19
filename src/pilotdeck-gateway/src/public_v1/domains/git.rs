//! `git.*` — repository observation and explicit mutation (S3 `cli.task.t014`).
//!
//! Strategy (brief `_tmp/20261118-s3/briefs/git.md` §3): every git interaction
//! spawns the system `git` binary through `tokio::process::Command` (argv
//! only, no shell — the TS baseline's `spawn(..., {shell:false})` in
//! `ui/server/routes/git.js:12-48`), so porcelain formats, error texts,
//! unborn HEAD and lease semantics match the baseline and the contract. No
//! `git2` (the C dependency would drag openssl/libcurl in for network
//! transports the system git already provides).
//!
//! Layout of this file:
//! - store access on the shared `V1Store` connection (`conn` is `pub(crate)`,
//!   the S3 substrate rule §1.4; the git tables' DDL is registered through
//!   the substrate's `reg.schema` hook),
//! - per-repository async write locks (contract precondition: "串行化Gateway
//!   本仓库写入"),
//! - the spawn layer with baseline error classification,
//! - ref/branch/path validation (baseline `git.js:51-102`),
//! - repository resolution (`projectId` → the t013 files domain's
//!   `resolve_project` over `authorized_roots` → `rev-parse --show-toplevel`),
//! - state observation and the content-derived `stateToken`
//!   (`notes.workspace`: "Gateway 观察到的条件令牌，不是 Git 全局 revision"),
//! - the `git.inspect` diff parser + artifact publication (t013
//!   `artifacts::publish_artifact`),
//! - the idempotent-submit machinery (operation receipts on the shared
//!   `idempotency_records` table, following `store.rs::submit_run`),
//! - the fourteen `git.*` handlers.
//!
//! `git_identity.*` lives in `git_identity.rs` and reuses this module's
//! spawn layer and repository resolution.

use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::envelope::compute_input_digest;
use agentcpm_pd_protocol::errors::{
    CODE_ALREADY_EXISTS, CODE_IDEMPOTENCY_CONFLICT, CODE_INVALID_ARGUMENT, CODE_NOT_FOUND,
    CODE_RESOURCE_LIMIT, CODE_RESULT_UNKNOWN, CODE_REVISION_CONFLICT, CODE_UNAVAILABLE,
    ContractError, EffectStatus, RetrySpec,
};
use base64::Engine;
use rusqlite::Connection;
use rusqlite::OptionalExtension;
use rusqlite::TransactionBehavior;
use rusqlite::params;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::OwnedMutexGuard;
use tokio::time::timeout;

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::runtime::DispatchError;
use crate::public_v1::runtime::PublicV1Runtime;
use crate::public_v1::store::now_rfc3339;

// ---------------------------------------------------------------------------
// Contract error codes this domain owns beyond the shared core set.
// ---------------------------------------------------------------------------

pub(super) const CODE_NOT_A_REPOSITORY: &str = "NOT_A_REPOSITORY";
pub(super) const CODE_INVALID_REF: &str = "INVALID_REF";
pub(super) const CODE_PATH_ESCAPE: &str = "PATH_ESCAPE";
pub(super) const CODE_GIT_CONFLICT: &str = "GIT_CONFLICT";
pub(super) const CODE_NOTHING_TO_COMMIT: &str = "NOTHING_TO_COMMIT";
pub(super) const CODE_IDENTITY_REQUIRED: &str = "IDENTITY_REQUIRED";
pub(super) const CODE_INDEX_SCOPE_CONFLICT: &str = "INDEX_SCOPE_CONFLICT";
pub(super) const CODE_BRANCH_CHECKED_OUT: &str = "BRANCH_CHECKED_OUT";
pub(super) const CODE_UNMERGED_BRANCH: &str = "UNMERGED_BRANCH";
pub(super) const CODE_DIRTY_WORKTREE: &str = "DIRTY_WORKTREE";
pub(super) const CODE_PLAN_EXPIRED: &str = "PLAN_EXPIRED";
pub(super) const CODE_PARTIAL_FAILURE: &str = "PARTIAL_FAILURE";
pub(super) const CODE_AUTHENTICATION_FAILED: &str = "AUTHENTICATION_FAILED";
pub(super) const CODE_REMOTE_UNREACHABLE: &str = "REMOTE_UNREACHABLE";
pub(super) const CODE_REMOTE_REF_CONFLICT: &str = "REMOTE_REF_CONFLICT";
pub(super) const CODE_MODEL_UNAVAILABLE: &str = "MODEL_UNAVAILABLE";
pub(super) const CODE_INVALID_IDENTITY: &str = "INVALID_IDENTITY";

const GIT_BINARY: &str = "git";
const GIT_TIMEOUT: Duration = Duration::from_secs(120);
const GIT_NETWORK_TIMEOUT: Duration = Duration::from_secs(300);
/// A single file's hunks inside a diff artifact are capped; the cap is
/// reported through `FileDiff.truncated`, never silently dropped.
const MAX_HUNK_BYTES: usize = 1024 * 1024;
/// Restore plans expire after ten minutes (contract: `PLAN_EXPIRED`).
const PLAN_TTL_SECS: i64 = 600;
/// Upper bound for `page.limit` on inspect views (RESOURCE_LIMIT above).
const MAX_PAGE_LIMIT: u64 = 10_000;
/// Idempotency records retention window (24 h, matching `run.submit`'s).
const RETENTION_SECS: i64 = 24 * 60 * 60;

pub(super) fn git_err(
    code: &str,
    message: impl Into<String>,
    retry: RetrySpec,
    effect: EffectStatus,
) -> ContractError {
    ContractError::new(code, message, retry, effect)
}

pub(super) fn invalid_argument(message: impl Into<String>) -> ContractError {
    git_err(
        CODE_INVALID_ARGUMENT,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn not_a_repository() -> ContractError {
    git_err(
        CODE_NOT_A_REPOSITORY,
        "Not a git repository. This directory does not contain a .git folder. Initialize a git repository with \"git init\" to use source control features.",
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn revision_conflict(message: impl Into<String>) -> ContractError {
    git_err(
        CODE_REVISION_CONFLICT,
        message,
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

fn invalid_ref_error() -> ContractError {
    git_err(
        CODE_INVALID_REF,
        "unknown revision or bad revision",
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

// ---------------------------------------------------------------------------
// Store access.
// ---------------------------------------------------------------------------

/// The git domain's tables live in the same `<home>/v1/state.sqlite`
/// database as the S2 core schema, through the shared `V1Store` connection
/// (`conn` is `pub(crate)`; the S3 substrate rule §1.4 lets domains keep
/// their store methods in their own `impl V1Store` blocks and their DDL in
/// `reg.schema` — this file only reads/writes its own tables through the
/// same connection).
pub(super) fn map_sql_err(err: rusqlite::Error) -> ContractError {
    git_err(
        CODE_UNAVAILABLE,
        format!("git domain storage error: {err}"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

/// Monotonic revision shared with `store.rs`'s `meta.rev_counter` (same
/// database file, same counter the store's own `next_rev_in_tx` uses).
pub(super) fn next_domain_rev(conn: &Connection) -> Result<String, ContractError> {
    let cur: i64 = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(map_sql_err)?
        .unwrap_or(0);
    let next = cur + 1;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('rev_counter', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![next.to_string()],
    )
    .map_err(map_sql_err)?;
    Ok(format!("rev-{next:08}"))
}

fn rfc3339_plus(seconds: i64) -> String {
    (chrono::Utc::now() + chrono::TimeDelta::seconds(seconds)).to_rfc3339()
}

// ---------------------------------------------------------------------------
// Per-repository write serialization.
// ---------------------------------------------------------------------------

static REPO_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>> = OnceLock::new();

async fn lock_repo(repo_root: &Path) -> OwnedMutexGuard<()> {
    let registry = REPO_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let lock = {
        let mut guard = registry.lock().expect("repo lock registry");
        guard.entry(repo_root.to_path_buf()).or_default().clone()
    };
    lock.lock_owned().await
}

// ---------------------------------------------------------------------------
// git availability + spawn layer.
// ---------------------------------------------------------------------------

static GIT_AVAILABLE: OnceLock<bool> = OnceLock::new();

fn git_available() -> bool {
    *GIT_AVAILABLE.get_or_init(|| {
        std::process::Command::new(GIT_BINARY)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

fn require_git() -> Result<(), ContractError> {
    if git_available() {
        Ok(())
    } else {
        Err(git_err(
            CODE_UNAVAILABLE,
            "system `git` binary is not available on PATH",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ))
    }
}

/// Environment overrides for a spawned `git` process. Production paths pass
/// an empty slice (inherit — the `system` identity source must be able to
/// read the host's global git config read-only); tests pass an isolated
/// `HOME` and null global/system config.
pub(super) type GitEnv = [(String, String)];

pub(super) struct GitFailure {
    pub(super) stderr: Vec<u8>,
    message: String,
}

pub(super) enum GitError {
    Failure(GitFailure),
    Spawn(String),
    /// The command hit its deadline and its whole process group was killed.
    /// `write` marks invocations that may have mutated repository or remote
    /// state before the kill landed (`git commit` updates refs before its
    /// exit is observable), so their outcome is unknown rather than
    /// "not applied"; killed reads are side-effect free and stay retryable.
    Timeout {
        write: bool,
    },
}

impl GitError {
    /// Lowercased combined output for the baseline's string-match
    /// classification (`git.js:343-348,1205-1219,1273-1290`).
    pub(super) fn combined_lower(&self) -> String {
        match self {
            Self::Failure(f) => {
                format!("{} ", f.message).to_lowercase()
                    + &String::from_utf8_lossy(&f.stderr).to_lowercase()
            }
            Self::Spawn(m) => m.to_lowercase(),
            Self::Timeout { .. } => "git command exceeded its time limit".to_lowercase(),
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Failure(f) => {
                let stderr = String::from_utf8_lossy(&f.stderr);
                if stderr.trim().is_empty() {
                    f.message.clone()
                } else {
                    format!("{}: {}", stderr.trim(), f.message)
                }
            }
            Self::Spawn(m) => m.clone(),
            Self::Timeout { .. } => "git command exceeded its time limit".to_owned(),
        }
    }

    pub(super) fn is_not_a_repository(&self) -> bool {
        let t = self.combined_lower();
        t.contains("not a git repository") || t.contains("not inside a git work tree")
    }

    pub(super) fn is_missing_revision(&self) -> bool {
        let t = self.combined_lower();
        t.contains("unknown revision")
            || t.contains("ambiguous argument")
            || t.contains("needed a single revision")
            || t.contains("bad revision")
    }

    pub(super) fn is_dirty_worktree(&self) -> bool {
        let t = self.combined_lower();
        t.contains("would be overwritten") || t.contains("please commit your changes or stash them")
    }

    pub(super) fn is_merge_conflict(&self) -> bool {
        let t = self.combined_lower();
        t.contains("automatic merge failed")
            || t.contains("conflict (content)")
            || t.contains("could not apply")
            || t.contains("conflict (modify/delete)")
    }

    pub(super) fn is_auth_failure(&self) -> bool {
        let t = self.combined_lower();
        t.contains("permission denied")
            || t.contains("authentication failed")
            || t.contains("could not read username")
            || t.contains("terminal prompts disabled")
            || t.contains("fatal: could not read from remote repository")
    }

    pub(super) fn is_remote_unreachable(&self) -> bool {
        let t = self.combined_lower();
        t.contains("could not resolve hostname")
            || t.contains("network is unreachable")
            || t.contains("connection refused")
            || t.contains("failed to connect")
            || t.contains("does not appear to be a git repository")
            || t.contains("could not find remote repository")
    }

    pub(super) fn is_push_rejected(&self) -> bool {
        let t = self.combined_lower();
        t.contains("rejected")
            || t.contains("non-fast-forward")
            || t.contains("fetch first")
            || t.contains("stale info")
    }

    /// A blocked `.git/index.lock` ("Unable to create '...index.lock': File
    /// exists" — verified against git 2.54): another git process holds the
    /// lock, or a killed/crashed one left it stale.
    pub(super) fn is_index_lock(&self) -> bool {
        let t = self.combined_lower();
        t.contains("index.lock") && (t.contains("unable to create") || t.contains("file exists"))
    }
}

fn git_spawn_error(e: &GitError) -> ContractError {
    if e.is_not_a_repository() {
        return not_a_repository();
    }
    if let GitError::Timeout { write: true } = e {
        // The killed command writes (the fetch/pull/push paths already
        // report this shape): a killed `git commit` may have updated refs
        // between our grace deadline and the group kill, so the effect is
        // unknown, never "not applied" (contract: `RESULT_UNKNOWN` is in
        // every git mutation's error list).
        return ContractError::result_unknown(
            None,
            "git command exceeded its time limit and was killed; its effect may have been applied",
        );
    }
    if e.is_index_lock() {
        // The hint comes first so the 256-char message clamp can only eat the
        // original stderr text, never the actionable part.
        return git_err(
            CODE_UNAVAILABLE,
            format!(
                "The .git/index.lock file exists: another git process may be \
                 running in this repository, or the lock is stale. If no git \
                 process is running, remove the lock file and retry. \
                 Original error: {}",
                e.describe()
            ),
            RetrySpec::backoff_read(Some(500)),
            EffectStatus::NotApplied,
        );
    }
    git_err(
        CODE_UNAVAILABLE,
        format!("git failed: {}", e.describe()),
        RetrySpec::backoff_read(Some(500)),
        EffectStatus::NotApplied,
    )
}

fn os(args: &[&str]) -> Vec<std::ffi::OsString> {
    args.iter().map(std::ffi::OsString::from).collect()
}

fn os_ref(argv: &[std::ffi::OsString]) -> Vec<&std::ffi::OsStr> {
    argv.iter().map(std::ffi::OsString::as_os_str).collect()
}

/// Grace after a group SIGTERM before the SIGKILL (git's SIGTERM handler
/// removes `.git/index.lock`; verified against git 2.54 — SIGKILL cannot).
const GIT_TERM_GRACE: Duration = Duration::from_secs(3);

/// Whether a git invocation can mutate repository or remote state — a killed
/// write's outcome is unknown (`EffectStatus::Unknown`), while a killed read
/// is side-effect free and stays retryable. The dual-mode subcommands are
/// classified by the read-only shapes this domain actually runs (`config
/// --get`, `branch --show-current`, `remote` as a bare list); anything not
/// enumerated as a read defaults to a write (fail toward "unknown").
fn is_write_git_command(args: &[&std::ffi::OsStr]) -> bool {
    // The subcommand is the first argv element that is not a global option;
    // `-c <k=v>` consumes its value.
    let mut iter = args.iter();
    let subcommand = loop {
        match iter.next() {
            Some(arg) if arg.to_string_lossy() == "-c" => {
                let _ = iter.next();
            }
            Some(arg) if arg.to_string_lossy().starts_with('-') => {}
            Some(arg) => break Some(arg),
            None => break None,
        }
    };
    let Some(sub) = subcommand else {
        return false;
    };
    let reads_only = match sub.to_string_lossy().as_ref() {
        "rev-parse" | "symbolic-ref" | "status" | "remote" | "for-each-ref" | "diff" | "show"
        | "hash-object" | "log" | "ls-remote" | "cat-file" | "var" | "describe" => true,
        "config" => args.iter().any(|a| a.to_string_lossy() == "--get"),
        "branch" => args.iter().any(|a| a.to_string_lossy() == "--show-current"),
        _ => false,
    };
    !reads_only
}

/// Drains one output pipe to EOF from its own task, so `child.wait()` never
/// races a full pipe (the compat executor's reader-thread pattern).
async fn drain_pipe<R: tokio::io::AsyncRead + Unpin>(mut pipe: Option<R>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(pipe) = pipe.as_mut() {
        use tokio::io::AsyncReadExt;
        let _ = pipe.read_to_end(&mut buf).await;
    }
    buf
}

async fn run_git_timed(
    cwd: &Path,
    args: &[&std::ffi::OsStr],
    env: &GitEnv,
    limit: Duration,
) -> Result<Vec<u8>, GitError> {
    let mut cmd = tokio::process::Command::new(GIT_BINARY);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    // The git child leads its own process group (the same ownership model as
    // the sidecar/hook/bash-runner spawns): a timeout signal must reach the
    // whole tree — `git commit`'s pre-commit hook, `git push`'s ssh helper —
    // so no orphan survives to hold `.git/index.lock`.
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return Err(GitError::Spawn(format!("spawn git: {e}"))),
    };
    let group = git_child_group(&child);
    // Kills whatever is left of the group if this future is dropped
    // mid-flight (a live or unreaped-zombie leader keeps the pgid reserved,
    // so the signal cannot hit a stranger's group).
    let mut guard = crate::process_group::GroupGuard(group);
    let stdout_task = tokio::spawn(drain_pipe(child.stdout.take()));
    let stderr_task = tokio::spawn(drain_pipe(child.stderr.take()));
    // If the deadline fires the wait future is dropped *unresolved*, so the
    // leader is alive or an unreaped zombie — either way the pgid is still
    // ours and the group signals below are safe.
    match timeout(limit, child.wait()).await {
        Ok(Ok(status)) => {
            // Natural exit: the leader is reaped, so the pgid may already be
            // free again — never signal the group (disarm; kill_on_drop is a
            // no-op on a reaped child). A lingering descendant holding a
            // pipe gets a bounded drain, then a hard error instead of the
            // unbounded hang `wait_with_output` had.
            let drained = timeout(GIT_TERM_GRACE, async {
                (
                    stdout_task.await.unwrap_or_default(),
                    stderr_task.await.unwrap_or_default(),
                )
            })
            .await;
            guard.disarm();
            let (stdout, stderr) = match drained {
                Ok(pair) => pair,
                Err(_) => {
                    return Err(GitError::Spawn(
                        "git exited but a descendant still holds its output pipe".into(),
                    ));
                }
            };
            if status.success() {
                return Ok(stdout);
            }
            Err(GitError::Failure(GitFailure {
                stderr,
                message: format!(
                    "Command failed: git {}",
                    args.iter()
                        .map(|a| a.to_string_lossy())
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
            }))
        }
        Ok(Err(e)) => Err(GitError::Spawn(format!("wait git: {e}"))),
        Err(_elapsed) => {
            // Timeout: SIGTERM the whole group first — git's SIGTERM handler
            // removes its lock files (verified: SIGKILL leaves a partial
            // commit's `.git/index.lock` behind and bricks the repository's
            // write path) — then a bounded grace that ends early once the
            // leader is gone, then SIGKILL whatever survives.
            guard.signal(rustix::process::Signal::TERM);
            let _ = crate::process_group::wait_for_exit_unreaped(group, GIT_TERM_GRACE).await;
            guard.signal(rustix::process::Signal::KILL);
            // Reap the leader so no zombie remains; the drained output is
            // discarded on this path anyway.
            if group.is_none() {
                // Non-unix: no owned group, so only the direct child dies.
                let _ = child.kill().await;
            }
            let _ = child.wait().await;
            guard.disarm();
            let _ = timeout(GIT_TERM_GRACE, async {
                let _ = stdout_task.await;
                let _ = stderr_task.await;
            })
            .await;
            Err(GitError::Timeout {
                write: is_write_git_command(args),
            })
        }
    }
}

/// The git child's own process group (it is the leader thanks to
/// `process_group(0)`); `None` where groups are unavailable.
#[cfg(unix)]
fn git_child_group(child: &tokio::process::Child) -> Option<rustix::process::Pid> {
    child
        .id()
        .and_then(|id| rustix::process::Pid::from_raw(id as i32))
}

#[cfg(not(unix))]
fn git_child_group(_child: &tokio::process::Child) -> Option<rustix::process::Pid> {
    None
}

/// Runs `git` in `cwd` capturing stdout as raw bytes (`-z` output stays
/// byte-exact; nothing is shell-quoted).
pub(super) async fn git_run(
    cwd: &Path,
    args: &[&std::ffi::OsStr],
    env: &GitEnv,
) -> Result<Vec<u8>, GitError> {
    run_git_timed(cwd, args, env, GIT_TIMEOUT).await
}

/// Static-argv convenience wrapper (the common case).
pub(super) async fn git_run_str(root: &Path, parts: &[&str]) -> Result<Vec<u8>, GitError> {
    let argv = os(parts);
    git_run(root, &os_ref(&argv), &[]).await
}

/// Static-argv wrapper with the network-operation timeout.
async fn git_run_net(root: &Path, parts: &[&str]) -> Result<Vec<u8>, GitError> {
    let argv = os(parts);
    run_git_timed(root, &os_ref(&argv), &[], GIT_NETWORK_TIMEOUT).await
}

/// `git` with one trailing dynamic byte argument (paths, refs).
async fn git_run_bytes(
    cwd: &Path,
    prefix: &[&str],
    byte_arg: &[u8],
    env: &GitEnv,
) -> Result<Vec<u8>, GitError> {
    let mut argv = os(prefix);
    argv.push(bytes_to_os(byte_arg));
    git_run(cwd, &os_ref(&argv), env).await
}

#[cfg(unix)]
pub(super) fn bytes_to_os(bytes: &[u8]) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::OsString::from(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
pub(super) fn bytes_to_os(bytes: &[u8]) -> std::ffi::OsString {
    String::from_utf8_lossy(bytes).into_owned().into()
}

// ---------------------------------------------------------------------------
// Baseline validation helpers (git.js:51-102).
// ---------------------------------------------------------------------------

fn valid_branch_name(branch: &str) -> bool {
    !branch.is_empty()
        && !branch.starts_with('-')
        && branch
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'/' | b'_' | b'-'))
}

fn valid_remote_name(remote: &str) -> bool {
    !remote.is_empty()
        && !remote.starts_with('-')
        && remote
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Commit-ish refs: hex hashes, `HEAD~N`, `HEAD^N`, tags, branch names
/// (`git.js:51-57`).
fn valid_commit_ref(rev: &str) -> bool {
    !rev.is_empty()
        && !rev.starts_with('-')
        && rev.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'.' | b'_' | b'~' | b'^' | b'{' | b'}' | b'@' | b'/' | b'-'
                )
        })
}

/// Push/pull refspecs must never smuggle refspec metacharacters into argv.
fn valid_push_ref(rev: &str) -> bool {
    valid_commit_ref(rev) && !rev.contains(':')
}

// ---------------------------------------------------------------------------
// BytePath (contract `$defs.workspace__BytePath`).
// ---------------------------------------------------------------------------

pub(super) fn byte_path_json(bytes: &[u8]) -> Value {
    match std::str::from_utf8(bytes) {
        Ok(text) => json!({"encoding": "utf8", "value": text}),
        Err(_) => json!({
            "encoding": "base64",
            "value": base64::engine::general_purpose::STANDARD.encode(bytes),
        }),
    }
}

pub(super) fn byte_path_bytes(value: &Value) -> Result<Vec<u8>, ContractError> {
    let encoding = value
        .get("encoding")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let text = value
        .get("value")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match encoding {
        "utf8" => Ok(text.as_bytes().to_vec()),
        "base64" => base64::engine::general_purpose::STANDARD
            .decode(text)
            .map_err(|e| invalid_argument(format!("invalid base64 BytePath value: {e}"))),
        other => Err(invalid_argument(format!(
            "invalid BytePath encoding `{other}` (expected utf8 or base64)"
        ))),
    }
}

/// Repo-relative path validation: no NUL, no absolute path, no `..`
/// component (the contract's `PATH_ESCAPE` guards; repository-root
/// containment for destructive operations is re-checked through
/// canonicalization).
pub(super) fn validate_repo_relative_path(path: &[u8]) -> Result<(), ContractError> {
    if path.is_empty() {
        return Err(invalid_argument("path must not be empty"));
    }
    if path.contains(&0) {
        return Err(invalid_argument("path contains NUL"));
    }
    if path.first() == Some(&b'/') {
        return Err(git_err(
            CODE_PATH_ESCAPE,
            "path must be repository-relative",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    for comp in path.split(|&b| b == b'/') {
        if comp == b".." {
            return Err(git_err(
                CODE_PATH_ESCAPE,
                "path traversal detected",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
    }
    Ok(())
}

/// Canonicalized containment check used before any destructive filesystem
/// operation (symlinks cannot escape the repository root).
fn canonical_path_inside(repo_root: &Path, rel: &[u8]) -> Result<PathBuf, ContractError> {
    let joined = repo_root.join(bytes_to_os(rel));
    let root_canon = repo_root
        .canonicalize()
        .map_err(|e| invalid_argument(format!("repository root: {e}")))?;
    let canon = joined.canonicalize().map_err(|_| {
        git_err(
            CODE_NOT_FOUND,
            "path does not exist in the repository",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    if canon != root_canon && !canon.starts_with(&root_canon) {
        return Err(git_err(
            CODE_PATH_ESCAPE,
            "path resolves outside the project workspace",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    Ok(canon)
}

// ---------------------------------------------------------------------------
// Repository resolution.
// ---------------------------------------------------------------------------

pub(super) struct RepoCtx {
    pub(super) project_id: String,
    /// Canonical repository toplevel (an ancestor of the registered
    /// workspace when the project sits inside a larger checkout — the TS
    /// baseline's `getRepositoryRootPath`, git.js:209-212).
    pub(super) repo_root: PathBuf,
    /// The opaque repositoryId reported in results: the canonical toplevel.
    pub(super) repository_id: String,
}

/// Resolves `projectId` → canonical workspace (the t013 `files` domain's
/// `resolve_project`: `authorized_roots` declaration + project
/// `root.relativePath`, the instance home is never a project root) → git
/// toplevel. The toplevel may be an ancestor of the workspace when the
/// project sits inside a larger checkout — the TS baseline's
/// `getRepositoryRootPath` (git.js:209-212).
pub(super) async fn resolve_repo(
    rt: &Arc<PublicV1Runtime>,
    project_id: &str,
) -> Result<RepoCtx, ContractError> {
    let store = rt.store();
    let workspace =
        super::files::resolve_project(store, project_id).map_err(ContractError::from)?;
    // The TS baseline accepts any directory inside a work tree (git.js:146-167).
    let inside = git_run(
        &workspace.workspace,
        &os_ref(&os(&["rev-parse", "--is-inside-work-tree"])),
        &[],
    )
    .await
    .map_err(|e| git_spawn_error(&e))?;
    if String::from_utf8_lossy(&inside).trim() != "true" {
        return Err(not_a_repository());
    }
    let toplevel = git_run(
        &workspace.workspace,
        &os_ref(&os(&["rev-parse", "--show-toplevel"])),
        &[],
    )
    .await
    .map_err(|e| git_spawn_error(&e))?;
    let repo_root = PathBuf::from(String::from_utf8_lossy(&toplevel).trim_end().to_owned());
    let repo_root = repo_root.canonicalize().unwrap_or(repo_root);
    Ok(RepoCtx {
        project_id: project_id.to_owned(),
        repository_id: repo_root.display().to_string(),
        repo_root,
    })
}

// ---------------------------------------------------------------------------
// State observation + stateToken.
// ---------------------------------------------------------------------------

pub(super) struct RepoState {
    pub(super) head_oid: Option<String>,
    pub(super) head_ref: Option<String>,
    pub(super) unborn: bool,
    pub(super) detached: bool,
    pub(super) porcelain_z: Vec<u8>,
    pub(super) remote_refs: Vec<(String, String)>,
    pub(super) remote_names: Vec<String>,
}

impl RepoState {
    /// Content-derived state token: `notes.workspace` — "Gateway 观察到的
    /// 条件令牌，不是 Git 全局 revision；外部 git 进程同样存在竞态边界，
    /// 必须前后核查". Never persisted; recomputed on every observation.
    pub(super) fn state_token(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"agentcpm-pd.git-state/1\x00");
        hasher.update(self.head_oid.as_deref().unwrap_or_default().as_bytes());
        hasher.update([0]);
        hasher.update(self.head_ref.as_deref().unwrap_or_default().as_bytes());
        hasher.update([0]);
        hasher.update([u8::from(self.detached)]);
        hasher.update([0]);
        hasher.update(&self.porcelain_z);
        hasher.update([0]);
        for (name, oid) in &self.remote_refs {
            hasher.update(name.as_bytes());
            hasher.update(b" ");
            hasher.update(oid.as_bytes());
            hasher.update([0]);
        }
        for name in &self.remote_names {
            hasher.update(name.as_bytes());
            hasher.update([0]);
        }
        format!("gst1-{:x}", hasher.finalize())
    }

    pub(super) fn remote_ref_oid(&self, refname: &str) -> Option<&str> {
        self.remote_refs
            .iter()
            .find(|(name, _)| name == refname)
            .map(|(_, oid)| oid.as_str())
    }
}

pub(super) async fn observe_repo(repo_root: &Path) -> Result<RepoState, ContractError> {
    let head_oid = match git_run(
        repo_root,
        &os_ref(&os(&["rev-parse", "--verify", "HEAD"])),
        &[],
    )
    .await
    {
        Ok(out) => Some(String::from_utf8_lossy(&out).trim_end().to_owned()),
        Err(e) if e.is_missing_revision() => None,
        Err(e) => return Err(git_spawn_error(&e)),
    };
    let head_ref = match git_run(
        repo_root,
        &os_ref(&os(&["symbolic-ref", "-q", "HEAD"])),
        &[],
    )
    .await
    {
        Ok(out) => Some(String::from_utf8_lossy(&out).trim_end().to_owned()),
        Err(_) => None,
    };
    let porcelain_z = git_run(
        repo_root,
        &os_ref(&os(&["status", "--porcelain=v1", "-z"])),
        &[],
    )
    .await
    .map_err(|e| git_spawn_error(&e))?;
    let remotes_out = git_run(repo_root, &os_ref(&os(&["remote"])), &[])
        .await
        .map_err(|e| git_spawn_error(&e))?;
    let remote_names: Vec<String> = String::from_utf8_lossy(&remotes_out)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    let refs_out = git_run(
        repo_root,
        &os_ref(&os(&[
            "for-each-ref",
            "refs/remotes",
            "--format=%(refname) %(objectname)",
        ])),
        &[],
    )
    .await
    .map_err(|e| git_spawn_error(&e))?;
    let remote_refs: Vec<(String, String)> = String::from_utf8_lossy(&refs_out)
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let (name, oid) = line.split_once(' ')?;
            Some((name.to_owned(), oid.to_owned()))
        })
        .collect();
    Ok(RepoState {
        unborn: head_oid.is_none(),
        detached: head_oid.is_some() && head_ref.is_none(),
        head_oid,
        head_ref,
        porcelain_z,
        remote_refs,
        remote_names,
    })
}

// ---------------------------------------------------------------------------
// Porcelain status parsing.
// ---------------------------------------------------------------------------

pub(super) struct StatusEntry {
    pub(super) x: u8,
    pub(super) y: u8,
    pub(super) path: Vec<u8>,
    pub(super) original_path: Option<Vec<u8>>,
}

pub(super) fn parse_porcelain_z(data: &[u8]) -> Vec<StatusEntry> {
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let Some(nul) = rest.iter().position(|&b| b == 0) else {
            break;
        };
        let record = &rest[..nul];
        rest = &rest[nul + 1..];
        if record.len() < 3 || record[2] != b' ' {
            continue;
        }
        let mut entry = StatusEntry {
            x: record[0],
            y: record[1],
            path: record[3..].to_vec(),
            original_path: None,
        };
        // porcelain v1 -z renames/copies append the original path as its own
        // NUL-terminated record ("R  new\0old\0").
        if (entry.x == b'R' || entry.x == b'C')
            && let Some(nul2) = rest.iter().position(|&b| b == 0)
        {
            entry.original_path = Some(rest[..nul2].to_vec());
            rest = &rest[nul2 + 1..];
        }
        out.push(entry);
    }
    out
}

impl StatusEntry {
    pub(super) fn untracked(&self) -> bool {
        self.x == b'?' && self.y == b'?'
    }

    pub(super) fn conflicted(&self) -> bool {
        matches!(
            (self.x, self.y),
            (b'D', b'D')
                | (b'A', b'U')
                | (b'U', b'D')
                | (b'U', b'A')
                | (b'D', b'U')
                | (b'U', b'U')
                | (b'A', b'A')
        )
    }
}

/// Binary detection: `git diff --numstat -z` marks binary files with `-`
/// counts (worktree-vs-index and index-vs-HEAD); untracked files are sniffed
/// for a NUL byte in their first 8 KiB.
async fn detect_binary_paths(repo_root: &Path, entries: &[StatusEntry]) -> HashSet<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut binary = HashSet::new();
    for range in ["", "--cached"] {
        let parts: Vec<&str> = if range.is_empty() {
            vec!["diff", "--numstat", "-z"]
        } else {
            vec!["diff", range, "--numstat", "-z"]
        };
        let Ok(out) = git_run(repo_root, &os_ref(&os(&parts)), &[]).await else {
            continue;
        };
        for record in out.split(|&b| b == 0) {
            if record.is_empty() {
                continue;
            }
            let parts: Vec<&[u8]> = record.splitn(3, |&b| b == b'\t').collect();
            if parts.len() == 3 && parts[0] == b"-" && parts[1] == b"-" {
                binary.insert(parts[2].to_vec());
            }
        }
    }
    for entry in entries.iter().filter(|e| e.untracked()) {
        let path = repo_root.join(bytes_to_os(&entry.path));
        if let Ok(mut file) = tokio::fs::File::open(&path).await {
            let mut buf = vec![0u8; 8192];
            if let Ok(n) = file.read(&mut buf).await
                && n > 0
                && buf[..n].contains(&0)
            {
                binary.insert(entry.path.clone());
            }
        }
    }
    binary
}

fn git_path_state_json(entry: &StatusEntry, binary: bool) -> Value {
    let mut obj = Map::new();
    obj.insert("path".into(), byte_path_json(&entry.path));
    if let Some(orig) = &entry.original_path {
        obj.insert("originalPath".into(), byte_path_json(orig));
    }
    obj.insert(
        "indexStatus".into(),
        json!(String::from_utf8_lossy(&[entry.x]).into_owned()),
    );
    obj.insert(
        "worktreeStatus".into(),
        json!(String::from_utf8_lossy(&[entry.y]).into_owned()),
    );
    obj.insert("untracked".into(), json!(entry.untracked()));
    obj.insert("conflicted".into(), json!(entry.conflicted()));
    obj.insert("binary".into(), json!(binary));
    Value::Object(obj)
}

// ---------------------------------------------------------------------------
// Unified diff parsing (`git.diff.v1` FileDiff structures).
// ---------------------------------------------------------------------------

struct FileDiffAcc {
    old_path: Option<Vec<u8>>,
    new_path: Option<Vec<u8>>,
    change: Option<&'static str>,
    binary: bool,
    old_oid: Option<String>,
    new_oid: Option<String>,
    old_mode: Option<String>,
    new_mode: Option<String>,
    combined: bool,
    hunks: Vec<Value>,
    truncated: bool,
    hunk_bytes: usize,
    in_hunk: bool,
    old_ln: u64,
    new_ln: u64,
}

impl FileDiffAcc {
    fn new(old: Option<Vec<u8>>, new: Option<Vec<u8>>) -> Self {
        Self {
            old_path: old,
            new_path: new,
            change: None,
            binary: false,
            old_oid: None,
            new_oid: None,
            old_mode: None,
            new_mode: None,
            combined: false,
            hunks: Vec::new(),
            truncated: false,
            hunk_bytes: 0,
            in_hunk: false,
            old_ln: 0,
            new_ln: 0,
        }
    }

    fn finalize(mut self) -> Value {
        let change = self.change.unwrap_or_else(|| {
            if self.combined {
                "unmerged"
            } else if self.old_path.is_none() && self.new_path.is_some() {
                "added"
            } else if self.old_path.is_some() && self.new_path.is_none() {
                "deleted"
            } else if self.old_mode.is_some()
                && self.new_mode.is_some()
                && self.old_mode != self.new_mode
            {
                "typeChanged"
            } else {
                "modified"
            }
        });
        let mut obj = Map::new();
        obj.insert(
            "oldPath".into(),
            self.old_path
                .as_deref()
                .map(byte_path_json)
                .unwrap_or(Value::Null),
        );
        obj.insert(
            "newPath".into(),
            self.new_path
                .as_deref()
                .map(byte_path_json)
                .unwrap_or(Value::Null),
        );
        obj.insert("change".into(), json!(change));
        obj.insert("binary".into(), json!(self.binary));
        obj.insert(
            "oldOid".into(),
            self.old_oid.map(|v| json!(v)).unwrap_or(Value::Null),
        );
        obj.insert(
            "newOid".into(),
            self.new_oid.map(|v| json!(v)).unwrap_or(Value::Null),
        );
        obj.insert(
            "oldMode".into(),
            self.old_mode.map(|v| json!(v)).unwrap_or(Value::Null),
        );
        obj.insert(
            "newMode".into(),
            self.new_mode.map(|v| json!(v)).unwrap_or(Value::Null),
        );
        obj.insert(
            "hunks".into(),
            Value::Array(std::mem::take(&mut self.hunks)),
        );
        obj.insert("truncated".into(), json!(self.truncated));
        Value::Object(obj)
    }
}

fn unquote_git_path(raw: &[u8]) -> Vec<u8> {
    if raw.len() >= 2 && raw[0] == b'"' && raw[raw.len() - 1] == b'"' {
        let inner = &raw[1..raw.len() - 1];
        let mut out = Vec::with_capacity(inner.len());
        let mut i = 0;
        while i < inner.len() {
            if inner[i] == b'\\' && i + 1 < inner.len() {
                match inner[i + 1] {
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    b'r' => out.push(b'\r'),
                    b'"' => out.push(b'"'),
                    b'\\' => out.push(b'\\'),
                    c if c.is_ascii_digit() && i + 3 < inner.len() => {
                        // Octal escape `\NNN`.
                        let octal = std::str::from_utf8(&inner[i + 1..i + 4]).unwrap_or("");
                        if let Ok(byte) = u8::from_str_radix(octal, 8) {
                            out.push(byte);
                            i += 2; // +2 below = 4 total (backslash + 3 digits)
                        } else {
                            out.push(inner[i + 1]);
                        }
                    }
                    c => out.push(c),
                }
                i += 2;
            } else {
                out.push(inner[i]);
                i += 1;
            }
        }
        out
    } else {
        raw.to_vec()
    }
}

/// Strips the `a/` or `b/` prefix from a `--- `/`+++ ` marker line.
fn marker_path(line: &[u8], prefix: u8) -> Option<Vec<u8>> {
    let rest = &line[4..];
    if rest == b"/dev/null" {
        return None;
    }
    let path = if rest.first() == Some(&prefix) {
        &rest[2..]
    } else {
        rest
    };
    Some(unquote_git_path(path))
}

fn parse_hunk_header(line: &[u8]) -> Option<(u64, u64, u64, u64, String)> {
    let text = String::from_utf8_lossy(line);
    let body = text.trim_start_matches('@');
    let (ranges, heading) = body.split_once('@')?;
    let mut old: Option<(u64, u64)> = None;
    let mut new: Option<(u64, u64)> = None;
    for token in ranges.split_whitespace() {
        if let Some(spec) = token.strip_prefix('-')
            && old.is_none()
        {
            let (start, count) = spec.split_once(',').unwrap_or((spec, "1"));
            if let (Ok(s), Ok(c)) = (start.parse::<u64>(), count.parse::<u64>()) {
                old = Some((s, c));
            }
        } else if let Some(spec) = token.strip_prefix('+')
            && new.is_none()
        {
            let (start, count) = spec.split_once(',').unwrap_or((spec, "1"));
            if let (Ok(s), Ok(c)) = (start.parse::<u64>(), count.parse::<u64>()) {
                new = Some((s, c));
            }
        }
    }
    let (old_start, old_count) = old.unwrap_or((0, 0));
    let (new_start, new_count) = new.unwrap_or((0, 0));
    Some((
        old_start,
        old_count,
        new_start,
        new_count,
        heading.trim_start().to_owned(),
    ))
}

/// Parses `git diff` unified output (run with `--full-index` and
/// `-c core.quotePath=false`) into `git.diff.v1` `FileDiff` structures.
pub(super) fn parse_unified_diff(input: &[u8]) -> Vec<Value> {
    let mut files: Vec<FileDiffAcc> = Vec::new();
    for line in split_diff_lines(input) {
        if line.starts_with(b"diff --git ") {
            let rest = &line[b"diff --git ".len()..];
            let (old, new) = split_git_header(rest);
            files.push(FileDiffAcc::new(old, new));
            continue;
        }
        let Some(acc) = files.last_mut() else {
            continue;
        };
        if line.starts_with(b"old mode ") {
            acc.old_mode = Some(
                String::from_utf8_lossy(&line[b"old mode ".len()..])
                    .trim()
                    .to_owned(),
            );
        } else if line.starts_with(b"new mode ") {
            acc.new_mode = Some(
                String::from_utf8_lossy(&line[b"new mode ".len()..])
                    .trim()
                    .to_owned(),
            );
        } else if let Some(mode) = line.strip_prefix(b"deleted file mode ") {
            acc.change = Some("deleted");
            acc.old_mode = Some(String::from_utf8_lossy(mode).trim().to_owned());
        } else if let Some(mode) = line.strip_prefix(b"new file mode ") {
            acc.change = Some("added");
            acc.new_mode = Some(String::from_utf8_lossy(mode).trim().to_owned());
        } else if let Some(from) = line.strip_prefix(b"rename from ") {
            if acc.change.is_none() {
                acc.change = Some("renamed");
            }
            // rename from/to carry exact paths — correct any earlier guess.
            let path = unquote_git_path(from);
            if acc.old_path.is_none() {
                acc.old_path = Some(path);
            }
        } else if let Some(to) = line.strip_prefix(b"rename to ") {
            let path = unquote_git_path(to);
            acc.new_path = Some(path);
            if acc.change.is_none() {
                acc.change = Some("renamed");
            }
        } else if line.starts_with(b"copy from ") || line.starts_with(b"copy to ") {
            if acc.change.is_none() {
                acc.change = Some("copied");
            }
        } else if line.starts_with(b"index ") {
            let rest = String::from_utf8_lossy(&line[b"index ".len()..]).to_string();
            let mut parts = rest.split_whitespace();
            if let Some(oids) = parts.next() {
                let (old, new) = oids.split_once("..").unwrap_or((oids, ""));
                if !old.is_empty() && old != "0000000" {
                    acc.old_oid = Some(old.to_owned());
                }
                if !new.is_empty() && new != "0000000" {
                    acc.new_oid = Some(new.to_owned());
                }
            }
            if let Some(mode) = parts.next()
                && acc.new_mode.is_none()
            {
                acc.new_mode = Some(mode.to_owned());
            }
        } else if line.starts_with(b"--- ") {
            acc.old_path = marker_path(line, b'a');
        } else if line.starts_with(b"+++ ") {
            acc.new_path = marker_path(line, b'b');
        } else if line.starts_with(b"Binary files ") || line.starts_with(b"GIT binary patch") {
            acc.binary = true;
        } else if line.starts_with(b"@@") {
            if let Some((old_start, old_count, new_start, new_count, heading)) =
                parse_hunk_header(line)
            {
                acc.combined = acc.combined || line.starts_with(b"@@@");
                acc.old_ln = old_start;
                acc.new_ln = new_start;
                acc.hunks.push(json!({
                    "oldStart": old_start,
                    "oldCount": old_count,
                    "newStart": new_start,
                    "newCount": new_count,
                    "heading": heading,
                    "lines": [],
                }));
                acc.in_hunk = true;
            }
        } else if acc.in_hunk
            && matches!(
                line.first(),
                Some(b' ') | Some(b'+') | Some(b'-') | Some(b'\\')
            )
        {
            if acc.truncated {
                continue;
            }
            let kind = match line.first() {
                Some(b'+') => "added",
                Some(b'-') => "deleted",
                Some(b'\\') => "noNewline",
                _ => "context",
            };
            let content = &line[1..];
            acc.hunk_bytes += content.len();
            let mut line_obj = Map::new();
            line_obj.insert("kind".into(), json!(kind));
            line_obj.insert(
                "bytesBase64".into(),
                json!(base64::engine::general_purpose::STANDARD.encode(content)),
            );
            if kind == "context" {
                line_obj.insert("oldLine".into(), json!(acc.old_ln));
                line_obj.insert("newLine".into(), json!(acc.new_ln));
                acc.old_ln += 1;
                acc.new_ln += 1;
            } else if kind == "added" {
                line_obj.insert("newLine".into(), json!(acc.new_ln));
                acc.new_ln += 1;
            } else if kind == "deleted" {
                line_obj.insert("oldLine".into(), json!(acc.old_ln));
                acc.old_ln += 1;
            }
            if let Some(Value::Object(h)) = acc.hunks.last_mut()
                && let Some(Value::Array(lines)) = h.get_mut("lines")
            {
                lines.push(Value::Object(line_obj));
            }
            if acc.hunk_bytes > MAX_HUNK_BYTES {
                acc.truncated = true;
            }
        }
    }
    files.into_iter().map(FileDiffAcc::finalize).collect()
}

fn split_diff_lines(input: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (i, &b) in input.iter().enumerate() {
        if b == b'\n' {
            lines.push(&input[start..i]);
            start = i + 1;
        }
    }
    if start < input.len() {
        lines.push(&input[start..]);
    }
    lines
}

/// Best-effort split of the `diff --git a/old b/new` header; ambiguous paths
/// (spaces) are corrected later by the `---`/`+++`/rename markers.
fn split_git_header(rest: &[u8]) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let text = String::from_utf8_lossy(rest).into_owned();
    if let Some(a) = text.strip_prefix("a/")
        && let Some(idx) = a.find(" b/")
    {
        return (
            Some(unquote_git_path(&a.as_bytes()[..idx])),
            Some(unquote_git_path(&a.as_bytes()[idx + 3..])),
        );
    }
    (None, None)
}

/// Synthesizes a full-file addition `FileDiff` for an untracked path (the TS
/// baseline's untracked diff handling, git.js:382-395).
async fn synthesize_untracked_diff(repo_root: &Path, rel_path: &[u8], is_dir: bool) -> Value {
    let mut acc = FileDiffAcc::new(None, Some(rel_path.to_vec()));
    if is_dir {
        acc.binary = true;
        return acc.finalize();
    }
    let abs = repo_root.join(bytes_to_os(rel_path));
    if let Ok(bytes) = tokio::fs::read(&abs).await {
        if bytes.contains(&0) {
            acc.binary = true;
            return acc.finalize();
        }
        let body = &bytes[..bytes.len().min(MAX_HUNK_BYTES)];
        if bytes.len() > MAX_HUNK_BYTES {
            acc.truncated = true;
        }
        let mut lines: Vec<Value> = Vec::new();
        for raw in body.split(|&b| b == b'\n') {
            lines.push(json!({
                "kind": "added",
                "bytesBase64": base64::engine::general_purpose::STANDARD.encode(raw),
            }));
        }
        acc.hunks.push(json!({
            "oldStart": 0,
            "oldCount": 0,
            "newStart": 1,
            "newCount": lines.len(),
            "heading": "",
            "lines": lines,
        }));
    }
    acc.finalize()
}

// ---------------------------------------------------------------------------
// Artifact publication (t013 dependency resolved: `git.inspect` publishes its
// payloads through the t013 artifacts domain's `publish_artifact` — the
// download path `artifacts.get` / `artifacts.read` serves them directly).
// ---------------------------------------------------------------------------

/// Publishes one inspect payload as an immutable artifact through the
/// t013 artifacts domain (`publish_artifact`: staging → fsync → rename into
/// `<home>/v1/artifacts/<artifactId>` + metadata row; served by
/// `artifacts.get` / `artifacts.read`).
fn store_artifact(
    store: &crate::public_v1::store::V1Store,
    project_id: &str,
    display_name: &str,
    content: &[u8],
) -> Result<Value, ContractError> {
    super::artifacts::publish_artifact(
        store,
        project_id,
        "application/json",
        Some(display_name),
        content,
        "git.inspect",
    )
}

// ---------------------------------------------------------------------------
// Idempotent submit machinery (operation receipts).
// ---------------------------------------------------------------------------

struct OpIntake {
    receipt: Value,
    operation_id: String,
    deduplicated: bool,
}

fn operation_receipt(operation_id: &str, revision: &str, state: &str) -> Value {
    json!({
        "kind": "operation",
        "operationId": operation_id,
        "durable": true,
        "acceptedRevision": revision,
        "deduplicated": false,
        "state": state,
    })
}

/// Accepts one idempotent submit: replays the stored receipt for a same-key
/// same-input retry (`deduplicated=true`), answers `IDEMPOTENCY_CONFLICT`
/// for a same-key different-input retry, and otherwise creates the durable
/// operation record (the same `idempotency_records` table `run.submit`
/// uses — `store.rs:892-1077`).
#[allow(clippy::too_many_arguments)]
fn intake_operation(
    store: &crate::public_v1::store::V1Store,
    method_id: &str,
    project_id: &str,
    repository_id: &str,
    input: &Value,
    key: Option<&str>,
    operation_id: &str,
) -> Result<OpIntake, ContractError> {
    let digest = compute_input_digest(method_id, input);
    let now = now_rfc3339();
    let retained_until = rfc3339_plus(RETENTION_SECS);
    let mut conn = store.conn.lock().expect("sqlite mutex");
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sql_err)?;

    if let Some(key) = key {
        let existing: Option<(String, String, Option<String>, Option<String>)> = tx
            .query_row(
                "SELECT input_digest, request_state, receipt_json, rejection_code
                 FROM idempotency_records WHERE idempotency_key = ?1 AND method_id = ?2",
                params![key, method_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(map_sql_err)?;
        if let Some((stored_digest, request_state, receipt_json, rejection_code)) = existing {
            if stored_digest != digest {
                return Err(git_err(
                    CODE_IDEMPOTENCY_CONFLICT,
                    format!(
                        "idempotencyKey `{key}` already used for {method_id} with different input"
                    ),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            if request_state == "accepted"
                && let Some(raw) = receipt_json
                && let Ok(mut receipt) = serde_json::from_str::<Value>(&raw)
                && let Some(obj) = receipt.as_object_mut()
            {
                obj.insert("deduplicated".into(), json!(true));
                let operation_id = obj
                    .get("operationId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                return Ok(OpIntake {
                    receipt,
                    operation_id,
                    deduplicated: true,
                });
            }
            return Err(git_err(
                &rejection_code.unwrap_or_else(|| CODE_INVALID_ARGUMENT.to_owned()),
                "request previously rejected definitively; retry with a new idempotencyKey",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
    }

    let revision = next_domain_rev(&tx)?;
    let input_json = serde_json::to_string(input).unwrap_or_else(|_| "{}".into());
    tx.execute(
        "INSERT INTO git_operations (operation_id, method_id, project_id, repository_id, state, revision, cancel_requested, effect_status, progress_json, input_json, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'running', ?5, 0, 'known', '[]', ?6, ?7, ?7)",
        params![
            operation_id,
            method_id,
            project_id,
            repository_id,
            revision,
            input_json,
            now
        ],
    )
    .map_err(map_sql_err)?;
    let receipt = operation_receipt(operation_id, &revision, "running");
    if let Some(key) = key {
        let resource = json!({"kind": "operation", "operationId": operation_id});
        tx.execute(
            "INSERT INTO idempotency_records (idempotency_key, method_id, input_digest, request_state, reason, resource_json, receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
             VALUES (?1, ?2, ?3, 'accepted', 'receiptFound', ?4, ?5, NULL, ?6, ?7, 'doNotReplay')",
            params![
                key,
                method_id,
                digest,
                serde_json::to_string(&resource).unwrap(),
                serde_json::to_string(&receipt).unwrap(),
                now,
                retained_until
            ],
        )
        .map_err(map_sql_err)?;
    }
    tx.commit().map_err(map_sql_err)?;
    Ok(OpIntake {
        receipt,
        operation_id: operation_id.to_owned(),
        deduplicated: false,
    })
}

fn finish_operation(
    store: &crate::public_v1::store::V1Store,
    operation_id: &str,
    succeeded: bool,
    result: Option<&Value>,
    error: Option<(&str, &str)>,
    effect_known: bool,
) {
    let now = now_rfc3339();
    let Ok(conn) = store.conn.lock() else {
        return;
    };
    let Ok(revision) = next_domain_rev(&conn) else {
        return;
    };
    let result_json = result.and_then(|r| serde_json::to_string(r).ok());
    let (error_code, error_message) = match error {
        Some((code, message)) => (Some(code.to_owned()), Some(message.to_owned())),
        None => (None, None),
    };
    let _ = conn.execute(
        "UPDATE git_operations SET state = ?1, revision = ?2, result_json = ?3, error_code = ?4, error_message = ?5, effect_status = ?6, updated_at = ?7 WHERE operation_id = ?8",
        params![
            if succeeded { "succeeded" } else { "failed" },
            revision,
            result_json,
            error_code,
            error_message,
            if effect_known { "known" } else { "unknown" },
            now,
            operation_id
        ],
    );
}

static EVENT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Updates the replayed receipt's terminal state once the operation
/// finishes, so a same-key retry observes the final state instead of a
/// stale `running`.
fn update_receipt_state(
    store: &crate::public_v1::store::V1Store,
    method_id: &str,
    key: Option<&str>,
    state: &str,
) {
    let Some(key) = key else {
        return;
    };
    let Ok(conn) = store.conn.lock() else {
        return;
    };
    let Ok(raw) = conn.query_row(
        "SELECT receipt_json FROM idempotency_records WHERE idempotency_key = ?1 AND method_id = ?2",
        params![key, method_id],
        |r| r.get::<_, String>(0),
    ) else {
        return;
    };
    if let Ok(mut receipt) = serde_json::from_str::<Value>(&raw)
        && let Some(obj) = receipt.as_object_mut()
    {
        obj.insert("state".into(), json!(state));
        let _ = conn.execute(
            "UPDATE idempotency_records SET receipt_json = ?1 WHERE idempotency_key = ?2 AND method_id = ?3",
            params![serde_json::to_string(&receipt).unwrap_or(raw), key, method_id],
        );
    }
}

/// Records a definitive rejection so the same key never re-executes the
/// mutation (the client retries with a new key after reconciling — the same
/// contract rule `run.submit` follows via `record_rejection_in_tx`). Unlike
/// `store.rs::record_rejection_in_tx`, whose `INSERT OR IGNORE` runs before
/// any accepted row exists, this is called *after* `intake_operation` has
/// already inserted the accepted row — so the rejection is an upsert that
/// flips that row, and the stored receipt's `state` becomes the terminal
/// `failed` instead of the stale `running` a same-key replay would otherwise
/// observe forever (`git.commit` acceptance: "响应丢失通过operationId查询，
/// 不自动再建第二个提交").
fn record_operation_rejection(
    store: &crate::public_v1::store::V1Store,
    method_id: &str,
    key: Option<&str>,
    input: &Value,
    code: &str,
) {
    let Some(key) = key else {
        return;
    };
    let digest = compute_input_digest(method_id, input);
    let Ok(conn) = store.conn.lock() else {
        return;
    };
    // Flip the stored receipt to the terminal `failed` state (the receipt
    // schema's enum includes it), keeping operationId/acceptedRevision so the
    // row stays answerable for receipt lookups.
    let failed_receipt: Option<String> = conn
        .query_row(
            "SELECT receipt_json FROM idempotency_records WHERE idempotency_key = ?1 AND method_id = ?2",
            params![key, method_id],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .ok()
        .flatten()
        .and_then(|raw| {
            let mut receipt = serde_json::from_str::<Value>(&raw).ok()?;
            receipt
                .as_object_mut()?
                .insert("state".into(), json!("failed"));
            serde_json::to_string(&receipt).ok()
        });
    let _ = conn.execute(
        "INSERT INTO idempotency_records (idempotency_key, method_id, input_digest, request_state, reason, resource_json, receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
         VALUES (?1, ?2, ?3, 'notAccepted', 'definitivelyRejected', NULL, ?4, ?5, ?6, ?7, 'correctRejectedInputWithNewKey')
         ON CONFLICT(idempotency_key, method_id) DO UPDATE SET
           request_state = 'notAccepted',
           reason = 'definitivelyRejected',
           receipt_json = COALESCE(excluded.receipt_json, idempotency_records.receipt_json),
           rejection_code = excluded.rejection_code,
           retry_disposition = 'correctRejectedInputWithNewKey',
           observed_at = excluded.observed_at,
           retained_until = excluded.retained_until",
        params![
            key,
            method_id,
            digest,
            failed_receipt,
            code,
            now_rfc3339(),
            rfc3339_plus(RETENTION_SECS)
        ],
    );
}

/// Emits the contract-declared events (`git.changed`, `files.changed`) into
/// the shared `events` table, keyed by operationId for the
/// `operations.events` stream (the collections task owns the consumer side).
fn emit_operation_events(
    store: &crate::public_v1::store::V1Store,
    operation_id: &str,
    changed_paths: &[Value],
) {
    let Ok(conn) = store.conn.lock() else {
        return;
    };
    let now = now_rfc3339();
    for event_type in ["git.changed", "files.changed"] {
        if event_type == "files.changed" && changed_paths.is_empty() {
            continue;
        }
        let payload = json!({
            "operationId": operation_id,
            "type": event_type,
            "changedPaths": changed_paths,
        });
        let payload_str = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into());
        let placeholder = format!(
            "tmp-evt-{}",
            EVENT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        if conn
            .execute(
                "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at)
                 VALUES (?1, ?2, '', ?3, 'agentcpm-pd.operation.event/1', ?4, ?4, ?5)",
                params![placeholder, operation_id, event_type, payload_str, now],
            )
            .is_err()
        {
            continue;
        }
        let seq = conn.last_insert_rowid();
        let _ = conn.execute(
            "UPDATE events SET cursor = ?1 WHERE seq = ?2",
            params![format!("cur-{seq:012}"), seq],
        );
    }
}

// ---------------------------------------------------------------------------
// Mutation runner: repo lock + stateToken CAS + operation record lifecycle.
// ---------------------------------------------------------------------------

/// A mutation's terminal outcome: full success with a `completionResult`, or
/// a partial result (some effects applied) that still surfaces an error.
pub(super) enum MutationOutcome {
    Completed(Value),
    Partial(Value, ContractError),
}

/// Shared pipeline for the eleven async `git.*` mutations: resolve repo →
/// intake (idempotency) → per-repo write lock → stateToken re-verification →
/// execute → durable operation record + receipt. The execution itself runs
/// inside the dispatch (spawned off the connection by `ws.rs`), so a dropped
/// connection never cancels an accepted mutation.
type MutationFuture<'a> =
    Pin<Box<dyn Future<Output = Result<MutationOutcome, ContractError>> + Send + 'a>>;

async fn run_mutation(
    rt: Arc<PublicV1Runtime>,
    method_id: &'static str,
    input: Value,
    key: Option<String>,
    execute: impl for<'a> FnOnce(&'a RepoCtx, &'a RepoState, &'a Value, &'a str) -> MutationFuture<'a>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    require_git()?;
    let project_id = input
        .get("projectId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let repo = resolve_repo(&rt, &project_id).await?;

    let operation_id = rt.store().next_id();
    let intake = intake_operation(
        rt.store(),
        method_id,
        &project_id,
        &repo.repository_id,
        &input,
        key.as_deref(),
        &operation_id,
    )?;
    if intake.deduplicated {
        // The original execution still owns this mutation; replays never
        // re-execute ("不自动再建第二个提交").
        return Ok((ResponseOutcome::Accepted, intake.receipt));
    }

    let _repo_guard = lock_repo(&repo.repo_root).await;
    let before = observe_repo(&repo.repo_root).await?;
    let expected_token = input
        .get("expectedStateToken")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if expected_token != before.state_token() {
        let err =
            revision_conflict("expectedStateToken does not match the observed repository state");
        finish_operation(
            rt.store(),
            &intake.operation_id,
            false,
            None,
            Some((err.code.as_str(), &err.message)),
            true,
        );
        record_operation_rejection(rt.store(), method_id, key.as_deref(), &input, &err.code);
        return Err(err.into());
    }

    let outcome = execute(&repo, &before, &input, &intake.operation_id).await;
    match outcome {
        Ok(MutationOutcome::Completed(completion)) => {
            let changed_paths = completion
                .get("changedPaths")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            finish_operation(
                rt.store(),
                &intake.operation_id,
                true,
                Some(&completion),
                None,
                true,
            );
            emit_operation_events(rt.store(), &intake.operation_id, &changed_paths);
            update_receipt_state(rt.store(), method_id, key.as_deref(), "succeeded");
            let receipt = operation_receipt(
                &intake.operation_id,
                intake
                    .receipt
                    .get("acceptedRevision")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                "succeeded",
            );
            Ok((ResponseOutcome::Accepted, receipt))
        }
        Ok(MutationOutcome::Partial(completion, err)) => {
            let changed_paths = completion
                .get("changedPaths")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            finish_operation(
                rt.store(),
                &intake.operation_id,
                false,
                Some(&completion),
                Some((err.code.as_str(), &err.message)),
                true,
            );
            emit_operation_events(rt.store(), &intake.operation_id, &changed_paths);
            record_operation_rejection(rt.store(), method_id, key.as_deref(), &input, &err.code);
            Err(err.into())
        }
        Err(err) => {
            // A write killed by its deadline (`RESULT_UNKNOWN`) leaves the
            // effect undetermined in the operation record too.
            let effect_known = err.effect != EffectStatus::Unknown;
            finish_operation(
                rt.store(),
                &intake.operation_id,
                false,
                None,
                Some((err.code.as_str(), &err.message)),
                effect_known,
            );
            record_operation_rejection(rt.store(), method_id, key.as_deref(), &input, &err.code);
            Err(err.into())
        }
    }
}

/// Completion-result helper shared by the mutation handlers: before/after
/// tokens, HEAD, and the changed remote-tracking refs.
fn git_completion(
    repo: &RepoCtx,
    before: &RepoState,
    after: &RepoState,
    changed_refs: Vec<Value>,
    changed_paths: Vec<Value>,
    conflicts: Vec<Value>,
    remote_outcome: &str,
) -> Value {
    let mut refs: Vec<Value> = Vec::new();
    if before.head_oid != after.head_oid {
        refs.push(json!({
            "refName": before.head_ref.clone().unwrap_or_else(|| "HEAD".to_owned()),
            "beforeOid": before.head_oid.clone().unwrap_or_default(),
            "afterOid": after.head_oid.clone().unwrap_or_default(),
        }));
    }
    let before_map: std::collections::BTreeMap<&str, &str> = before
        .remote_refs
        .iter()
        .map(|(r, o)| (r.as_str(), o.as_str()))
        .collect();
    let after_map: std::collections::BTreeMap<&str, &str> = after
        .remote_refs
        .iter()
        .map(|(r, o)| (r.as_str(), o.as_str()))
        .collect();
    for (name, oid) in &before_map {
        match after_map.get(*name) {
            Some(new_oid) if new_oid != oid => {
                refs.push(json!({"refName": name, "beforeOid": oid, "afterOid": new_oid}));
            }
            None => {
                refs.push(json!({"refName": name, "beforeOid": oid, "afterOid": ""}));
            }
            _ => {}
        }
    }
    for (name, oid) in &after_map {
        if !before_map.contains_key(*name) {
            refs.push(json!({"refName": name, "beforeOid": "", "afterOid": oid}));
        }
    }
    refs.extend(changed_refs);
    let mut obj = Map::new();
    obj.insert("repositoryId".into(), json!(repo.repository_id));
    obj.insert("beforeStateToken".into(), json!(before.state_token()));
    obj.insert("afterStateToken".into(), json!(after.state_token()));
    if let Some(oid) = &after.head_oid {
        obj.insert("headOid".into(), json!(oid));
    }
    obj.insert("changedRefs".into(), Value::Array(refs));
    obj.insert("changedPaths".into(), Value::Array(changed_paths));
    obj.insert("conflicts".into(), Value::Array(conflicts));
    obj.insert("remoteOutcome".into(), json!(remote_outcome));
    Value::Object(obj)
}

// ---------------------------------------------------------------------------
// Registration.
// ---------------------------------------------------------------------------

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema(
        "git_operations",
        "CREATE TABLE IF NOT EXISTS git_operations (
            operation_id TEXT PRIMARY KEY,
            method_id TEXT NOT NULL,
            project_id TEXT NOT NULL,
            repository_id TEXT NOT NULL,
            state TEXT NOT NULL,
            revision TEXT NOT NULL,
            cancel_requested INTEGER NOT NULL DEFAULT 0,
            effect_status TEXT NOT NULL,
            progress_json TEXT NOT NULL DEFAULT '[]',
            result_json TEXT,
            error_code TEXT,
            error_message TEXT,
            input_json TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );",
    );
    reg.schema(
        "git_restore_plans",
        "CREATE TABLE IF NOT EXISTS git_restore_plans (
            plan_id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL,
            repository_id TEXT NOT NULL,
            action TEXT NOT NULL,
            paths_json TEXT NOT NULL,
            natures_json TEXT NOT NULL,
            expected_state_token TEXT NOT NULL,
            plan_revision TEXT NOT NULL,
            expires_at TEXT NOT NULL,
            created_at TEXT NOT NULL
        );",
    );
    reg.handler("git.status", |rt, input, _key, _rev| {
        Box::pin(git_status(rt, input))
    });
    reg.handler("git.inspect", |rt, input, _key, _rev| {
        Box::pin(git_inspect(rt, input))
    });
    reg.handler("git.commit", |rt, input, key, _rev| {
        Box::pin(async move { submit_commit(rt, input, key).await })
    });
    reg.handler("git.uncommit", |rt, input, key, _rev| {
        Box::pin(async move { submit_uncommit(rt, input, key).await })
    });
    reg.handler("git.create_branch", |rt, input, key, _rev| {
        Box::pin(async move { submit_create_branch(rt, input, key).await })
    });
    reg.handler("git.delete_branch", |rt, input, key, _rev| {
        Box::pin(async move { submit_delete_branch(rt, input, key).await })
    });
    reg.handler("git.checkout", |rt, input, key, _rev| {
        Box::pin(async move { submit_checkout(rt, input, key).await })
    });
    reg.handler("git.plan_restore", |rt, input, _key, _rev| {
        Box::pin(plan_restore(rt, input))
    });
    reg.handler("git.restore", |rt, input, key, _rev| {
        Box::pin(async move { submit_restore(rt, input, key).await })
    });
    reg.handler("git.fetch", |rt, input, key, _rev| {
        Box::pin(async move { submit_fetch(rt, input, key).await })
    });
    reg.handler("git.pull", |rt, input, key, _rev| {
        Box::pin(async move { submit_pull(rt, input, key).await })
    });
    reg.handler("git.push", |rt, input, key, _rev| {
        Box::pin(async move { submit_push(rt, input, key).await })
    });
    reg.handler("git.publish", |rt, input, key, _rev| {
        Box::pin(async move { submit_publish(rt, input, key).await })
    });
    reg.handler("git.suggest_message", |rt, input, key, _rev| {
        Box::pin(async move { submit_suggest_message(rt, input, key).await })
    });
}

// ---------------------------------------------------------------------------
// git.status
// ---------------------------------------------------------------------------

async fn git_status(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    require_git()?;
    let project_id = input["projectId"].as_str().unwrap_or_default();
    let repo = resolve_repo(&rt, project_id).await?;
    let state = observe_repo(&repo.repo_root).await?;
    let entries = parse_porcelain_z(&state.porcelain_z);
    let binary = detect_binary_paths(&repo.repo_root, &entries).await;
    let files: Vec<Value> = entries
        .iter()
        .map(|e| git_path_state_json(e, binary.contains(&e.path)))
        .collect();
    let mut git_state = Map::new();
    git_state.insert("projectId".into(), json!(project_id));
    git_state.insert("repositoryId".into(), json!(repo.repository_id));
    git_state.insert("stateToken".into(), json!(state.state_token()));
    if let Some(oid) = &state.head_oid {
        git_state.insert("headOid".into(), json!(oid));
    }
    if let Some(r) = &state.head_ref {
        git_state.insert("headRef".into(), json!(r));
    }
    git_state.insert("unborn".into(), json!(state.unborn));
    git_state.insert("detached".into(), json!(state.detached));
    git_state.insert("files".into(), Value::Array(files));
    git_state.insert(
        "remoteNames".into(),
        Value::Array(
            state
                .remote_names
                .iter()
                .map(|n| json!(n))
                .collect::<Vec<_>>(),
        ),
    );
    git_state.insert("observedAt".into(), json!(now_rfc3339()));
    Ok((
        ResponseOutcome::Succeeded,
        json!({"state": Value::Object(git_state)}),
    ))
}

/// Fresh porcelain observation (for handlers that re-verify inside the repo
/// write lock).
async fn porcelain_bytes(repo_root: &Path) -> Result<Vec<u8>, ContractError> {
    git_run(
        repo_root,
        &os_ref(&os(&["status", "--porcelain=v1", "-z"])),
        &[],
    )
    .await
    .map_err(|e| git_spawn_error(&e))
}

fn path_display(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

// ---------------------------------------------------------------------------
// git.inspect
// ---------------------------------------------------------------------------

fn parse_page(input: &Value) -> Result<(u64, u64), ContractError> {
    let (mut offset, mut limit) = (0u64, 100u64);
    if let Some(p) = input.get("page") {
        limit = p.get("limit").and_then(Value::as_u64).unwrap_or(100);
        if let Some(cursor) = p.get("cursor").and_then(Value::as_str)
            && !cursor.is_empty()
        {
            offset = cursor
                .strip_prefix("gp1-")
                .and_then(|raw| raw.parse::<u64>().ok())
                .ok_or_else(|| {
                    invalid_argument("unrecognized page cursor; restart from the first page")
                })?;
        }
    }
    if limit < 1 {
        return Err(invalid_argument("page.limit must be >= 1"));
    }
    if limit > MAX_PAGE_LIMIT {
        return Err(git_err(
            CODE_RESOURCE_LIMIT,
            format!("page.limit above the supported maximum of {MAX_PAGE_LIMIT}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    Ok((offset, limit))
}

fn page_slice(items: Vec<Value>, offset: u64, limit: u64) -> (Vec<Value>, Option<String>) {
    let total = items.len() as u64;
    if offset >= total {
        return (Vec::new(), None);
    }
    let end = (offset + limit).min(total) as usize;
    let page = items[offset as usize..end].to_vec();
    let next = if end as u64 >= total {
        None
    } else {
        Some(format!("gp1-{end}"))
    };
    (page, next)
}

async fn git_inspect(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    require_git()?;
    let project_id = input["projectId"].as_str().unwrap_or_default();
    let repo = resolve_repo(&rt, project_id).await?;
    let state = observe_repo(&repo.repo_root).await?;
    if let Some(expected) = input.get("expectedStateToken").and_then(Value::as_str)
        && !expected.is_empty()
        && expected != state.state_token()
    {
        return Err(revision_conflict(
            "expectedStateToken does not match the observed repository state",
        )
        .into());
    }
    let view = input["view"].as_str().unwrap_or_default();
    let (offset, limit) = parse_page(&input)?;
    let root = repo.repo_root.clone();
    let (schema_id, payload, next_cursor, fetched_at): (
        &str,
        Value,
        Option<String>,
        Option<String>,
    ) = match view {
        "diff" => inspect_diff(&repo, &input, &state, offset, limit).await?,
        "fileWithDiff" => inspect_file_with_diff(&repo, &input, &state, rt.store()).await?,
        "commits" => inspect_commits(&root, &state, offset, limit).await?,
        "commitDiff" => inspect_commit_diff(&root, &input, offset, limit).await?,
        "branches" => inspect_branches(&root, offset, limit).await?,
        "remoteStatus" => inspect_remote_status(&root, &state).await?,
        other => return Err(invalid_argument(format!("unknown inspect view `{other}`")).into()),
    };

    let mut payload = payload;
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("stateToken".into(), json!(state.state_token()));
    }
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    let artifact = store_artifact(
        rt.store(),
        project_id,
        &format!("{schema_id} {project_id}"),
        &bytes,
    )?;
    let mut result = Map::new();
    result.insert("stateToken".into(), json!(state.state_token()));
    result.insert("view".into(), json!(view));
    result.insert("dataArtifact".into(), artifact);
    result.insert("dataSchemaId".into(), json!(schema_id));
    if let Some(cursor) = next_cursor {
        result.insert("nextCursor".into(), json!(cursor));
    }
    if let Some(at) = fetched_at {
        result.insert("remoteDataFetchedAt".into(), json!(at));
    }
    result.insert("remoteReachability".into(), json!("notChecked"));
    Ok((ResponseOutcome::Succeeded, Value::Object(result)))
}

/// `view=diff`: whole-changeset diff vs HEAD (untracked files synthesized
/// as full additions — TS git.js:382-395 semantics), or a base..target
/// comparison, optionally narrowed to one path.
async fn inspect_diff(
    repo: &RepoCtx,
    input: &Value,
    state: &RepoState,
    offset: u64,
    limit: u64,
) -> Result<(&'static str, Value, Option<String>, Option<String>), ContractError> {
    let root = &repo.repo_root;
    let base = input.get("baseOid").and_then(Value::as_str);
    let target = input.get("targetOid").and_then(Value::as_str);
    let path_filter = match input.get("path") {
        Some(p) => Some(byte_path_bytes(p)?),
        None => None,
    };
    let files: Vec<Value> = match (base, target) {
        (Some(base), Some(target)) => {
            if !valid_commit_ref(base) || !valid_commit_ref(target) {
                return Err(invalid_ref_error());
            }
            parse_unified_diff(&run_diff(root, &[base, target], path_filter.as_deref()).await?)
        }
        (None, None) => {
            let mut files = if state.unborn {
                // Unborn HEAD: `git diff HEAD` cannot resolve ("fatal: bad
                // revision 'HEAD'", verified against git 2.54) and would be
                // misclassified as INVALID_REF. The TS baseline's /diff route
                // never names HEAD — it runs the unstaged `git diff -- <path>`
                // and falls back to the staged `git diff --cached -- <path>`
                // (git.js:409-424), both of which resolve on an unborn HEAD
                // (`--cached` diffs the index against the empty tree). The
                // whole-changeset form generalizes that per-path preference.
                let unstaged = run_diff(root, &[], path_filter.as_deref()).await?;
                let staged = run_diff(root, &["--cached"], path_filter.as_deref()).await?;
                merge_unborn_diffs(parse_unified_diff(&staged), parse_unified_diff(&unstaged))
            } else {
                let tracked = run_diff(root, &["HEAD"], path_filter.as_deref()).await?;
                parse_unified_diff(&tracked)
            };
            if path_filter.is_none() {
                for entry in parse_porcelain_z(&state.porcelain_z)
                    .into_iter()
                    .filter(|e| e.untracked())
                {
                    let abs = root.join(bytes_to_os(&entry.path));
                    let is_dir = abs.is_dir();
                    files.push(synthesize_untracked_diff(root, &entry.path, is_dir).await);
                }
            }
            files
        }
        _ => {
            return Err(invalid_argument(
                "baseOid and targetOid must be provided together",
            ));
        }
    };
    let (page, next) = page_slice(files, offset, limit);
    let payload = json!({
        "schemaId": "git.diff.v1",
        "schemaVersion": "1",
        "files": page,
        "complete": next.is_none(),
        "nextCursor": next,
    });
    Ok(("git.diff.v1", payload, next, None))
}

async fn run_diff(
    root: &Path,
    refs: &[&str],
    path: Option<&[u8]>,
) -> Result<Vec<u8>, ContractError> {
    // `refs` carries the diff selection before `--`: revisions (`HEAD`,
    // `<base> <target>`) or a mode flag (`--cached`) for the unborn form.
    let mut argv = os(&["-c", "core.quotePath=false", "diff", "--full-index"]);
    for r in refs {
        argv.push(std::ffi::OsString::from(*r));
    }
    argv.push("--".into());
    if let Some(p) = path {
        validate_repo_relative_path(p)?;
        argv.push(bytes_to_os(p));
    }
    match git_run(root, &os_ref(&argv), &[]).await {
        Ok(out) => Ok(out),
        Err(e) if e.is_missing_revision() => Err(invalid_ref_error()),
        Err(e) => Err(git_spawn_error(&e)),
    }
}

/// The per-path key of a parsed FileDiff (its new path, or the old path for
/// deletions), used to merge an unborn HEAD's staged and unstaged diffs.
fn diff_path_key(diff: &Value) -> Vec<u8> {
    diff.get("newPath")
        .filter(|p| !p.is_null())
        .or_else(|| diff.get("oldPath"))
        .filter(|p| !p.is_null())
        .and_then(|p| byte_path_bytes(p).ok())
        .unwrap_or_default()
}

/// Merges the staged (index vs the empty tree) and unstaged (worktree vs
/// index) diffs of an unborn-HEAD changeset, preferring the unstaged
/// FileDiff per path — the same preference the TS baseline's per-file /diff
/// route applies (git.js:409-424: unstaged first, staged only as fallback).
fn merge_unborn_diffs(staged: Vec<Value>, unstaged: Vec<Value>) -> Vec<Value> {
    let mut out = staged;
    let mut position: HashMap<Vec<u8>, usize> = out
        .iter()
        .enumerate()
        .map(|(i, diff)| (diff_path_key(diff), i))
        .collect();
    for diff in unstaged {
        let key = diff_path_key(&diff);
        match position.get(&key) {
            Some(&index) => out[index] = diff,
            None => {
                position.insert(key, out.len());
                out.push(diff);
            }
        }
    }
    out
}

/// The schema-legal empty diff for a path with no changes (a clean tree or a
/// path git does not know — the TS baseline returns an empty diff string for
/// both, git.js:421 `stripDiffHeaders(stagedDiff) || ''`). The
/// `git.fileWithDiff.v1` artifact schema requires `diff` to be a `FileDiff`
/// (`cli-v1-operations.json`), never `null`.
fn empty_file_diff(rel_path: &[u8]) -> Value {
    FileDiffAcc::new(Some(rel_path.to_vec()), Some(rel_path.to_vec())).finalize()
}

/// `view=fileWithDiff`: per-file diff (TS git.js:437-514) plus the current
/// content artifact, from one consistent observation.
async fn inspect_file_with_diff(
    repo: &RepoCtx,
    input: &Value,
    _state: &RepoState,
    store: &crate::public_v1::store::V1Store,
) -> Result<(&'static str, Value, Option<String>, Option<String>), ContractError> {
    let root = &repo.repo_root;
    let Some(path_value) = input.get("path") else {
        return Err(invalid_argument("view=fileWithDiff requires `path`"));
    };
    let path = byte_path_bytes(path_value)?;
    validate_repo_relative_path(&path)?;

    let status = git_run_bytes(root, &["status", "--porcelain", "--"], &path, &[]).await;
    let status_text = match status {
        Ok(out) => String::from_utf8_lossy(&out).into_owned(),
        Err(e) => return Err(git_spawn_error(&e)),
    };
    let is_untracked = status_text.starts_with("??");
    let is_deleted = status_text.starts_with("D ")
        || status_text.starts_with(" D")
        || status_text.starts_with("AD");

    let diff = if is_untracked {
        let abs = root.join(bytes_to_os(&path));
        let is_dir = abs.is_dir();
        synthesize_untracked_diff(root, &path, is_dir).await
    } else {
        let unstaged = run_diff(root, &[], Some(&path)).await?;
        if !unstaged.is_empty() {
            parse_unified_diff(&unstaged)
                .into_iter()
                .next()
                .unwrap_or_else(|| empty_file_diff(&path))
        } else {
            let staged = git_run_bytes(
                root,
                &[
                    "-c",
                    "core.quotePath=false",
                    "diff",
                    "--cached",
                    "--full-index",
                    "--",
                ],
                &path,
                &[],
            )
            .await
            .map_err(|e| git_spawn_error(&e))?;
            parse_unified_diff(&staged)
                .into_iter()
                .next()
                .unwrap_or_else(|| empty_file_diff(&path))
        }
    };

    // Current-content artifact (HEAD content for deleted files) so the file
    // and its diff come from one consistent observation.
    let (content, file_revision): (Option<Vec<u8>>, Option<String>) = if is_deleted {
        let mut refspec = b"HEAD:".to_vec();
        refspec.extend_from_slice(&path);
        let content = git_run_bytes(root, &["show"], &refspec, &[]).await.ok();
        let rev = git_run_bytes(root, &["rev-parse"], &refspec, &[])
            .await
            .ok()
            .map(|o| String::from_utf8_lossy(&o).trim_end().to_owned());
        (content, rev)
    } else {
        let abs = root.join(bytes_to_os(&path));
        let content = tokio::fs::read(&abs).await.ok();
        let rev = git_run_bytes(root, &["hash-object", "--"], &path, &[])
            .await
            .ok()
            .map(|o| String::from_utf8_lossy(&o).trim_end().to_owned());
        (content, rev)
    };
    let file_artifact_id = match &content {
        Some(bytes) => {
            let descriptor = store_artifact(store, &repo.project_id, &path_display(&path), bytes)?;
            descriptor
                .get("artifactId")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        }
        None => None,
    };
    let payload = json!({
        "schemaId": "git.fileWithDiff.v1",
        "schemaVersion": "1",
        "path": byte_path_json(&path),
        "fileArtifactId": file_artifact_id,
        "fileRevision": file_revision,
        "diff": diff,
    });
    Ok(("git.fileWithDiff.v1", payload, None, None))
}

/// `view=commits` — NUL/RS-separated log records (the TS `|` format loses
/// messages containing `|`, a baseline bug this port fixes structurally).
async fn inspect_commits(
    root: &Path,
    state: &RepoState,
    offset: u64,
    limit: u64,
) -> Result<(&'static str, Value, Option<String>, Option<String>), ContractError> {
    if state.unborn {
        let payload = json!({
            "schemaId": "git.commits.v1",
            "schemaVersion": "1",
            "commits": [],
            "complete": true,
            "nextCursor": null,
        });
        return Ok(("git.commits.v1", payload, None, None));
    }
    let fetch = limit + 1;
    let n_arg = format!("-n{fetch}");
    let skip_arg = format!("--skip={offset}");
    let out = git_run_str(
        root,
        &[
            "log",
            "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%aI%x1f%cn%x1f%ce%x1f%cI%x1f%s%x1f%b%x1e",
            n_arg.as_str(),
            skip_arg.as_str(),
            "HEAD",
        ],
    )
    .await
    .map_err(|e| git_spawn_error(&e))?;
    let text = String::from_utf8_lossy(&out).into_owned();
    let records: Vec<&str> = text
        .split('\u{1e}')
        .filter(|r| !r.trim().is_empty())
        .collect();
    let mut commits: Vec<Value> = Vec::new();
    for record in records.iter().take(limit as usize) {
        let record = record.trim_start_matches('\n');
        let fields: Vec<&str> = record.split('\u{1f}').collect();
        if fields.len() < 10 {
            continue;
        }
        let [oid, parents, an, ae, aat, cn, ce, cat, subject, body] = fields[..] else {
            continue;
        };
        commits.push(json!({
            "oid": oid,
            "parentOids": if parents.is_empty() {
                json!([])
            } else {
                json!(parents.split_whitespace().collect::<Vec<_>>())
            },
            "author": {"name": an, "email": ae, "timestamp": aat},
            "committer": {"name": cn, "email": ce, "timestamp": cat},
            "subject": subject,
            "message": if body.trim().is_empty() {
                json!(subject)
            } else {
                json!(format!("{subject}\n\n{}", body.trim_end_matches('\n')))
            },
            "signatureStatus": "notChecked",
        }));
    }
    let next = if records.len() as u64 > limit {
        Some(format!(
            "gp1-{}",
            offset + records.len().min(limit as usize) as u64
        ))
    } else {
        None
    };
    let payload = json!({
        "schemaId": "git.commits.v1",
        "schemaVersion": "1",
        "commits": commits,
        "complete": next.is_none(),
        "nextCursor": next,
    });
    Ok(("git.commits.v1", payload, next, None))
}

/// `view=commitDiff` — commit metadata plus the patch against its first
/// parent (no 500k truncation: the artifact carries everything, files page).
async fn inspect_commit_diff(
    root: &Path,
    input: &Value,
    offset: u64,
    limit: u64,
) -> Result<(&'static str, Value, Option<String>, Option<String>), ContractError> {
    let Some(target) = input.get("targetOid").and_then(Value::as_str) else {
        return Err(invalid_argument("view=commitDiff requires `targetOid`"));
    };
    if !valid_commit_ref(target) {
        return Err(invalid_ref_error());
    }
    let Some(commit) = log_one_commit(root, target).await? else {
        return Err(invalid_ref_error());
    };
    let compared_parent = commit
        .get("parentOids")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let mut argv = os(&[
        "-c",
        "core.quotePath=false",
        "diff-tree",
        "--full-index",
        "-r",
        "-c",
        "-p",
        "--root",
        "--no-commit-id",
    ]);
    argv.push(std::ffi::OsString::from(target));
    let raw = match git_run(root, &os_ref(&argv), &[]).await {
        Ok(out) => out,
        Err(e) if e.is_missing_revision() => return Err(invalid_ref_error()),
        Err(e) => return Err(git_spawn_error(&e)),
    };
    let files = parse_unified_diff(&raw);
    let (page, next) = page_slice(files, offset, limit);
    let payload = json!({
        "schemaId": "git.commitDiff.v1",
        "schemaVersion": "1",
        "commit": commit,
        "comparedParentOid": compared_parent,
        "files": page,
        "complete": next.is_none(),
        "nextCursor": next,
    });
    Ok(("git.commitDiff.v1", payload, next, None))
}

async fn log_one_commit(root: &Path, target: &str) -> Result<Option<Value>, ContractError> {
    let out = match git_run_str(
        root,
        &[
            "log",
            "-n",
            "1",
            "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%aI%x1f%cn%x1f%ce%x1f%cI%x1f%s%x1f%b%x1e",
            target,
        ],
    )
    .await
    {
        Ok(out) => out,
        Err(e) if e.is_missing_revision() => return Ok(None),
        Err(e) => return Err(git_spawn_error(&e)),
    };
    let text = String::from_utf8_lossy(&out).into_owned();
    let Some(record) = text.split('\u{1e}').find(|r| !r.trim().is_empty()) else {
        return Ok(None);
    };
    let fields: Vec<&str> = record.trim_start_matches('\n').split('\u{1f}').collect();
    if fields.len() < 10 {
        return Ok(None);
    }
    let [oid, parents, an, ae, aat, cn, ce, cat, subject, body] = fields[..] else {
        return Ok(None);
    };
    Ok(Some(json!({
        "oid": oid,
        "parentOids": if parents.is_empty() {
            json!([])
        } else {
            json!(parents.split_whitespace().collect::<Vec<_>>())
        },
        "author": {"name": an, "email": ae, "timestamp": aat},
        "committer": {"name": cn, "email": ce, "timestamp": cat},
        "subject": subject,
        "message": if body.trim().is_empty() {
            json!(subject)
        } else {
            json!(format!("{subject}\n\n{}", body.trim_end_matches('\n')))
        },
        "signatureStatus": "notChecked",
    })))
}

/// `view=branches` — local + remote branches with ahead/behind where an
/// upstream exists (TS git.js:639-680, structured).
async fn inspect_branches(
    root: &Path,
    offset: u64,
    limit: u64,
) -> Result<(&'static str, Value, Option<String>, Option<String>), ContractError> {
    let out = git_run(
        root,
        &os_ref(&os(&[
            "for-each-ref",
            "refs/heads",
            "refs/remotes",
            "--format=%(refname)%09%(objectname)%09%(upstream)%09%(HEAD)",
        ])),
        &[],
    )
    .await
    .map_err(|e| git_spawn_error(&e))?;
    let text = String::from_utf8_lossy(&out).into_owned();
    let mut branches: Vec<Value> = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 4 {
            continue;
        }
        let (refname, oid, upstream, head_mark) = (parts[0], parts[1], parts[2], parts[3]);
        let remote = refname.starts_with("refs/remotes/");
        let name = refname
            .strip_prefix("refs/heads/")
            .or_else(|| refname.strip_prefix("refs/remotes/"))
            .unwrap_or(refname);
        let upstream_ref = if upstream.is_empty() {
            None
        } else {
            Some(upstream)
        };
        let mut ahead = Value::Null;
        let mut behind = Value::Null;
        if let (false, Some(up)) = (remote, upstream_ref) {
            let range = format!("{up}...{refname}");
            if let Ok(counts) =
                git_run_str(root, &["rev-list", "--count", "--left-right", &range]).await
            {
                let counts = String::from_utf8_lossy(&counts).into_owned();
                let mut parts = counts.split_whitespace();
                if let (Some(b), Some(a)) = (parts.next(), parts.next()) {
                    behind = json!(b.parse::<u64>().unwrap_or(0));
                    ahead = json!(a.parse::<u64>().unwrap_or(0));
                }
            }
        }
        branches.push(json!({
            "name": name,
            "oid": oid,
            "remote": remote,
            "current": head_mark == "*",
            "upstreamRef": upstream_ref,
            "ahead": ahead,
            "behind": behind,
        }));
    }
    let (page, next) = page_slice(branches, offset, limit);
    let payload = json!({
        "schemaId": "git.branches.v1",
        "schemaVersion": "1",
        "branches": page,
        "complete": next.is_none(),
        "nextCursor": next,
    });
    Ok(("git.branches.v1", payload, next, None))
}

/// `view=remoteStatus` — purely local (upstream + rev-list counts, never a
/// network fetch: git.js:1043-1118; contract "本操作不偷偷fetch").
async fn inspect_remote_status(
    root: &Path,
    state: &RepoState,
) -> Result<(&'static str, Value, Option<String>, Option<String>), ContractError> {
    let fallback_remote = if state.remote_names.is_empty() {
        None
    } else if state.remote_names.iter().any(|r| r == "origin") {
        Some("origin".to_owned())
    } else {
        state.remote_names.first().cloned()
    };
    let mut upstream_ref: Option<String> = None;
    if !state.unborn
        && !state.detached
        && let Ok(out) =
            git_run_str(root, &["rev-parse", "--symbolic-full-name", "@{upstream}"]).await
    {
        let text = String::from_utf8_lossy(&out).trim_end().to_owned();
        if !text.is_empty() {
            upstream_ref = Some(text);
        }
    }
    let (ahead, behind, known_remote_oid) = if let Some(up) = &upstream_ref {
        let known = state.remote_ref_oid(up).map(ToOwned::to_owned);
        let range = format!("{up}...HEAD");
        match git_run_str(root, &["rev-list", "--count", "--left-right", &range]).await {
            Ok(counts) => {
                let text = String::from_utf8_lossy(&counts).into_owned();
                let mut parts = text.split_whitespace();
                let behind = parts.next().and_then(|v| v.parse::<u64>().ok());
                let ahead = parts.next().and_then(|v| v.parse::<u64>().ok());
                (ahead.or(Some(0)), behind.or(Some(0)), known)
            }
            Err(_) => (None, None, known),
        }
    } else {
        (None, None, None)
    };
    let known_at = fetch_head_mtime(root).await;
    let remote_name = upstream_ref
        .as_deref()
        .and_then(|r| r.strip_prefix("refs/remotes/"))
        .and_then(|r| r.split('/').next())
        .map(ToOwned::to_owned)
        .or(fallback_remote);
    let local_ref = if state.unborn || state.detached {
        None
    } else {
        state.head_ref.clone()
    };
    let payload = json!({
        "schemaId": "git.remoteStatus.v1",
        "schemaVersion": "1",
        "remoteName": remote_name,
        "localRef": local_ref,
        "localOid": state.head_oid,
        "upstreamRef": upstream_ref,
        "knownRemoteOid": known_remote_oid,
        "ahead": ahead,
        "behind": behind,
        "knownAt": known_at,
        "reachability": "notChecked",
        "unborn": state.unborn,
        "detached": state.detached,
    });
    Ok(("git.remoteStatus.v1", payload, None, known_at))
}

async fn fetch_head_mtime(root: &Path) -> Option<String> {
    let out = git_run_str(root, &["rev-parse", "--git-path", "FETCH_HEAD"])
        .await
        .ok()?;
    let rel = String::from_utf8_lossy(&out).trim_end().to_owned();
    let path = root.join(rel);
    let meta = tokio::fs::metadata(&path).await.ok()?;
    let dt: chrono::DateTime<chrono::Utc> = meta.modified().ok()?.into();
    Some(dt.to_rfc3339())
}

// ---------------------------------------------------------------------------
// Identity (struct shared with git_identity.rs).
// ---------------------------------------------------------------------------

pub(super) struct EffectiveIdentity {
    pub(super) name: String,
    pub(super) email: String,
}

// ---------------------------------------------------------------------------
// git.commit / uncommit.
// ---------------------------------------------------------------------------

/// Resolves the selection paths to repo-relative byte vectors (mode=paths).
fn selection_paths(selection: &Value) -> Result<Vec<Vec<u8>>, ContractError> {
    let Some(paths) = selection.get("paths").and_then(Value::as_array) else {
        return Err(invalid_argument("selection.mode=paths requires paths"));
    };
    let mut out = Vec::with_capacity(paths.len());
    for p in paths {
        let bytes = byte_path_bytes(p)?;
        validate_repo_relative_path(&bytes)?;
        out.push(bytes);
    }
    Ok(out)
}

async fn staged_paths(root: &Path) -> Result<Vec<Vec<u8>>, ContractError> {
    let out = git_run(
        root,
        &os_ref(&os(&["diff", "--cached", "--name-only", "-z"])),
        &[],
    )
    .await
    .map_err(|e| git_spawn_error(&e))?;
    Ok(out
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| p.to_vec())
        .collect())
}

async fn submit_commit(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let rt_for_identity = rt.clone();
    run_mutation(rt, "git.commit", input, key, |repo, before, input, _op| {
        Box::pin(async move {
        let root = repo.repo_root.clone();
        let message = input.get("message").and_then(Value::as_str).unwrap_or_default();
        if message.trim().is_empty() {
            return Err(invalid_argument("commit message must not be empty"));
        }
        let initial = input.get("initial").and_then(Value::as_bool).unwrap_or(false);
        // "初始提交仅无HEAD时允许" — error when the initial flag and the
        // observed HEAD disagree.
        if initial != before.unborn {
            return Err(git_err(
                CODE_GIT_CONFLICT,
                if initial {
                    "repository already has commits; initial commit not allowed"
                } else {
                    "repository has no commits; pass initial=true for the first commit"
                },
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        let identity = super::git_identity::effective_identity_for_commit(&rt_for_identity, repo)
            .await
            .ok_or_else(|| {
                git_err(
                    CODE_IDENTITY_REQUIRED,
                    "no git identity configured; call git identity set first",
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )
            })?;

        let selection = input.get("selection").cloned().unwrap_or(Value::Null);
        let mode = selection.get("mode").and_then(Value::as_str).unwrap_or_default();
        match mode {
            "index" => {}
            "paths" => {
                let selected = selection_paths(&selection)?;
                let staged = staged_paths(&root).await?;
                let selected_set: HashSet<Vec<u8>> = selected.iter().cloned().collect();
                let outside: Vec<Vec<u8>> =
                    staged.into_iter().filter(|p| !selected_set.contains(p)).collect();
                if !outside.is_empty() {
                    let mut details = Map::new();
                    details.insert(
                        "stagedPaths".into(),
                        json!(outside.iter().map(|p| byte_path_json(p)).collect::<Vec<_>>()),
                    );
                    return Err(git_err(
                        CODE_INDEX_SCOPE_CONFLICT,
                        "selection.mode=paths would expand an existing index with different staged paths; commit those via selection.mode=index",
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    )
                    .with_details(details));
                }
                for path in &selected {
                    git_run_bytes(&root, &["add", "--"], path, &[])
                        .await
                        .map_err(|e| git_spawn_error(&e))?;
                }
            }
            "all" => {
                git_run(
                    &root,
                    &os_ref(&os(&["add", "-A", "--", "."])),
                    &[],
                )
                .await
                .map_err(|e| git_spawn_error(&e))?;
            }
            other => {
                return Err(invalid_argument(format!(
                    "unknown selection mode `{other}`"
                )))
            }
        }

        let name_arg = format!("user.name={}", identity.name);
        let email_arg = format!("user.email={}", identity.email);
        let commit = git_run_str(
            &root,
            &["-c", &name_arg, "-c", &email_arg, "commit", "-m", message],
        )
        .await;
        if let Err(e) = &commit {
            let text = e.combined_lower();
            if text.contains("nothing to commit")
                || text.contains("no changes added to commit")
                || text.contains("nothing added to commit")
            {
                return Err(git_err(
                    CODE_NOTHING_TO_COMMIT,
                    "nothing to commit",
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
        }
        commit.map_err(|e| git_spawn_error(&e))?;

        let after = observe_repo(&root).await?;
        let changed_paths = commit_changed_paths(&root, &after).await?;
        let completion = git_completion(
            repo,
            before,
            &after,
            Vec::new(),
            changed_paths,
            Vec::new(),
            "notRequested",
        );
        Ok(MutationOutcome::Completed(completion))
        })
    })
    .await
}

async fn commit_changed_paths(root: &Path, after: &RepoState) -> Result<Vec<Value>, ContractError> {
    let Some(head) = &after.head_oid else {
        return Ok(Vec::new());
    };
    let head_arg = head.as_str();
    let parts: Vec<&str> = if head_has_parent(root, head).await {
        vec!["diff-tree", "--no-commit-id", "--name-only", "-r", head_arg]
    } else {
        vec![
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            "--root",
            head_arg,
        ]
    };
    let out = git_run_str(root, &parts)
        .await
        .map_err(|e| git_spawn_error(&e))?;
    Ok(out
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(byte_path_json)
        .collect())
}

async fn head_has_parent(root: &Path, head: &str) -> bool {
    git_run_str(root, &["rev-list", "--parents", "-n", "1", head])
        .await
        .ok()
        .map(|out| {
            let text = String::from_utf8_lossy(&out).into_owned();
            text.split_whitespace().count() > 1
        })
        .unwrap_or(false)
}

async fn submit_uncommit(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    run_mutation(rt, "git.uncommit", input, key, |repo, before, input, _op| {
        Box::pin(async move {
        let root = repo.repo_root.clone();
        let expected_head = input
            .get("expectedHeadOid")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if before.head_oid.as_deref() != Some(expected_head) {
            return Err(revision_conflict(
                "expectedHeadOid does not match the observed HEAD",
            ));
        }
        let allow_rewrite = input
            .get("allowPublishedHistoryRewrite")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !allow_rewrite {
            let upstream = git_run_str(&root, &["rev-parse", "--symbolic-full-name", "@{upstream}"])
                .await
                .ok()
                .map(|o| String::from_utf8_lossy(&o).trim_end().to_owned())
                .filter(|s| !s.is_empty());
            if let Some(up) = upstream
                && git_run_str(&root, &["merge-base", "--is-ancestor", "HEAD", &up])
                    .await
                    .is_ok()
            {
                return Err(git_err(
                    CODE_GIT_CONFLICT,
                    "HEAD is already published on the upstream branch; pass allowPublishedHistoryRewrite=true to rewrite it",
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
        }
        let initial = !head_has_parent(&root, expected_head).await;
        if initial {
            // The initial commit has no parent: deleting HEAD keeps every
            // change staged (git.js:612-626).
            git_run_str(&root, &["update-ref", "-d", "HEAD"])
                .await
                .map_err(|e| git_spawn_error(&e))?;
        } else {
            git_run_str(&root, &["reset", "--soft", "HEAD~1"])
                .await
                .map_err(|e| git_spawn_error(&e))?;
        }
        let after = observe_repo(&root).await?;
        let completion = git_completion(
            repo,
            before,
            &after,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            "notRequested",
        );
        Ok(MutationOutcome::Completed(completion))
        })
    })
    .await
}

// ---------------------------------------------------------------------------
// Branch operations.
// ---------------------------------------------------------------------------

async fn submit_create_branch(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    run_mutation(
        rt,
        "git.create_branch",
        input,
        key,
        |repo, before, input, _op| {
        Box::pin(async move {
            let root = repo.repo_root.clone();
            let branch = input
                .get("branchName")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !valid_branch_name(branch) {
                return Err(invalid_argument(format!("invalid branch name `{branch}`")));
            }
            let start = input
                .get("startOid")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !valid_commit_ref(start) {
                return Err(invalid_ref_error());
            }
            let verify_arg = format!("{start}^{{commit}}");
            match git_run_str(&root, &["rev-parse", "--verify", &verify_arg]).await {
                Ok(_) => {}
                Err(e) if e.is_missing_revision() => return Err(invalid_ref_error()),
                Err(e) => return Err(git_spawn_error(&e)),
            }
            let branch_ref = format!("refs/heads/{branch}");
            if git_run_bytes(
                &root,
                &["rev-parse", "--verify", "--quiet"],
                branch_ref.as_bytes(),
                &[],
            )
            .await
            .is_ok()
            {
                return Err(git_err(
                    CODE_ALREADY_EXISTS,
                    format!("branch `{branch}` already exists"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            git_run_str(&root, &["branch", branch, start])
                .await
                .map_err(|e| git_spawn_error(&e))?;
            let checkout = input
                .get("checkout")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if checkout {
                match git_run_str(&root, &["checkout", branch]).await {
                    Ok(_) => {}
                    Err(e) if e.is_dirty_worktree() => {
                        return Err(git_err(
                            CODE_DIRTY_WORKTREE,
                            "working tree has local changes that would be overwritten by checkout",
                            RetrySpec::never(),
                            EffectStatus::NotApplied,
                        ));
                    }
                    Err(e) => return Err(git_spawn_error(&e)),
                }
            }
            let after = observe_repo(&root).await?;
            let start_oid = String::from_utf8_lossy(
                &git_run_str(&root, &["rev-parse", &branch_ref])
                    .await
                    .map_err(|e| git_spawn_error(&e))?,
            )
            .trim_end()
            .to_owned();
            let changed_refs = vec![json!({
                "refName": branch_ref,
                "beforeOid": "",
                "afterOid": start_oid,
            })];
            let completion = git_completion(
                repo,
                before,
                &after,
                changed_refs,
                Vec::new(),
                Vec::new(),
                "notRequested",
            );
            Ok(MutationOutcome::Completed(completion))
            })
        },
    )
    .await
}

async fn submit_delete_branch(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    run_mutation(
        rt,
        "git.delete_branch",
        input,
        key,
        |repo, before, input, _op| {
        Box::pin(async move {
            let root = repo.repo_root.clone();
            let branch = input.get("branchName").and_then(Value::as_str).unwrap_or_default();
            if !valid_branch_name(branch) {
                return Err(invalid_argument(format!("invalid branch name `{branch}`")));
            }
            let expected_oid = input
                .get("expectedBranchOid")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let current = git_run_str(&root, &["branch", "--show-current"])
                .await
                .map(|o| String::from_utf8_lossy(&o).trim_end().to_owned())
                .map_err(|e| git_spawn_error(&e))?;
            if current == branch {
                return Err(git_err(
                    CODE_BRANCH_CHECKED_OUT,
                    format!("branch `{branch}` is the currently checked-out branch"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            let branch_ref = format!("refs/heads/{branch}");
            let oid = match git_run_bytes(
                &root,
                &["rev-parse", "--verify", "--quiet"],
                branch_ref.as_bytes(),
                &[],
            )
            .await
            {
                Ok(out) => String::from_utf8_lossy(&out).trim_end().to_owned(),
                Err(_) => {
                    return Err(git_err(
                        CODE_NOT_FOUND,
                        format!("branch `{branch}` not found"),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ))
                }
            };
            if oid != expected_oid {
                return Err(revision_conflict(
                    "expectedBranchOid does not match the observed branch tip",
                ));
            }
            let allow_unmerged = input
                .get("allowUnmerged")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let flag = if allow_unmerged { "-D" } else { "-d" };
            if let Err(e) = git_run_str(&root, &["branch", flag, branch]).await {
                let text = e.combined_lower();
                if text.contains("not fully merged") {
                    return Err(git_err(
                        CODE_UNMERGED_BRANCH,
                        format!(
                            "branch `{branch}` is not fully merged; pass allowUnmerged=true to delete it"
                        ),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
                return Err(git_spawn_error(&e));
            }
            let after = observe_repo(&root).await?;
            let changed_refs = vec![json!({
                "refName": branch_ref,
                "beforeOid": oid,
                "afterOid": "",
            })];
            let completion = git_completion(
                repo,
                before,
                &after,
                changed_refs,
                Vec::new(),
                Vec::new(),
                "notRequested",
            );
            Ok(MutationOutcome::Completed(completion))
            })
        },
    )
    .await
}

async fn submit_checkout(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    run_mutation(
        rt,
        "git.checkout",
        input,
        key,
        |repo, before, input, _op| {
            Box::pin(async move {
                let root = repo.repo_root.clone();
                let target = input
                    .get("targetRef")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !valid_commit_ref(target) {
                    return Err(invalid_ref_error());
                }
                let detach = input
                    .get("detach")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let argv: Vec<&str> = if detach {
                    vec!["checkout", "--detach", target]
                } else {
                    vec!["checkout", target]
                };
                match git_run_str(&root, &argv).await {
                    Ok(_) => {}
                    Err(e) if e.is_dirty_worktree() => {
                        return Err(git_err(
                            CODE_DIRTY_WORKTREE,
                            "working tree has local changes that would be overwritten by checkout",
                            RetrySpec::never(),
                            EffectStatus::NotApplied,
                        ));
                    }
                    Err(e) if e.is_missing_revision() => {
                        return Err(invalid_ref_error());
                    }
                    Err(e) => return Err(git_spawn_error(&e)),
                }
                let after = observe_repo(&root).await?;
                let changed_paths = checkout_changed_paths(&root, before, &after).await;
                let completion = git_completion(
                    repo,
                    before,
                    &after,
                    Vec::new(),
                    changed_paths,
                    Vec::new(),
                    "notRequested",
                );
                Ok(MutationOutcome::Completed(completion))
            })
        },
    )
    .await
}

async fn checkout_changed_paths(root: &Path, before: &RepoState, after: &RepoState) -> Vec<Value> {
    match (&before.head_oid, &after.head_oid) {
        (Some(a), Some(b)) if a != b => {
            let range = format!("{a}..{b}");
            git_run_str(root, &["diff", "--name-only", "-z", &range])
                .await
                .map(|out| {
                    out.split(|&x| x == 0)
                        .filter(|p| !p.is_empty())
                        .map(byte_path_json)
                        .collect()
                })
                .unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// plan_restore / restore.
// ---------------------------------------------------------------------------

#[allow(dead_code)] // plan bookkeeping columns retained for the two-phase record
struct RestorePlanRow {
    plan_id: String,
    repository_id: String,
    action: String,
    paths_json: String,
    natures_json: String,
    expected_state_token: String,
    plan_revision: String,
    expires_at: String,
}

async fn plan_restore(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    require_git()?;
    let project_id = input["projectId"].as_str().unwrap_or_default();
    let action = input["action"].as_str().unwrap_or_default();
    if !matches!(action, "discardTracked" | "deleteUntracked") {
        return Err(invalid_argument(format!("unknown restore action `{action}`")).into());
    }
    let repo = resolve_repo(&rt, project_id).await?;
    let state = observe_repo(&repo.repo_root).await?;
    let expected = input
        .get("expectedStateToken")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if expected != state.state_token() {
        return Err(revision_conflict(
            "expectedStateToken does not match the observed repository state",
        )
        .into());
    }
    let Some(paths) = input.get("paths").and_then(Value::as_array) else {
        return Err(invalid_argument("paths is required").into());
    };
    let entries = parse_porcelain_z(&state.porcelain_z);
    let mut affected: Vec<String> = Vec::new();
    let mut conflicts: Vec<Value> = Vec::new();
    let mut natures: Vec<bool> = Vec::new(); // true = untracked
    let mut path_bytes: Vec<Vec<u8>> = Vec::new();
    for p in paths {
        let bytes = byte_path_bytes(p)?;
        validate_repo_relative_path(&bytes)?;
        let display = path_display(&bytes);
        let entry = entries.iter().find(|e| e.path == bytes);
        let (untracked, present) = match entry {
            Some(e) => (e.untracked(), true),
            None => (false, false),
        };
        match action {
            "discardTracked" => {
                if !present {
                    conflicts.push(json!({"resourceId": display, "code": "PATH_NOT_MODIFIED"}));
                } else if untracked {
                    conflicts.push(json!({"resourceId": display, "code": "PATH_UNTRACKED"}));
                } else {
                    affected.push(display);
                    natures.push(false);
                    path_bytes.push(bytes);
                }
            }
            "deleteUntracked" => {
                if !present || !untracked {
                    conflicts.push(json!({"resourceId": display, "code": "NOT_UNTRACKED"}));
                } else {
                    affected.push(display);
                    natures.push(true);
                    path_bytes.push(bytes);
                }
            }
            _ => unreachable!("action validated above"),
        }
    }

    let plan_id = format!("gplan-{}", rt.store().next_id());
    let expires = chrono::Utc::now() + chrono::TimeDelta::seconds(PLAN_TTL_SECS);
    let plan_revision = {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        let rev = next_domain_rev(&conn)?;
        conn.execute(
            "INSERT INTO git_restore_plans (plan_id, project_id, repository_id, action, paths_json, natures_json, expected_state_token, plan_revision, expires_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                plan_id,
                project_id,
                repo.repository_id,
                action,
                serde_json::to_string(
                    &path_bytes.iter().map(|p| byte_path_json(p)).collect::<Vec<_>>()
                )
                .unwrap(),
                serde_json::to_string(&natures).unwrap(),
                expected,
                rev,
                expires.to_rfc3339(),
                now_rfc3339(),
            ],
        )
        .map_err(map_sql_err)?;
        rev
    };
    let plan = json!({
        "planId": plan_id,
        "planRevision": plan_revision,
        "expiresAt": expires.to_rfc3339(),
        "affectedResources": affected,
        "conflicts": conflicts,
        "destructive": true,
    });
    Ok((
        ResponseOutcome::Succeeded,
        json!({"plan": plan, "stateToken": state.state_token()}),
    ))
}

async fn submit_restore(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let rt_for_store = rt.clone();
    run_mutation(rt, "git.restore", input, key, |repo, before, input, _op| {
        Box::pin(async move {
        let root = repo.repo_root.clone();
        let plan_id = input.get("planId").and_then(Value::as_str).unwrap_or_default();
        let expected_plan_rev = input
            .get("expectedPlanRevision")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let row: Option<RestorePlanRow> = {
            let conn = rt_for_store.store().conn.lock().expect("sqlite mutex");
            conn.query_row(
                "SELECT plan_id, repository_id, action, paths_json, natures_json, expected_state_token, plan_revision, expires_at
                 FROM git_restore_plans WHERE plan_id = ?1",
                params![plan_id],
                |r| {
                    Ok(RestorePlanRow {
                        plan_id: r.get(0)?,
                        repository_id: r.get(1)?,
                        action: r.get(2)?,
                        paths_json: r.get(3)?,
                        natures_json: r.get(4)?,
                        expected_state_token: r.get(5)?,
                        plan_revision: r.get(6)?,
                        expires_at: r.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(map_sql_err)?
        };
        let Some(row) = row else {
            return Err(git_err(
                CODE_NOT_FOUND,
                format!("restore plan `{plan_id}` not found"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        };
        if row.plan_revision != expected_plan_rev {
            return Err(revision_conflict(
                "expectedPlanRevision does not match the stored plan",
            ));
        }
        let expires: chrono::DateTime<chrono::Utc> = row
            .expires_at
            .parse()
            .map_err(|e| invalid_argument(format!("stored plan expiry: {e}")))?;
        if chrono::Utc::now() > expires {
            return Err(git_err(
                CODE_PLAN_EXPIRED,
                "restore plan expired; call git plan-restore again",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        // Per-path nature re-verification (contract: "执行前重验状态和每个
        // 路径；tracked/untracked性质变化使计划失效").
        let paths: Vec<Value> = serde_json::from_str(&row.paths_json).unwrap_or_default();
        let natures: Vec<bool> = serde_json::from_str(&row.natures_json).unwrap_or_default();
        let entries = parse_porcelain_z(&porcelain_bytes(&root).await?);

        let mut changed: Vec<Value> = Vec::new();
        let mut failed: Vec<Value> = Vec::new();
        for (idx, path_value) in paths.iter().enumerate() {
            let bytes = byte_path_bytes(path_value)?;
            let planned_untracked = natures.get(idx).copied().unwrap_or(false);
            let still_untracked = entries.iter().any(|e| e.path == bytes && e.untracked());
            if still_untracked != planned_untracked {
                failed.push(byte_path_json(&bytes));
                continue;
            }
            let result = if row.action == "deleteUntracked" {
                delete_untracked_path(&root, &bytes).await
            } else {
                discard_tracked_path(&root, &bytes, planned_untracked).await
            };
            match result {
                Ok(()) => changed.push(byte_path_json(&bytes)),
                Err(_) => failed.push(byte_path_json(&bytes)),
            }
        }
        let after = observe_repo(&root).await?;
        let completion = git_completion(
            repo,
            before,
            &after,
            Vec::new(),
            changed.clone(),
            failed.clone(),
            "notRequested",
        );
        if !failed.is_empty() {
            return Ok(MutationOutcome::Partial(
                completion,
                git_err(
                    CODE_PARTIAL_FAILURE,
                    "restore applied to a subset of the planned paths; per-path results are in the operation record",
                    RetrySpec::after_reconcile(),
                    EffectStatus::Applied,
                ),
            ));
        }
        Ok(MutationOutcome::Completed(completion))
        })
    })
    .await
}

async fn delete_untracked_path(root: &Path, rel: &[u8]) -> Result<(), ContractError> {
    let canon = canonical_path_inside(root, rel)?;
    let meta = tokio::fs::symlink_metadata(&canon)
        .await
        .map_err(|e| invalid_argument(format!("stat: {e}")))?;
    if meta.is_dir() {
        tokio::fs::remove_dir_all(&canon)
            .await
            .map_err(|e| invalid_argument(format!("remove directory: {e}")))
    } else {
        tokio::fs::remove_file(&canon)
            .await
            .map_err(|e| invalid_argument(format!("remove file: {e}")))
    }
}

/// TS discard semantics (git.js:1406-1424): untracked → delete; staged-add →
/// unstage; modified/deleted → restore from HEAD (index and worktree).
async fn discard_tracked_path(
    root: &Path,
    rel: &[u8],
    planned_untracked: bool,
) -> Result<(), ContractError> {
    if planned_untracked {
        return delete_untracked_path(root, rel).await;
    }
    let status = git_run_bytes(root, &["status", "--porcelain", "--"], rel, &[])
        .await
        .map_err(|e| git_spawn_error(&e))?;
    let text = String::from_utf8_lossy(&status).into_owned();
    if text.trim().is_empty() {
        return Err(git_err(
            CODE_GIT_CONFLICT,
            "path no longer has changes to discard",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    if text.starts_with("A ") || text.starts_with("AM") || text.starts_with("AD") {
        git_run_bytes(root, &["reset", "-q", "HEAD", "--"], rel, &[])
            .await
            .map_err(|e| git_spawn_error(&e))?;
        return Ok(());
    }
    git_run_bytes(
        root,
        &["restore", "--source=HEAD", "--worktree", "--staged", "--"],
        rel,
        &[],
    )
    .await
    .map_err(|e| git_spawn_error(&e))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// fetch / pull / push / publish.
// ---------------------------------------------------------------------------

fn credential_ref_unsupported(input: &Value) -> Result<(), ContractError> {
    if input
        .get("credentialRef")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty())
    {
        // Brief R3: omitted means "let system git resolve credentials" (the
        // TS baseline behavior); a provided ref must not be silently ignored.
        Err(git_err(
            CODE_UNAVAILABLE,
            "credentialRef injection is not wired yet; omit credentialRef to use the system git credential resolution",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ))
    } else {
        Ok(())
    }
}

fn map_remote_error(e: &GitError, phase: &str) -> ContractError {
    if e.is_auth_failure() {
        return git_err(
            CODE_AUTHENTICATION_FAILED,
            format!("{phase}: authentication failed"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        );
    }
    if e.is_push_rejected() {
        return git_err(
            CODE_REMOTE_REF_CONFLICT,
            format!("{phase}: remote rejected the ref update"),
            RetrySpec::after_reconcile(),
            EffectStatus::NotApplied,
        );
    }
    if e.is_remote_unreachable() {
        return git_err(
            CODE_REMOTE_UNREACHABLE,
            format!("{phase}: unable to connect to remote repository"),
            RetrySpec::backoff_read(Some(1000)),
            EffectStatus::NotApplied,
        );
    }
    git_spawn_error(e)
}

async fn check_remote_exists(root: &Path, remote: &str) -> Result<(), ContractError> {
    let out = git_run_str(root, &["remote"])
        .await
        .map_err(|e| git_spawn_error(&e))?;
    let text = String::from_utf8_lossy(&out).into_owned();
    let remotes: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if !remotes.contains(&remote) {
        return Err(invalid_argument(format!(
            "remote `{remote}` is not configured; configured: {}",
            remotes.join(", ")
        )));
    }
    Ok(())
}

async fn submit_fetch(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    run_mutation(rt, "git.fetch", input, key, |repo, before, input, _op| {
        Box::pin(async move {
            let root = repo.repo_root.clone();
            credential_ref_unsupported(input)?;
            let remote = input
                .get("remoteName")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !valid_remote_name(remote) {
                return Err(invalid_argument(format!("invalid remote name `{remote}`")));
            }
            check_remote_exists(&root, remote).await?;
            let prune = input.get("prune").and_then(Value::as_bool).unwrap_or(false);
            let argv: Vec<&str> = if prune {
                vec!["fetch", "--prune", remote]
            } else {
                vec!["fetch", remote]
            };
            match git_run_net(&root, &argv).await {
                Ok(_) => {}
                Err(e @ GitError::Timeout { .. }) => {
                    return Err(git_err(
                        CODE_RESULT_UNKNOWN,
                        format!("git fetch timed out: {}", e.describe()),
                        RetrySpec::after_reconcile(),
                        EffectStatus::Unknown,
                    ));
                }
                Err(e) => return Err(map_remote_error(&e, "fetch")),
            }
            let after = observe_repo(&root).await?;
            let completion = git_completion(
                repo,
                before,
                &after,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                "notRequested",
            );
            Ok(MutationOutcome::Completed(completion))
        })
    })
    .await
}

async fn submit_pull(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    run_mutation(rt, "git.pull", input, key, |repo, before, input, _op| {
        Box::pin(async move {
            let root = repo.repo_root.clone();
            credential_ref_unsupported(input)?;
            if before.unborn {
                return Err(git_err(
                    CODE_GIT_CONFLICT,
                    "repository has no commits; nothing to pull into",
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            let remote = input
                .get("remoteName")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let remote_branch = input
                .get("remoteBranch")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let strategy = input
                .get("strategy")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !valid_remote_name(remote) {
                return Err(invalid_argument(format!("invalid remote name `{remote}`")));
            }
            if !valid_branch_name(remote_branch) {
                return Err(invalid_argument(format!(
                    "invalid remote branch `{remote_branch}`"
                )));
            }
            if !matches!(strategy, "fastForwardOnly" | "merge" | "rebase") {
                return Err(invalid_argument(format!(
                    "unknown pull strategy `{strategy}`"
                )));
            }
            check_remote_exists(&root, remote).await?;
            match git_run_net(&root, &["fetch", remote, remote_branch]).await {
                Ok(_) => {}
                Err(e @ GitError::Timeout { .. }) => {
                    return Err(git_err(
                        CODE_RESULT_UNKNOWN,
                        format!("git pull fetch stage timed out: {}", e.describe()),
                        RetrySpec::after_reconcile(),
                        EffectStatus::Unknown,
                    ));
                }
                Err(e) => return Err(map_remote_error(&e, "pull")),
            }
            // Strategy is explicit argv, never host config (brief R6).
            let integrate: Vec<&str> = match strategy {
                "fastForwardOnly" => vec!["merge", "--ff-only", "--no-edit", "FETCH_HEAD"],
                "merge" => vec!["merge", "--ff", "--no-edit", "FETCH_HEAD"],
                _ => vec!["rebase", "FETCH_HEAD"],
            };
            match git_run_net(&root, &integrate).await {
                Ok(_) => {}
                Err(e @ GitError::Timeout { .. }) => {
                    return Err(git_err(
                        CODE_RESULT_UNKNOWN,
                        format!("git pull integration timed out: {}", e.describe()),
                        RetrySpec::after_reconcile(),
                        EffectStatus::Unknown,
                    ));
                }
                Err(e) if e.is_merge_conflict() => {
                    let conflicts = conflicted_paths(&root).await;
                    let mut details = Map::new();
                    details.insert(
                        "conflictedPaths".into(),
                        json!(
                            conflicts
                                .iter()
                                .map(|p| byte_path_json(p))
                                .collect::<Vec<_>>()
                        ),
                    );
                    return Err(git_err(
                        CODE_GIT_CONFLICT,
                        "pull created conflicts; resolve them in the worktree and commit",
                        RetrySpec::after_reconcile(),
                        EffectStatus::Applied,
                    )
                    .with_details(details));
                }
                Err(e) if e.is_dirty_worktree() => {
                    return Err(git_err(
                        CODE_DIRTY_WORKTREE,
                        "working tree has uncommitted changes that the pull would overwrite",
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
                Err(e) => return Err(map_remote_error(&e, "pull")),
            }
            let after = observe_repo(&root).await?;
            let completion = git_completion(
                repo,
                before,
                &after,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                "confirmed",
            );
            Ok(MutationOutcome::Completed(completion))
        })
    })
    .await
}

async fn conflicted_paths(root: &Path) -> Vec<Vec<u8>> {
    match porcelain_bytes(root).await {
        Ok(bytes) => parse_porcelain_z(&bytes)
            .into_iter()
            .filter(|e| e.conflicted())
            .map(|e| e.path)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Remote side of the push CAS: `expectedRemoteOid` (empty = the destination
/// must not exist yet). Returns the currently observed remote oid.
async fn ls_remote_oid(
    root: &Path,
    remote: &str,
    destination_ref: &str,
) -> Result<Option<String>, ContractError> {
    let out = git_run_net(root, &["ls-remote", remote])
        .await
        .map_err(|e| map_remote_error(&e, "ls-remote"))?;
    let text = String::from_utf8_lossy(&out).into_owned();
    let normalized = if destination_ref.starts_with("refs/") {
        destination_ref.to_owned()
    } else {
        format!("refs/heads/{destination_ref}")
    };
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let mut parts = line.split_whitespace();
        let (Some(oid), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        if name == normalized || name == destination_ref {
            return Ok(Some(oid.to_owned()));
        }
    }
    Ok(None)
}

/// Push with remote CAS. `--force-with-lease=<ref>:<expect>` is the one-step
/// atomic CAS (empty `<expect>` requires the ref not to exist); without
/// force the remote is verified first, then a plain (non-force) push runs.
async fn push_with_cas(
    root: &Path,
    remote: &str,
    source_ref: &str,
    destination_ref: &str,
    expected_remote_oid: &str,
    force_with_lease: bool,
) -> Result<(), ContractError> {
    if !valid_push_ref(source_ref) {
        return Err(invalid_argument(format!(
            "invalid sourceRef `{source_ref}`"
        )));
    }
    if !valid_push_ref(destination_ref) {
        return Err(invalid_argument(format!(
            "invalid destinationRef `{destination_ref}`"
        )));
    }
    let observed = ls_remote_oid(root, remote, destination_ref).await?;
    match (observed.as_deref(), expected_remote_oid) {
        (Some(observed_oid), expected) if observed_oid != expected => {
            return Err(git_err(
                CODE_REMOTE_REF_CONFLICT,
                format!(
                    "remote ref `{destination_ref}` is at `{observed_oid}`, expected `{expected}`"
                ),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        (None, expected) if !expected.is_empty() => {
            return Err(git_err(
                CODE_REMOTE_REF_CONFLICT,
                format!("remote ref `{destination_ref}` does not exist, expected `{expected}`"),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        _ => {}
    }
    let lease = format!("--force-with-lease={destination_ref}:{expected_remote_oid}");
    let refspec = format!("{source_ref}:{destination_ref}");
    let argv: Vec<&str> = if force_with_lease {
        vec!["push", &lease, remote, &refspec]
    } else {
        vec!["push", remote, &refspec]
    };
    match git_run_net(root, &argv).await {
        Ok(_) => Ok(()),
        Err(e @ GitError::Timeout { .. }) => Err(git_err(
            CODE_RESULT_UNKNOWN,
            format!(
                "git push timed out; remote outcome unknown: {}",
                e.describe()
            ),
            RetrySpec::after_reconcile(),
            EffectStatus::Unknown,
        )),
        Err(e) => Err(map_remote_error(&e, "push")),
    }
}

async fn submit_push(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    run_mutation(rt, "git.push", input, key, |repo, before, input, _op| {
        Box::pin(async move {
            let root = repo.repo_root.clone();
            credential_ref_unsupported(input)?;
            let remote = input
                .get("remoteName")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !valid_remote_name(remote) {
                return Err(invalid_argument(format!("invalid remote name `{remote}`")));
            }
            check_remote_exists(&root, remote).await?;
            push_with_cas(
                &root,
                remote,
                input
                    .get("sourceRef")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                input
                    .get("destinationRef")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                input
                    .get("expectedRemoteOid")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                input
                    .get("forceWithLease")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            )
            .await?;
            let after = observe_repo(&root).await?;
            let completion = git_completion(
                repo,
                before,
                &after,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                "confirmed",
            );
            Ok(MutationOutcome::Completed(completion))
        })
    })
    .await
}

async fn submit_publish(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    run_mutation(rt, "git.publish", input, key, |repo, before, input, _op| {
        Box::pin(async move {
        let root = repo.repo_root.clone();
        credential_ref_unsupported(input)?;
        let remote = input.get("remoteName").and_then(Value::as_str).unwrap_or_default();
        let branch = input.get("branchName").and_then(Value::as_str).unwrap_or_default();
        let expected_remote_oid = input
            .get("expectedRemoteOid")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !valid_remote_name(remote) {
            return Err(invalid_argument(format!("invalid remote name `{remote}`")));
        }
        if !valid_branch_name(branch) {
            return Err(invalid_argument(format!("invalid branch name `{branch}`")));
        }
        if before.unborn {
            return Err(git_err(
                CODE_GIT_CONFLICT,
                "repository has no commits; nothing to publish",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        check_remote_exists(&root, remote).await?;
        // The publish target must be the currently selected branch (TS
        // git.js:1316-1322).
        let current = git_run_str(&root, &["branch", "--show-current"])
            .await
            .map(|o| String::from_utf8_lossy(&o).trim_end().to_owned())
            .map_err(|e| git_spawn_error(&e))?;
        if current != branch {
            return Err(git_err(
                CODE_GIT_CONFLICT,
                format!(
                    "branch mismatch: current branch is `{current}`, tried to publish `{branch}`"
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        // Segment 1: the remote push (with CAS semantics).
        push_with_cas(&root, remote, branch, branch, expected_remote_oid, false).await?;
        let after = observe_repo(&root).await?;
        // Segment 2: the local upstream configuration — reported separately
        // (contract: "远端副作用和本地upstream设置分别报告"); a failure here
        // is a partial result (the remote is already pushed, never blindly
        // re-pushed).
        let upstream_refspec = format!("{remote}/{branch}");
        let upstream_ok = git_run_str(
            &root,
            &[
                "branch",
                "--set-upstream-to",
                &upstream_refspec,
                branch,
            ],
        )
        .await
        .is_ok();
        let completion = git_completion(
            repo,
            before,
            &after,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            "confirmed",
        );
        if !upstream_ok {
            return Ok(MutationOutcome::Partial(
                completion,
                git_err(
                    CODE_GIT_CONFLICT,
                    "remote push succeeded but the local upstream configuration failed; the remote branch is already published — set branch.<name>.remote manually instead of re-publishing",
                    RetrySpec::after_reconcile(),
                    EffectStatus::Applied,
                ),
            ));
        }
        Ok(MutationOutcome::Completed(completion))
        })
    })
    .await
}

// ---------------------------------------------------------------------------
// git.suggest_message.
// ---------------------------------------------------------------------------

async fn submit_suggest_message(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    run_mutation(
        rt,
        "git.suggest_message",
        input,
        key,
        |repo, before, input, _op| {
            Box::pin(async move {
                let root = repo.repo_root.clone();
                let selection = input.get("selection").cloned().unwrap_or(Value::Null);
                let selected: Vec<Vec<u8>> = match selection
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                {
                    "paths" => selection_paths(&selection)?,
                    "index" => staged_paths(&root).await?,
                    "all" => parse_porcelain_z(&porcelain_bytes(&root).await?)
                        .into_iter()
                        .filter(|e| !e.conflicted())
                        .map(|e| e.path)
                        .collect(),
                    other => {
                        return Err(invalid_argument(format!(
                            "unknown selection mode `{other}`"
                        )));
                    }
                };
                // The diff context mirrors the TS baseline (git.js:859-895):
                // tracked changes via `git diff HEAD`, untracked via content.
                // Kept for the model seam to consume once it lands.
                let mut diff_context = String::new();
                for path in &selected {
                    if let Ok(out) = git_run_bytes(
                        &root,
                        &[
                            "-c",
                            "core.quotePath=false",
                            "diff",
                            "HEAD",
                            "--full-index",
                            "--",
                        ],
                        path,
                        &[],
                    )
                    .await
                    {
                        diff_context.push_str(&format!(
                            "\n--- {} ---\n{}",
                            path_display(path),
                            String::from_utf8_lossy(&out)
                        ));
                    }
                }
                if diff_context.trim().is_empty() {
                    for path in &selected {
                        let abs = root.join(bytes_to_os(path));
                        if let Ok(bytes) = tokio::fs::read(&abs).await {
                            diff_context.push_str(&format!(
                                "\n--- {} (new file) ---\n{}\n",
                                path_display(path),
                                String::from_utf8_lossy(&bytes[..bytes.len().min(1000)])
                            ));
                        }
                    }
                }
                let _ = diff_context;
                // Model seam: `models.invoke` belongs to the runtime task. Until
                // it lands, the suggestion fails with MODEL_UNAVAILABLE — never
                // an empty-string fake success (acceptance: "模型失败不能用空
                // 字符串伪装建议成功"; the TS fallback "chore: update files" is
                // a registered contract divergence).
                let message = invoke_suggestion_model(&selected).await?;
                let completion = json!({
                    "message": message,
                    "sourceStateToken": before.state_token(),
                    "selectedPaths": selected.iter().map(|p| byte_path_json(p)).collect::<Vec<_>>(),
                });
                Ok(MutationOutcome::Completed(completion))
            })
        },
    )
    .await
}

/// Placeholder for the runtime task's `models.invoke` seam.
async fn invoke_suggestion_model(_selected: &[Vec<u8>]) -> Result<String, ContractError> {
    Err(git_err(
        CODE_MODEL_UNAVAILABLE,
        "commit message suggestion needs the model invocation seam (models.invoke), which is not available in this build",
        RetrySpec::backoff_read(Some(1000)),
        EffectStatus::NotApplied,
    ))
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::initialize_instance;
    use crate::public_v1::bootstrap::load_local_instance_and_token;
    use crate::public_v1::store::InstanceMeta;
    use crate::public_v1::store::V1Store;
    use crate::public_v1::test_support::TempDir;

    fn git_present() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn git_sync(cwd: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("spawn git")
            .success();
        assert!(ok, "git {} failed in {}", args.join(" "), cwd.display());
    }

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    /// Whether the host's global git config provides an identity (unit tests
    /// cannot null out the ambient environment; the IDENTITY_REQUIRED
    /// negative path is fully deterministic only under the e2e harness,
    /// which sets GIT_CONFIG_GLOBAL=/dev/null).
    fn host_has_git_identity() -> bool {
        let out = std::process::Command::new("git")
            .args(["config", "--get", "user.email"])
            .output()
            .expect("git config");
        out.status.success() && !String::from_utf8_lossy(&out.stdout).trim().is_empty()
    }

    /// A runtime whose home carries a registered project rooted at
    /// `<home>/roots/root-1/ws` with a freshly initialized repository.
    async fn setup_repo(tag: &str) -> (Arc<PublicV1Runtime>, String, PathBuf, TempDir) {
        let tmp = TempDir::new(tag);
        let home = tmp.0.join("pd-home");
        initialize_instance(&home, None, Some("local"), Some(true)).unwrap();
        let (file, _token) = load_local_instance_and_token(&home, None).unwrap();
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
        let rt = PublicV1Runtime::new(store.clone());
        // The workspace lives under a host-declared authorized root (the
        // t013 files-domain convention: rootId → authorized_roots row).
        let declared_root = tmp.0.join("authorized-root");
        let ws = declared_root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        git_sync(&ws, &["init", "-q", "-b", "main"]);
        // Neutralize host-side commit.gpgsign for deterministic commits.
        git_sync(&ws, &["config", "commit.gpgsign", "false"]);
        {
            let conn = store.conn.lock().expect("sqlite mutex");
            conn.execute(
                "INSERT INTO authorized_roots (root_id, instance_id, absolute_path, created_at)
                 VALUES ('root-1', ?1, ?2, ?3)",
                params![
                    file.instance_id,
                    declared_root.canonicalize().unwrap().display().to_string(),
                    now_rfc3339(),
                ],
            )
            .unwrap();
        }
        let iid = file.instance_id.clone();
        rt.dispatch(
            "project.register",
            json!({
                "instanceId": iid,
                "projectId": "p-git",
                "name": "Git",
                "environmentId": "local",
                "root": {"instanceId": iid, "rootId": "root-1", "relativePath": "ws"}
            }),
            None,
            None,
        )
        .await
        .unwrap();
        (rt, iid, ws, tmp)
    }

    async fn state_of(rt: &Arc<PublicV1Runtime>) -> Value {
        match rt
            .dispatch("git.status", json!({"projectId": "p-git"}), None, None)
            .await
        {
            Ok((ResponseOutcome::Succeeded, result)) => result["state"].clone(),
            Ok((outcome, _)) => panic!("git.status unexpected outcome: {outcome:?}"),
            Err(e) => panic!("git.status failed: {e:?}"),
        }
    }

    fn dispatch_err_code(e: &DispatchError) -> String {
        match e {
            DispatchError::Application(err) => err.code.clone(),
            DispatchError::Rpc { message, .. } => format!("RPC({message})"),
        }
    }

    async fn set_project_identity(rt: &Arc<PublicV1Runtime>, revision: &str) -> Value {
        match rt
            .dispatch(
                "git_identity.set",
                json!({
                    "scope": "project",
                    "projectId": "p-git",
                    "name": "Test User",
                    "email": "test@example.com",
                    "expectedRevision": revision,
                }),
                None,
                None,
            )
            .await
        {
            Ok((ResponseOutcome::Succeeded, result)) => result,
            Ok((outcome, _)) => panic!("git_identity.set unexpected outcome: {outcome:?}"),
            Err(e) => panic!("git_identity.set failed: {e:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Pure-function tests.
    // -----------------------------------------------------------------------

    #[test]
    fn porcelain_z_parses_rename_untracked_and_basic_statuses() {
        let mut data = Vec::new();
        data.extend_from_slice(b"R  renamed.txt");
        data.push(0);
        data.extend_from_slice(b"a.txt");
        data.push(0);
        data.extend_from_slice(b" M sub/b.txt");
        data.push(0);
        data.extend_from_slice(b"?? untracked.txt");
        data.push(0);
        data.extend_from_slice(b"UU conflicted.txt");
        data.push(0);
        let entries = parse_porcelain_z(&data);
        // The rename is ONE entry: porcelain v1 -z appends the original path
        // as a second NUL-terminated field ("R  new\0old\0"), not a separate
        // record — 4 status records here (R-pair, M, ??, UU).
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].path, b"renamed.txt");
        assert_eq!(
            entries[0].original_path.as_deref(),
            Some(b"a.txt".as_slice())
        );
        assert!(!entries[0].untracked() && !entries[0].conflicted());
        assert_eq!(entries[1].x, b' ');
        assert_eq!(entries[1].y, b'M');
        assert!(entries[2].untracked());
        assert!(entries[3].conflicted());
    }

    #[test]
    fn state_token_is_deterministic_and_content_sensitive() {
        let base = RepoState {
            head_oid: Some("abc".into()),
            head_ref: Some("refs/heads/main".into()),
            unborn: false,
            detached: false,
            porcelain_z: b" M a.txt\0".to_vec(),
            remote_refs: vec![("refs/remotes/origin/main".to_owned(), "def".to_owned())],
            remote_names: vec!["origin".to_owned()],
        };
        let same = RepoState {
            ..clone_state(&base)
        };
        assert_eq!(base.state_token(), same.state_token());
        let moved = RepoState {
            head_oid: Some("xyz".into()),
            ..clone_state(&base)
        };
        assert_ne!(base.state_token(), moved.state_token());
        let dirtier = RepoState {
            porcelain_z: b" M a.txt\0?? b.txt\0".to_vec(),
            ..clone_state(&base)
        };
        assert_ne!(base.state_token(), dirtier.state_token());
        let fetched = RepoState {
            remote_refs: vec![("refs/remotes/origin/main".to_owned(), "newerv".to_owned())],
            ..clone_state(&base)
        };
        assert_ne!(base.state_token(), fetched.state_token());
    }

    /// `RepoState` is not `Clone` (holds Vec fields only); a manual copier for
    /// test fixtures.
    fn clone_state(s: &RepoState) -> RepoState {
        RepoState {
            head_oid: s.head_oid.clone(),
            head_ref: s.head_ref.clone(),
            unborn: s.unborn,
            detached: s.detached,
            porcelain_z: s.porcelain_z.clone(),
            remote_refs: s.remote_refs.clone(),
            remote_names: s.remote_names.clone(),
        }
    }

    #[test]
    fn unified_diff_parser_structures_files_hunks_and_lines() {
        // NOTE: Rust's `\<newline>` string continuation strips the next
        // line's leading whitespace, so the context line's diff-prefix space
        // must be spelled `\x20` — a bare leading space would be swallowed
        // and the line silently dropped from the hunk.
        let diff = b"diff --git a/a.txt b/a.txt\n\
index 1111111111111111111111111111111111111111..2222222222222222222222222222222222222222 100644\n\
--- a/a.txt\n\
+++ b/a.txt\n\
@@ -1,2 +1,3 @@\n\
\x20context\n\
-old\n\
+new\n\
+added\n\
diff --git a/new.bin b/new.bin\n\
new file mode 100644\n\
index 0000000..3333333\n\
Binary files /dev/null and b/new.bin differ\n";
        let files = parse_unified_diff(diff);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0]["oldPath"]["value"], "a.txt");
        assert_eq!(files[0]["change"], "modified");
        assert_eq!(files[0]["hunks"].as_array().unwrap().len(), 1);
        let hunk = &files[0]["hunks"][0];
        assert_eq!(hunk["oldStart"], 1);
        assert_eq!(hunk["oldCount"], 2);
        assert_eq!(hunk["newStart"], 1);
        assert_eq!(hunk["newCount"], 3);
        let lines = hunk["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0]["kind"], "context");
        assert_eq!(lines[0]["oldLine"], 1);
        assert_eq!(lines[1]["kind"], "deleted");
        assert_eq!(lines[1]["oldLine"], 2);
        assert_eq!(lines[2]["kind"], "added");
        assert_eq!(lines[2]["newLine"], 2);
        assert_eq!(lines[3]["kind"], "added");
        assert_eq!(lines[3]["newLine"], 3);
        assert_eq!(
            files[0]["oldOid"],
            "1111111111111111111111111111111111111111"
        );
        assert_eq!(files[1]["change"], "added");
        assert_eq!(files[1]["binary"], json!(true));
        assert_eq!(files[1]["hunks"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn unquote_git_path_handles_octal_escapes() {
        assert_eq!(unquote_git_path(b"plain"), b"plain".to_vec());
        // "中文.txt" UTF-8 bytes as octal escapes.
        assert_eq!(
            unquote_git_path(b"\"\\344\\270\\255\\346\\226\\207.txt\""),
            "中文.txt".as_bytes().to_vec()
        );
    }

    #[test]
    fn byte_path_roundtrip_utf8_and_base64() {
        let utf8 = byte_path_json("中文.txt".as_bytes());
        assert_eq!(utf8["encoding"], "utf8");
        assert_eq!(utf8["value"], "中文.txt");
        assert_eq!(
            byte_path_bytes(&utf8).unwrap(),
            "中文.txt".as_bytes().to_vec()
        );
        let raw = vec![0xff, 0xfe, 0x01];
        let b64 = byte_path_json(&raw);
        assert_eq!(b64["encoding"], "base64");
        assert_eq!(byte_path_bytes(&b64).unwrap(), raw);
    }

    #[test]
    fn repo_relative_validation_rejects_traversal() {
        assert!(validate_repo_relative_path(b"a/b.txt").is_ok());
        assert!(validate_repo_relative_path("中文.txt".as_bytes()).is_ok());
        assert_eq!(
            validate_repo_relative_path(b"/etc/passwd")
                .unwrap_err()
                .code,
            CODE_PATH_ESCAPE
        );
        assert_eq!(
            validate_repo_relative_path(b"../escape.txt")
                .unwrap_err()
                .code,
            CODE_PATH_ESCAPE
        );
        assert!(validate_repo_relative_path(b"a\x00b").is_err());
    }

    #[test]
    fn push_ref_validation_blocks_refspec_metacharacters() {
        assert!(valid_push_ref("refs/heads/main"));
        assert!(valid_push_ref("main"));
        assert!(!valid_push_ref("main:refs/heads/evil"));
        assert!(!valid_push_ref("--exec=touch"));
        assert!(!valid_push_ref(""));
    }

    // -----------------------------------------------------------------------
    // fx-h review fixes: timeout/group-kill classification, unborn diff,
    // schema-legal empty fileWithDiff, idempotent rejection terminal state.
    // -----------------------------------------------------------------------

    #[test]
    fn killed_write_timeout_is_unknown_and_read_stays_retryable() {
        // A killed read is side-effect free: retryable UNAVAILABLE/NotApplied.
        let read = git_spawn_error(&GitError::Timeout { write: false });
        assert_eq!(read.code, CODE_UNAVAILABLE);
        assert_eq!(read.effect, EffectStatus::NotApplied);
        // A killed write may have taken effect (a `git commit` updates refs
        // before its exit is observable): RESULT_UNKNOWN/Unknown.
        let write = git_spawn_error(&GitError::Timeout { write: true });
        assert_eq!(write.code, CODE_RESULT_UNKNOWN);
        assert_eq!(write.effect, EffectStatus::Unknown);
        assert_eq!(
            write.retry.mode,
            agentcpm_pd_protocol::errors::RetryMode::AfterReconcile
        );
        // A blocked `.git/index.lock` (git 2.54 stderr) keeps UNAVAILABLE but
        // carries the actionable hint instead of the bare "git failed" text.
        let lock = git_spawn_error(&GitError::Failure(GitFailure {
            stderr: b"fatal: Unable to create '/repo/.git/index.lock': File exists.\n\nAnother git process seems to be running in this repository, or the lock file may be stale.\nPlease remove the file manually to make progress.\n".to_vec(),
            message: "Command failed: git add -- f.txt".to_owned(),
        }));
        assert_eq!(lock.code, CODE_UNAVAILABLE);
        assert_eq!(lock.effect, EffectStatus::NotApplied);
        assert!(
            lock.message.contains(".git/index.lock")
                && lock.message.contains("remove the lock file"),
            "the index.lock hint must survive the 256-char message clamp: {:?}",
            lock.message
        );
    }

    #[test]
    fn write_command_classification_for_timeout_effect_status() {
        let run = |parts: &[&str]| os(parts);
        // Reads the observation paths run.
        assert!(!is_write_git_command(&os_ref(&run(&[
            "status",
            "--porcelain=v1",
            "-z"
        ]))));
        assert!(!is_write_git_command(&os_ref(&run(&[
            "-c",
            "core.quotePath=false",
            "diff",
            "--full-index"
        ]))));
        assert!(!is_write_git_command(&os_ref(&run(&[
            "config",
            "--get",
            "user.email"
        ]))));
        assert!(!is_write_git_command(&os_ref(&run(&[
            "branch",
            "--show-current"
        ]))));
        assert!(!is_write_git_command(&os_ref(&run(&[
            "log", "-n1", "HEAD"
        ]))));
        assert!(!is_write_git_command(&os_ref(&run(&[
            "rev-parse",
            "--show-toplevel"
        ]))));
        assert!(!is_write_git_command(&[]));
        // Writes the mutations run (with or without global `-c` options).
        assert!(is_write_git_command(&os_ref(&run(&["add", "--", "a.txt"]))));
        assert!(is_write_git_command(&os_ref(&run(&["commit", "-m", "x"]))));
        assert!(is_write_git_command(&os_ref(&run(&[
            "-c",
            "core.quotePath=false",
            "checkout",
            "-q",
            "feature/x"
        ]))));
        assert!(is_write_git_command(&os_ref(&run(&[
            "push", "origin", "main"
        ]))));
        assert!(is_write_git_command(&os_ref(&run(&[
            "fetch", "--prune", "origin"
        ]))));
        // Dual-mode subcommands count as writes outside their read-only
        // shapes used by this domain.
        assert!(is_write_git_command(&os_ref(&run(&[
            "config",
            "user.name",
            "X"
        ]))));
        assert!(is_write_git_command(&os_ref(&run(&[
            "branch",
            "feature/x"
        ]))));
    }

    #[test]
    fn merge_unborn_diffs_prefers_the_unstaged_file_per_path() {
        let staged = vec![
            json!({"newPath": {"encoding": "utf8", "value": "a.txt"}, "change": "added"}),
            json!({"newPath": {"encoding": "utf8", "value": "b.txt"}, "change": "added"}),
        ];
        let unstaged = vec![
            json!({"newPath": {"encoding": "utf8", "value": "a.txt"}, "change": "modified"}),
            json!({"newPath": {"encoding": "utf8", "value": "c.txt"}, "change": "deleted"}),
        ];
        let merged = merge_unborn_diffs(staged, unstaged);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0]["newPath"]["value"], json!("a.txt"));
        assert_eq!(
            merged[0]["change"],
            json!("modified"),
            "the unstaged diff wins per path (git.js:409-424 preference), keeping the position"
        );
        assert_eq!(merged[1]["change"], json!("added"));
        assert_eq!(merged[2]["newPath"]["value"], json!("c.txt"));
        assert_eq!(merged[2]["change"], json!("deleted"));
    }

    #[test]
    fn empty_file_diff_is_a_schema_legal_filediff() {
        let diff = empty_file_diff(b"clean.txt");
        assert!(diff.is_object(), "diff must never be null for a known path");
        assert_eq!(diff["oldPath"]["value"], json!("clean.txt"));
        assert_eq!(diff["newPath"]["value"], json!("clean.txt"));
        assert_eq!(diff["change"], json!("modified"));
        assert_eq!(diff["binary"], json!(false));
        assert_eq!(diff["hunks"].as_array().map(Vec::len), Some(0));
        assert_eq!(diff["truncated"], json!(false));

        // The full payload shape passes the registered artifact schema from
        // the contract; the pre-fix `diff: null` shape must be rejected.
        let contract =
            agentcpm_pd_protocol::registry::OperationRegistry::global().public_contract();
        let schema = &contract["artifactSchemas"]["git.fileWithDiff.v1"];
        let payload_with_diff = json!({
            "schemaId": "git.fileWithDiff.v1",
            "schemaVersion": "1",
            "stateToken": "gst1-x",
            "path": byte_path_json(b"clean.txt"),
            "fileArtifactId": null,
            "fileRevision": null,
            "diff": diff,
        });
        agentcpm_pd_protocol::schema_validator::validate_against_schema(&payload_with_diff, schema)
            .unwrap_or_else(|errors| panic!("payload violates the schema: {errors:?}"));
        let payload_with_null = json!({
            "schemaId": "git.fileWithDiff.v1",
            "schemaVersion": "1",
            "stateToken": "gst1-x",
            "path": byte_path_json(b"clean.txt"),
            "fileArtifactId": null,
            "fileRevision": null,
            "diff": null,
        });
        agentcpm_pd_protocol::schema_validator::validate_against_schema(&payload_with_null, schema)
            .expect_err("a null diff violates the registered schema");
    }

    // -----------------------------------------------------------------------
    // Integration tests (real git binary; skipped when git is unavailable).
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn status_reports_unborn_repository_and_validates_schema() {
        if !git_present() {
            eprintln!("skipping: git not available");
            return;
        }
        let (rt, _iid, _ws, _tmp) = setup_repo("git-status").await;
        let state = state_of(&rt).await;
        assert_eq!(state["unborn"], json!(true));
        assert_eq!(state["detached"], json!(false));
        assert_eq!(state["files"].as_array().unwrap().len(), 0);
        assert_eq!(state["remoteNames"].as_array().unwrap().len(), 0);
        assert!(state["stateToken"].as_str().is_some_and(|t| !t.is_empty()));
        // The full result must satisfy the contract schema.
        let result = rt
            .dispatch("git.status", json!({"projectId": "p-git"}), None, None)
            .await
            .unwrap()
            .1;
        agentcpm_pd_protocol::registry::OperationRegistry::global()
            .validate_result("git.status", &result)
            .expect("git.status result passes the contract schema");
    }

    #[tokio::test]
    async fn status_reports_not_a_repository_for_plain_directory() {
        if !git_present() {
            eprintln!("skipping: git not available");
            return;
        }
        let tmp = TempDir::new("git-not-a-repo");
        let home = tmp.0.join("pd-home");
        initialize_instance(&home, None, Some("local"), Some(true)).unwrap();
        let (file, _token) = load_local_instance_and_token(&home, None).unwrap();
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
        let rt = PublicV1Runtime::new(store.clone());
        // Declared root, plain workspace directory: no git repository.
        let declared_root = tmp.0.join("plain-root");
        let ws = declared_root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        {
            let conn = store.conn.lock().expect("sqlite mutex");
            conn.execute(
                "INSERT INTO authorized_roots (root_id, instance_id, absolute_path, created_at)
                 VALUES ('root-1', ?1, ?2, ?3)",
                params![
                    file.instance_id,
                    declared_root.canonicalize().unwrap().display().to_string(),
                    now_rfc3339(),
                ],
            )
            .unwrap();
        }
        rt.dispatch(
            "project.register",
            json!({
                "instanceId": file.instance_id,
                "projectId": "p-plain",
                "name": "Plain",
                "environmentId": "local",
                "root": {"instanceId": file.instance_id, "rootId": "root-1", "relativePath": "ws"}
            }),
            None,
            None,
        )
        .await
        .unwrap();
        let err = rt
            .dispatch("git.status", json!({"projectId": "p-plain"}), None, None)
            .await
            .unwrap_err();
        assert_eq!(dispatch_err_code(&err), CODE_NOT_A_REPOSITORY);
    }

    #[tokio::test]
    async fn commit_requires_state_token_and_identity_then_uncommit_restores() {
        if !git_present() {
            eprintln!("skipping: git not available");
            return;
        }
        let (rt, _iid, ws, _tmp) = setup_repo("git-commit").await;
        write_file(&ws.join("hello.txt"), "hello\n");
        let state = state_of(&rt).await;
        let token = state["stateToken"].as_str().unwrap().to_owned();

        // Missing identity -> IDENTITY_REQUIRED (no commit created). This
        // negative path is only observable when the host provides no system
        // git identity; the e2e harness (isolated GIT_CONFIG_GLOBAL) covers
        // it deterministically.
        if !host_has_git_identity() {
            let err = rt
                .dispatch(
                    "git.commit",
                    json!({
                        "projectId": "p-git",
                        "expectedStateToken": token,
                        "selection": {"mode": "all"},
                        "message": "Initial commit",
                        "initial": true,
                    }),
                    Some("k-commit-no-id"),
                    None,
                )
                .await
                .unwrap_err();
            assert_eq!(dispatch_err_code(&err), CODE_IDENTITY_REQUIRED);
        } else {
            eprintln!(
                "note: host git identity present; IDENTITY_REQUIRED covered by the e2e harness"
            );
        }

        // Provide the project identity and commit with a fresh token (a
        // rejected key must not be reused; a new key works either way).
        set_project_identity(&rt, "").await;
        let state = state_of(&rt).await;
        let token = state["stateToken"].as_str().unwrap().to_owned();
        let (outcome, receipt) = rt
            .dispatch(
                "git.commit",
                json!({
                    "projectId": "p-git",
                    "expectedStateToken": token,
                    "selection": {"mode": "all"},
                    "message": "Initial commit",
                    "initial": true,
                }),
                Some("k-commit-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome, ResponseOutcome::Accepted);
        assert_eq!(receipt["kind"], "operation");
        assert_eq!(receipt["state"], "succeeded");
        agentcpm_pd_protocol::registry::OperationRegistry::global()
            .validate_result("git.commit", &receipt)
            .expect("git.commit receipt passes the contract schema");
        // Same key + same input replays the receipt (deduplicated).
        let (outcome2, receipt2) = rt
            .dispatch(
                "git.commit",
                json!({
                    "projectId": "p-git",
                    "expectedStateToken": token,
                    "selection": {"mode": "all"},
                    "message": "Initial commit",
                    "initial": true,
                }),
                Some("k-commit-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome2, ResponseOutcome::Accepted);
        assert_eq!(receipt2["deduplicated"], json!(true));
        assert_eq!(receipt2["operationId"], receipt["operationId"]);
        assert_eq!(receipt2["state"], json!("succeeded"));
        // Same key + different input conflicts.
        let err = rt
            .dispatch(
                "git.commit",
                json!({
                    "projectId": "p-git",
                    "expectedStateToken": token,
                    "selection": {"mode": "all"},
                    "message": "Other message",
                    "initial": true,
                }),
                Some("k-commit-1"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(dispatch_err_code(&err), "IDEMPOTENCY_CONFLICT");

        let state = state_of(&rt).await;
        assert_eq!(state["unborn"], json!(false));
        let head = state["headOid"].as_str().unwrap().to_owned();
        let token_after = state["stateToken"].as_str().unwrap().to_owned();
        assert_ne!(token, token_after);

        // Stale stateToken -> REVISION_CONFLICT.
        let err = rt
            .dispatch(
                "git.commit",
                json!({
                    "projectId": "p-git",
                    "expectedStateToken": token,
                    "selection": {"mode": "all"},
                    "message": "Second commit",
                }),
                Some("k-commit-stale"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(dispatch_err_code(&err), CODE_REVISION_CONFLICT);

        // uncommit with the HEAD CAS anchor restores the unborn state with
        // the content still staged.
        let (outcome, receipt) = rt
            .dispatch(
                "git.uncommit",
                json!({
                    "projectId": "p-git",
                    "expectedStateToken": token_after,
                    "expectedHeadOid": head,
                }),
                Some("k-uncommit-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome, ResponseOutcome::Accepted);
        assert_eq!(receipt["state"], "succeeded");
        let state = state_of(&rt).await;
        assert_eq!(state["unborn"], json!(true));
        let files = state["files"].as_array().unwrap();
        assert!(
            files
                .iter()
                .any(|f| f["path"]["value"] == "hello.txt" && f["indexStatus"] == "A"),
            "uncommitted content stays staged: {files:?}"
        );
    }

    #[tokio::test]
    async fn branch_lifecycle_and_delete_cas() {
        if !git_present() {
            eprintln!("skipping: git not available");
            return;
        }
        let (rt, _iid, ws, _tmp) = setup_repo("git-branch").await;
        set_project_identity(&rt, "").await;
        write_file(&ws.join("seed.txt"), "seed\n");
        let state = state_of(&rt).await;
        let token = state["stateToken"].as_str().unwrap().to_owned();
        rt.dispatch(
            "git.commit",
            json!({
                "projectId": "p-git",
                "expectedStateToken": token,
                "selection": {"mode": "all"},
                "message": "Initial commit",
                "initial": true,
            }),
            Some("k-branch-init"),
            None,
        )
        .await
        .unwrap();
        let state = state_of(&rt).await;
        let token = state["stateToken"].as_str().unwrap().to_owned();
        let head = state["headOid"].as_str().unwrap().to_owned();

        // Create at an explicit startOid without checking out.
        rt.dispatch(
            "git.create_branch",
            json!({
                "projectId": "p-git",
                "expectedStateToken": token,
                "branchName": "feature/x",
                "startOid": head,
            }),
            Some("k-branch-create"),
            None,
        )
        .await
        .unwrap();
        // Duplicate creation -> ALREADY_EXISTS.
        let err = rt
            .dispatch(
                "git.create_branch",
                json!({
                    "projectId": "p-git",
                    "expectedStateToken": token,
                    "branchName": "feature/x",
                    "startOid": head,
                }),
                Some("k-branch-dup"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(dispatch_err_code(&err), CODE_ALREADY_EXISTS);
        // Invalid startOid -> INVALID_REF.
        let err = rt
            .dispatch(
                "git.create_branch",
                json!({
                    "projectId": "p-git",
                    "expectedStateToken": token,
                    "branchName": "feature/y",
                    "startOid": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
                }),
                Some("k-branch-badref"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(dispatch_err_code(&err), CODE_INVALID_REF);

        // Deleting the checked-out branch -> BRANCH_CHECKED_OUT.
        let err = rt
            .dispatch(
                "git.delete_branch",
                json!({
                    "projectId": "p-git",
                    "expectedStateToken": token,
                    "branchName": "main",
                    "expectedBranchOid": head,
                }),
                Some("k-branch-del-current"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(dispatch_err_code(&err), CODE_BRANCH_CHECKED_OUT);

        // Wrong expectedBranchOid -> REVISION_CONFLICT.
        let err = rt
            .dispatch(
                "git.delete_branch",
                json!({
                    "projectId": "p-git",
                    "expectedStateToken": token,
                    "branchName": "feature/x",
                    "expectedBranchOid": "0000000000000000000000000000000000000000",
                }),
                Some("k-branch-del-wrong"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(dispatch_err_code(&err), CODE_REVISION_CONFLICT);

        // Correct CAS deletes the (merged) branch.
        rt.dispatch(
            "git.delete_branch",
            json!({
                "projectId": "p-git",
                "expectedStateToken": token,
                "branchName": "feature/x",
                "expectedBranchOid": head,
            }),
            Some("k-branch-del-ok"),
            None,
        )
        .await
        .unwrap();
        let state = state_of(&rt).await;
        let token = state["stateToken"].as_str().unwrap().to_owned();

        // Diverge main from feature/z, then make the worktree dirty: the
        // checkout that would overwrite the dirty file is refused.
        rt.dispatch(
            "git.create_branch",
            json!({
                "projectId": "p-git",
                "expectedStateToken": token,
                "branchName": "feature/z",
                "startOid": head,
            }),
            Some("k-branch-z"),
            None,
        )
        .await
        .unwrap();
        write_file(&ws.join("seed.txt"), "v2\n");
        let state = state_of(&rt).await;
        let token = state["stateToken"].as_str().unwrap().to_owned();
        rt.dispatch(
            "git.commit",
            json!({
                "projectId": "p-git",
                "expectedStateToken": token,
                "selection": {"mode": "all"},
                "message": "Diverge main",
            }),
            Some("k-branch-diverge"),
            None,
        )
        .await
        .unwrap();
        write_file(&ws.join("seed.txt"), "dirty\n");
        let state = state_of(&rt).await;
        let token = state["stateToken"].as_str().unwrap().to_owned();
        let err = rt
            .dispatch(
                "git.checkout",
                json!({
                    "projectId": "p-git",
                    "expectedStateToken": token,
                    "targetRef": "feature/z",
                }),
                Some("k-checkout-dirty"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(dispatch_err_code(&err), CODE_DIRTY_WORKTREE);
    }

    #[tokio::test]
    async fn plan_restore_conflicts_and_restore_executes_per_path() {
        if !git_present() {
            eprintln!("skipping: git not available");
            return;
        }
        let (rt, _iid, ws, tmp) = setup_repo("git-restore").await;
        set_project_identity(&rt, "").await;
        write_file(&ws.join("tracked.txt"), "v1\n");
        let state = state_of(&rt).await;
        let token = state["stateToken"].as_str().unwrap().to_owned();
        rt.dispatch(
            "git.commit",
            json!({
                "projectId": "p-git",
                "expectedStateToken": token,
                "selection": {"mode": "all"},
                "message": "Initial commit",
                "initial": true,
            }),
            Some("k-restore-init"),
            None,
        )
        .await
        .unwrap();

        write_file(&ws.join("tracked.txt"), "v2\n");
        write_file(&ws.join("untracked.txt"), "scratch\n");
        let state = state_of(&rt).await;
        let token = state["stateToken"].as_str().unwrap().to_owned();

        // discardTracked over an untracked path reports a conflict entry.
        let (_, planned) = rt
            .dispatch(
                "git.plan_restore",
                json!({
                    "projectId": "p-git",
                    "action": "discardTracked",
                    "paths": [
                        {"encoding": "utf8", "value": "tracked.txt"},
                        {"encoding": "utf8", "value": "untracked.txt"},
                    ],
                    "expectedStateToken": token,
                }),
                None,
                None,
            )
            .await
            .unwrap();
        let plan = &planned["plan"];
        assert_eq!(plan["affectedResources"], json!(["tracked.txt"]));
        assert_eq!(plan["conflicts"][0]["resourceId"], "untracked.txt");
        assert_eq!(plan["conflicts"][0]["code"], "PATH_UNTRACKED");
        assert_eq!(plan["destructive"], json!(true));
        agentcpm_pd_protocol::registry::OperationRegistry::global()
            .validate_result("git.plan_restore", &planned)
            .expect("git.plan_restore result passes the contract schema");
        let plan_id = plan["planId"].as_str().unwrap().to_owned();
        let plan_revision = plan["planRevision"].as_str().unwrap().to_owned();

        // Executing the plan restores tracked.txt only.
        rt.dispatch(
            "git.restore",
            json!({
                "projectId": "p-git",
                "expectedStateToken": token,
                "planId": plan_id,
                "expectedPlanRevision": plan_revision,
            }),
            Some("k-restore-1"),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(ws.join("tracked.txt")).unwrap(),
            "v1\n"
        );
        assert_eq!(
            std::fs::read_to_string(ws.join("untracked.txt")).unwrap(),
            "scratch\n"
        );

        // A symlink that resolves outside the repository fails per path
        // (PATH_ESCAPE containment check) while the sibling path succeeds:
        // PARTIAL_FAILURE with per-path results. The link target lives in
        // the temp dir, outside the repository.
        let outside = tmp.0.join("outside-target.txt");
        write_file(&outside, "outside\n");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, ws.join("escape-link")).unwrap();
        #[cfg(not(unix))]
        std::fs::write(ws.join("escape-link"), "not a symlink on this platform\n").unwrap();
        let state = state_of(&rt).await;
        let token = state["stateToken"].as_str().unwrap().to_owned();
        let (_, planned) = rt
            .dispatch(
                "git.plan_restore",
                json!({
                    "projectId": "p-git",
                    "action": "deleteUntracked",
                    "paths": [
                        {"encoding": "utf8", "value": "untracked.txt"},
                        {"encoding": "utf8", "value": "escape-link"},
                    ],
                    "expectedStateToken": token,
                }),
                None,
                None,
            )
            .await
            .unwrap();
        let plan = &planned["plan"];
        let plan_id = plan["planId"].as_str().unwrap().to_owned();
        let plan_revision = plan["planRevision"].as_str().unwrap().to_owned();
        let err = rt
            .dispatch(
                "git.restore",
                json!({
                    "projectId": "p-git",
                    "expectedStateToken": token,
                    "planId": plan_id,
                    "expectedPlanRevision": plan_revision,
                }),
                Some("k-restore-partial"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(dispatch_err_code(&err), CODE_PARTIAL_FAILURE);
        // The non-escaping path was deleted; the escape target survives.
        assert!(!ws.join("untracked.txt").exists());
        assert!(outside.exists(), "the symlink escape target must survive");

        // A stale plan (the state moved on) is rejected up front.
        let err = rt
            .dispatch(
                "git.restore",
                json!({
                    "projectId": "p-git",
                    "expectedStateToken": token,
                    "planId": plan_id,
                    "expectedPlanRevision": plan_revision,
                }),
                Some("k-restore-stale"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(dispatch_err_code(&err), CODE_REVISION_CONFLICT);
    }

    #[tokio::test]
    async fn push_remote_cas_rejects_stale_expectations() {
        if !git_present() {
            eprintln!("skipping: git not available");
            return;
        }
        let (rt, _iid, ws, tmp) = setup_repo("git-push").await;
        // A local bare repository acts as the remote (zero-network fixture).
        let bare = tmp.0.join("bare.git");
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q", "--bare", "-b", "main"])
                .arg(&bare)
                .status()
                .expect("git init --bare")
                .success(),
            "bare repository init failed"
        );
        git_sync(
            &ws,
            &["remote", "add", "origin", bare.to_str().expect("utf8 path")],
        );
        set_project_identity(&rt, "").await;
        write_file(&ws.join("a.txt"), "a\n");
        let state = state_of(&rt).await;
        let token = state["stateToken"].as_str().unwrap().to_owned();
        rt.dispatch(
            "git.commit",
            json!({
                "projectId": "p-git",
                "expectedStateToken": token,
                "selection": {"mode": "all"},
                "message": "Initial commit",
                "initial": true,
            }),
            Some("k-push-init"),
            None,
        )
        .await
        .unwrap();
        let state = state_of(&rt).await;
        let token = state["stateToken"].as_str().unwrap().to_owned();
        let head = state["headOid"].as_str().unwrap().to_owned();

        // expectedRemoteOid="" — the destination must not exist yet.
        rt.dispatch(
            "git.push",
            json!({
                "projectId": "p-git",
                "expectedStateToken": token,
                "remoteName": "origin",
                "sourceRef": "main",
                "destinationRef": "main",
                "expectedRemoteOid": "",
            }),
            Some("k-push-create"),
            None,
        )
        .await
        .unwrap();

        // The push created refs/remotes/origin/main (git updates the
        // remote-tracking ref), and the state token is content-sensitive to
        // remote refs — refresh it so the following stale-remote-expectation
        // pushes reach the remote CAS check instead of failing the local
        // expectedStateToken gate with REVISION_CONFLICT.
        let state = state_of(&rt).await;
        let token = state["stateToken"].as_str().unwrap().to_owned();

        // Same expectation now conflicts (the remote ref exists).
        let err = rt
            .dispatch(
                "git.push",
                json!({
                    "projectId": "p-git",
                    "expectedStateToken": token,
                    "remoteName": "origin",
                    "sourceRef": "main",
                    "destinationRef": "main",
                    "expectedRemoteOid": "",
                }),
                Some("k-push-stale"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(dispatch_err_code(&err), CODE_REMOTE_REF_CONFLICT);

        // A stale concrete OID also conflicts.
        let err = rt
            .dispatch(
                "git.push",
                json!({
                    "projectId": "p-git",
                    "expectedStateToken": token,
                    "remoteName": "origin",
                    "sourceRef": "main",
                    "destinationRef": "main",
                    "expectedRemoteOid": "1111111111111111111111111111111111111111",
                }),
                Some("k-push-stale-oid"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(dispatch_err_code(&err), CODE_REMOTE_REF_CONFLICT);

        // The correct remote OID succeeds (idempotent re-push).
        rt.dispatch(
            "git.push",
            json!({
                "projectId": "p-git",
                "expectedStateToken": token,
                "remoteName": "origin",
                "sourceRef": "main",
                "destinationRef": "main",
                "expectedRemoteOid": head,
            }),
            Some("k-push-ok"),
            None,
        )
        .await
        .unwrap();

        // publish reports the upstream configuration as its second segment.
        rt.dispatch(
            "git.publish",
            json!({
                "projectId": "p-git",
                "expectedStateToken": token,
                "remoteName": "origin",
                "branchName": "main",
                "expectedRemoteOid": head,
            }),
            Some("k-publish"),
            None,
        )
        .await
        .unwrap();
        let status = git_sync_status(&ws, &["config", "--get", "branch.main.remote"]);
        assert_eq!(status.trim(), "origin");
    }

    fn git_sync_status(cwd: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git output");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    #[tokio::test]
    async fn suggest_message_never_fakes_success() {
        if !git_present() {
            eprintln!("skipping: git not available");
            return;
        }
        let (rt, _iid, ws, _tmp) = setup_repo("git-suggest").await;
        write_file(&ws.join("hello.txt"), "hello\n");
        let state = state_of(&rt).await;
        let token = state["stateToken"].as_str().unwrap().to_owned();
        let err = rt
            .dispatch(
                "git.suggest_message",
                json!({
                    "projectId": "p-git",
                    "expectedStateToken": token,
                    "selection": {"mode": "all"},
                }),
                Some("k-suggest"),
                None,
            )
            .await
            .unwrap_err();
        // The model seam is absent in this build; the contract forbids an
        // empty-string fake success (TS git.js:998 fallback removed).
        assert_eq!(dispatch_err_code(&err), CODE_MODEL_UNAVAILABLE);
    }

    #[tokio::test]
    async fn identity_revision_cas_and_clear() {
        if !git_present() {
            eprintln!("skipping: git not available");
            return;
        }
        let (rt, _iid, _ws, _tmp) = setup_repo("git-identity").await;
        // Fresh state: no project row.
        let (outcome, got) = rt
            .dispatch(
                "git_identity.get",
                json!({"projectId": "p-git"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome, ResponseOutcome::Succeeded);
        // Host git config may provide a system identity; both unset and
        // system are acceptable here — the project row must not exist yet.
        assert_ne!(got["source"], json!("project"));
        agentcpm_pd_protocol::registry::OperationRegistry::global()
            .validate_result("git_identity.get", &got)
            .expect("git_identity.get result passes the contract schema");

        let set1 = set_project_identity(&rt, "").await;
        let revision = set1["revision"].as_str().unwrap().to_owned();
        assert!(!revision.is_empty());

        // Old revision no longer matches.
        let err = rt
            .dispatch(
                "git_identity.set",
                json!({
                    "scope": "project",
                    "projectId": "p-git",
                    "name": "Other",
                    "email": "other@example.com",
                    "expectedRevision": "",
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(dispatch_err_code(&err), CODE_REVISION_CONFLICT);

        let got = rt
            .dispatch(
                "git_identity.get",
                json!({"projectId": "p-git"}),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        assert_eq!(got["source"], json!("project"));
        assert_eq!(got["name"], json!("Test User"));

        // Invalid identity values are rejected without modification.
        let err = rt
            .dispatch(
                "git_identity.set",
                json!({
                    "scope": "project",
                    "projectId": "p-git",
                    "name": "Other",
                    "email": "not-an-email",
                    "expectedRevision": revision,
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(dispatch_err_code(&err), CODE_INVALID_IDENTITY);

        // Clear with the matching revision.
        rt.dispatch(
            "git_identity.set",
            json!({
                "scope": "project",
                "projectId": "p-git",
                "clear": true,
                "expectedRevision": revision,
            }),
            None,
            None,
        )
        .await
        .unwrap();
        let got = rt
            .dispatch(
                "git_identity.get",
                json!({"projectId": "p-git"}),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        assert_ne!(got["source"], json!("project"));
    }

    /// Reads one published inspect artifact back from the instance home.
    fn read_artifact_payload(tmp: &TempDir, artifact_id: &str) -> Value {
        let path = tmp
            .0
            .join("pd-home")
            .join("v1")
            .join("artifacts")
            .join(artifact_id);
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("read artifact {artifact_id} at {}: {e}", path.display()));
        serde_json::from_slice(&bytes).expect("artifact payload is JSON")
    }

    #[tokio::test]
    async fn inspect_diff_on_unborn_head_reports_changeset_not_invalid_ref() {
        if !git_present() {
            eprintln!("skipping: git not available");
            return;
        }
        let (rt, _iid, ws, tmp) = setup_repo("git-unborn-diff").await;
        // Staged with a further unstaged modification, plus an untracked
        // file: the unborn changeset must merge the staged/unstaged diffs
        // (unstaged preferred per path, git.js:409-424) and synthesize the
        // untracked addition — never INVALID_REF.
        write_file(&ws.join("a.txt"), "line1\nline2\n");
        git_sync(&ws, &["add", "a.txt"]);
        write_file(&ws.join("a.txt"), "line1\nline2 modified\n");
        write_file(&ws.join("u.txt"), "untracked\n");
        let (outcome, result) = rt
            .dispatch(
                "git.inspect",
                json!({"projectId": "p-git", "view": "diff"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome, ResponseOutcome::Succeeded);
        assert_eq!(result["dataSchemaId"], json!("git.diff.v1"));
        let artifact_id = result["dataArtifact"]["artifactId"].as_str().unwrap();
        let payload = read_artifact_payload(&tmp, artifact_id);
        agentcpm_pd_protocol::schema_validator::validate_against_schema(
            &payload,
            &agentcpm_pd_protocol::registry::OperationRegistry::global()
                .public_contract()["artifactSchemas"]["git.diff.v1"],
        )
        .unwrap_or_else(|errors| panic!("unborn diff payload violates the schema: {errors:?}"));
        let files = payload["files"].as_array().expect("files");
        let find = |name: &str| {
            files
                .iter()
                .find(|f| f["newPath"]["value"] == name)
                .unwrap_or_else(|| panic!("file {name} missing from unborn diff: {files:?}"))
                .clone()
        };
        let a = find("a.txt");
        assert_eq!(
            a["change"],
            json!("modified"),
            "the unstaged delta wins over the staged addition: {a:?}"
        );
        let lines = a["hunks"][0]["lines"].as_array().unwrap();
        assert_eq!(lines[1]["kind"], json!("deleted"));
        assert_eq!(
            lines[1]["bytesBase64"],
            json!(base64::engine::general_purpose::STANDARD.encode(b"line2"))
        );
        assert_eq!(lines[2]["kind"], json!("added"));
        let u = find("u.txt");
        assert_eq!(u["change"], json!("added"));
        assert!(!u["hunks"].as_array().unwrap().is_empty());

        // Path-filtered unborn diff follows the same baseline two-step for
        // the named file only.
        let (outcome, result) = rt
            .dispatch(
                "git.inspect",
                json!({
                    "projectId": "p-git",
                    "view": "diff",
                    "path": {"encoding": "utf8", "value": "a.txt"},
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome, ResponseOutcome::Succeeded);
        let payload =
            read_artifact_payload(&tmp, result["dataArtifact"]["artifactId"].as_str().unwrap());
        let files = payload["files"].as_array().expect("files");
        assert_eq!(files.len(), 1, "path filter narrows to one file: {files:?}");
        assert_eq!(files[0]["newPath"]["value"], json!("a.txt"));
        assert_eq!(files[0]["change"], json!("modified"));
    }

    #[tokio::test]
    async fn inspect_file_with_diff_on_unchanged_file_emits_schema_legal_diff() {
        if !git_present() {
            eprintln!("skipping: git not available");
            return;
        }
        let (rt, _iid, ws, tmp) = setup_repo("git-fwd-clean").await;
        set_project_identity(&rt, "").await;
        write_file(&ws.join("clean.txt"), "committed\n");
        let state = state_of(&rt).await;
        let token = state["stateToken"].as_str().unwrap().to_owned();
        rt.dispatch(
            "git.commit",
            json!({
                "projectId": "p-git",
                "expectedStateToken": token,
                "selection": {"mode": "all"},
                "message": "seed",
                "initial": true,
            }),
            Some("k-fwd-clean-seed"),
            None,
        )
        .await
        .unwrap();
        // No further changes: the artifact's `diff` must be a FileDiff with
        // no hunks (the schema forbids null), never `diff: null`.
        let (outcome, result) = rt
            .dispatch(
                "git.inspect",
                json!({
                    "projectId": "p-git",
                    "view": "fileWithDiff",
                    "path": {"encoding": "utf8", "value": "clean.txt"},
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome, ResponseOutcome::Succeeded);
        assert_eq!(result["dataSchemaId"], json!("git.fileWithDiff.v1"));
        let payload =
            read_artifact_payload(&tmp, result["dataArtifact"]["artifactId"].as_str().unwrap());
        agentcpm_pd_protocol::schema_validator::validate_against_schema(
            &payload,
            &agentcpm_pd_protocol::registry::OperationRegistry::global()
                .public_contract()["artifactSchemas"]["git.fileWithDiff.v1"],
        )
        .unwrap_or_else(|errors| {
            panic!("clean fileWithDiff payload violates the schema: {errors:?}")
        });
        assert!(
            payload["diff"].is_object(),
            "diff must be a FileDiff, got: {}",
            payload["diff"]
        );
        assert_eq!(payload["diff"]["change"], json!("modified"));
        assert_eq!(payload["diff"]["hunks"].as_array().map(Vec::len), Some(0));
    }

    #[tokio::test]
    async fn failed_mutation_same_key_retry_reports_terminal_outcome() {
        if !git_present() {
            eprintln!("skipping: git not available");
            return;
        }
        let (rt, _iid, ws, _tmp) = setup_repo("git-idem-fail").await;
        set_project_identity(&rt, "").await;
        write_file(&ws.join("hello.txt"), "hello\n");
        let state = state_of(&rt).await;
        let token = state["stateToken"].as_str().unwrap().to_owned();
        rt.dispatch(
            "git.commit",
            json!({
                "projectId": "p-git",
                "expectedStateToken": token,
                "selection": {"mode": "all"},
                "message": "seed",
                "initial": true,
            }),
            Some("k-idem-seed"),
            None,
        )
        .await
        .unwrap();

        // A deterministic failure under an idempotency key: nothing new to
        // commit with selection.mode=index.
        let state = state_of(&rt).await;
        let token = state["stateToken"].as_str().unwrap().to_owned();
        let commit_input = json!({
            "projectId": "p-git",
            "expectedStateToken": token,
            "selection": {"mode": "index"},
            "message": "empty",
        });
        let err = rt
            .dispatch(
                "git.commit",
                commit_input.clone(),
                Some("k-idem-fail"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(dispatch_err_code(&err), CODE_NOTHING_TO_COMMIT);

        // Same key + same input replays the deterministic failure — never a
        // deduplicated `running` receipt that would never resolve.
        let err = rt
            .dispatch(
                "git.commit",
                commit_input.clone(),
                Some("k-idem-fail"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(dispatch_err_code(&err), CODE_NOTHING_TO_COMMIT);

        // The stored row is a terminal rejection whose receipt says failed.
        {
            let store = rt.store();
            let conn = store.conn.lock().expect("sqlite mutex");
            let (request_state, receipt_json, rejection_code): (
                String,
                Option<String>,
                Option<String>,
            ) = conn
                .query_row(
                    "SELECT request_state, receipt_json, rejection_code FROM idempotency_records WHERE idempotency_key = 'k-idem-fail' AND method_id = 'git.commit'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();
            assert_eq!(request_state, "notAccepted");
            assert_eq!(
                rejection_code.as_deref(),
                Some("NOTHING_TO_COMMIT"),
                "the rejection code must land: {rejection_code:?}"
            );
            let receipt: Value =
                serde_json::from_str(&receipt_json.expect("receipt kept")).unwrap();
            assert_eq!(receipt["state"], json!("failed"), "receipt: {receipt:?}");
            let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
            let (op_state, op_effect): (String, String) = conn
                .query_row(
                    "SELECT state, effect_status FROM git_operations WHERE operation_id = ?1",
                    params![operation_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(op_state, "failed");
            assert_eq!(op_effect, "known");
        }

        // A fresh key re-executes (the contract's
        // correctRejectedInputWithNewKey) and gets its own rejection row.
        let err = rt
            .dispatch("git.commit", commit_input, Some("k-idem-fail-2"), None)
            .await
            .unwrap_err();
        assert_eq!(dispatch_err_code(&err), CODE_NOTHING_TO_COMMIT);
        {
            let store = rt.store();
            let conn = store.conn.lock().expect("sqlite mutex");
            let (request_state, rejection_code): (String, Option<String>) = conn
                .query_row(
                    "SELECT request_state, rejection_code FROM idempotency_records WHERE idempotency_key = 'k-idem-fail-2' AND method_id = 'git.commit'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(request_state, "notAccepted");
            assert_eq!(rejection_code.as_deref(), Some("NOTHING_TO_COMMIT"));
        }
    }
}
