//! `installation.*` — the diagnostics & release domain (t021, 009).
//!
//! Contract split (the operation registry is the authority):
//!
//! * `installation.inspect` — `execution=local`, CLI-owned (the
//!   `local_ops::installation` handler in the CLI crate); this domain
//!   registers only its availability row so `system.capabilities` stays
//!   truthful.
//! * `installation.check` / `installation.apply` / `installation.restart` /
//!   `installation.diagnose` — async `idempotentSubmit` operations accepted
//!   through the shared `v1_domain_operations` seam (the schedules accept
//!   machinery) and completed by spawned tasks.
//! * `installation.control_update` — sync, the control plane over a live
//!   apply: pause/resume/cancel follow 009 §3.4's stage matrix; an action
//!   unsupported in the current phase answers `INVALID_STATE`, an unknown
//!   operationId `NOT_FOUND`. Like `uploads.cancel`, a repeated control
//!   submit converges on the durable phase rather than replaying a receipt
//!   (a recorded difference).
//!
//! The release artifact is "products built from this source tree plus their
//! manifest and verification rules" — no prebuilt binaries are ever fetched
//! by the build ([`release`] owns the package model). The install state
//! lives under the install root (009 §3.1): `generations/<buildId>`, the
//! atomic `current` symlink, `staging/`, and `state/` with the mkdir lock
//! and `last-update.json`. The data root holds only the channel
//! configuration and the restart marker.
//!
//! Restart (009 §7): the operation drains active runs (deadline-honest:
//! `DEADLINE_EXCEEDED` leaves everything running), spawns a successor
//! process from `<installRoot>/current/bin/agentcpm-pd` carrying the
//! [`layout::RESTART_HANDOFF_ENV`] pid, then SIGTERMs itself. The successor
//! waits for the predecessor, and its startup hook
//! ([`recover_pending_restart`]) completes the operation — `healthy` only
//! when the install root's current target really matches the requested
//! buildId.

pub mod channel;
pub mod layout;
pub mod release;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_IDEMPOTENCY_CONFLICT, CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_UNAVAILABLE,
    ContractError, EffectStatus, RetrySpec,
};
use serde_json::{Value, json};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::schedules::{
    DDL_DOMAIN_OPERATIONS, DomainDb, DomainOperationLookup, accept_domain_operation,
    finish_domain_operation, lookup_domain_operation, mark_domain_operation_running,
};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};

/// How long `installation.check` waits for the channel's manifest (a dead
/// network must report, never hang).
const CHECK_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// The apply download deadline (the baseline's one-hour apply cap,
/// `webUpdateCommand.js:77`).
const DOWNLOAD_DEADLINE: Duration = Duration::from_secs(3600);

/// `apply --restart-policy=whenIdle`'s honest wait bound before
/// `DEADLINE_EXCEEDED` (the contract input has no timeout field).
const APPLY_WHEN_IDLE_BOUND: Duration = Duration::from_secs(600);

/// Generations kept after an apply: the current one plus one for rollback
/// (009 §3.1 默认保留最近 2 代).
const KEEP_GENERATIONS: usize = 2;

/// Grace between the accepted restart receipt going out and the self
/// SIGTERM (the in-flight response gets a chance to flush).
const RESTART_RECEIPT_GRACE: Duration = Duration::from_millis(400);

/// The successor's wait bound for the predecessor pid to exit.
const RESTART_HANDOFF_BOUND: Duration = Duration::from_secs(30);

/// The v1 state-db schema this generation understands (`PRAGMA
/// user_version`); a database above it is newer than this binary
/// (009 §5.4).
const SUPPORTED_V1_SCHEMA: i64 = 1;

/// Free-space warning threshold for diagnose's `storage.freeSpace`.
const FREE_SPACE_WARNING_BYTES: u64 = 1024 * 1024 * 1024;

pub(super) fn register(reg: &mut DomainRegistry) {
    // execution=local: served by the CLI's local_ops table, never /v1/ws.
    reg.available("installation.inspect");
    reg.handler("installation.check", |rt, input, key, _rev| {
        Box::pin(installation_check(rt, input, key))
    });
    reg.handler("installation.apply", |rt, input, key, _rev| {
        Box::pin(installation_apply(rt, input, key))
    });
    reg.handler("installation.control_update", |rt, input, _key, _rev| {
        Box::pin(installation_control_update(rt, input))
    });
    reg.handler("installation.restart", |rt, input, key, _rev| {
        Box::pin(installation_restart(rt, input, key))
    });
    reg.handler("installation.diagnose", |rt, input, key, _rev| {
        Box::pin(installation_diagnose(rt, input, key))
    });
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

fn invalid_arg(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn invalid_state(message: impl Into<String>) -> ContractError {
    ContractError::new(
        "INVALID_STATE",
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn io_error(message: impl Into<String>) -> ContractError {
    ContractError::new(
        "IO_ERROR",
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn unavailable(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn str_field(input: &Value, name: &str) -> Result<String, ContractError> {
    input
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| invalid_arg(format!("`{name}` is required")))
}

// ---------------------------------------------------------------------------
// The per-dataRoot domain singleton
// ---------------------------------------------------------------------------

/// One live apply operation: its phase (the control plane's view) and the
/// download control handle.
struct UpdateState {
    phase: Mutex<UpdatePhase>,
    control: Arc<channel::DownloadControl>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpdatePhase {
    Resolving,
    Downloading,
    Verifying,
    Committing,
    Switching,
    Finalizing,
}

impl UpdatePhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Resolving => "resolving",
            Self::Downloading => "downloading",
            Self::Verifying => "verifying",
            Self::Committing => "committing",
            Self::Switching => "switching",
            Self::Finalizing => "finalizing",
        }
    }
}

/// The 009 §3.4 stage matrix: pause/resume only while downloading, cancel
/// until the verification starts; from verification on, nothing is
/// cancellable and nothing pretends to be.
fn allowed_actions(phase: UpdatePhase) -> Vec<&'static str> {
    match phase {
        UpdatePhase::Resolving => vec!["cancel"],
        UpdatePhase::Downloading => vec!["pause", "resume", "cancel"],
        _ => Vec::new(),
    }
}

pub(crate) struct InstallationDomain {
    rt: Arc<PublicV1Runtime>,
    db: DomainDb,
    /// Live apply operations by operation id (the control plane).
    updates: Mutex<HashMap<String, Arc<UpdateState>>>,
}

fn domains_registry() -> &'static Mutex<HashMap<PathBuf, Weak<InstallationDomain>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<InstallationDomain>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn domain_for(rt: &Arc<PublicV1Runtime>) -> Result<Arc<InstallationDomain>, ContractError> {
    let home = rt.store().home().to_path_buf();
    let mut registry = domains_registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = registry.get(&home).and_then(Weak::upgrade) {
        return Ok(existing);
    }
    let domain = Arc::new(InstallationDomain {
        rt: rt.clone(),
        db: DomainDb::open(&home, &[DDL_DOMAIN_OPERATIONS])?,
        updates: Mutex::new(HashMap::new()),
    });
    registry.insert(home, Arc::downgrade(&domain));
    Ok(domain)
}

/// Opens a private read-side connection to the shared v1 state database
/// (the bootstrap `non_terminal_run_ids_readonly` pattern).
fn open_state_db_readonly(home: &Path) -> Result<rusqlite::Connection, ContractError> {
    let path = home.join("v1").join("state.sqlite");
    if !path.exists() {
        return Err(unavailable(format!(
            "the state database {} does not exist",
            path.display()
        )));
    }
    let conn = rusqlite::Connection::open(&path)
        .map_err(|error| unavailable(format!("open {}: {error}", path.display())))?;
    let _ = conn.busy_timeout(Duration::from_millis(5000));
    Ok(conn)
}

/// Active (running / waiting / cancelling) run count — the drain condition.
fn active_run_count(home: &Path) -> Result<u64, ContractError> {
    let conn = open_state_db_readonly(home)?;
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM runs WHERE state IN ('running', 'waiting', 'cancelling')",
            [],
            |row| row.get(0),
        )
        .map_err(|error| unavailable(format!("count active runs: {error}")))?;
    Ok(count.max(0) as u64)
}

// ---------------------------------------------------------------------------
// installation.check (009 §3.2)
// ---------------------------------------------------------------------------

async fn installation_check(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let domain = domain_for(&rt)?;
    let channel_name = input
        .get("channel")
        .and_then(Value::as_str)
        .unwrap_or("stable")
        .to_owned();
    let home = domain.rt.store().home().to_path_buf();
    let config = layout::load_channel_config(&home).map_err(invalid_arg)?;
    let channel_url = config
        .url_for(&channel_name)
        .map(str::to_owned)
        .ok_or_else(|| {
            invalid_arg(format!(
                "channel `{channel_name}` is not configured; record its URL in {}",
                layout::channel_config_path(&home).display()
            ))
        })?;

    let operation = accept_domain_operation(
        &domain.db,
        "installation.check",
        &input,
        key.as_deref(),
        || format!("op-{}", domain.rt.store().next_id()),
        || Ok(()),
    )?;
    let runner = Arc::clone(&domain);
    let operation_id = operation.operation_id.clone();
    tokio::spawn(async move {
        let _ = mark_domain_operation_running(&runner.db, &operation_id, "");
        let root = match layout::resolve_install_root(None) {
            Ok(root) => root,
            Err(message) => {
                let _ = finish_domain_operation(
                    &runner.db,
                    &operation_id,
                    "",
                    "failed",
                    &json!({"error": {"code": "UNAVAILABLE", "message": message, "effect": "not_applied"}}),
                );
                return;
            }
        };
        let outcome = perform_check(&root, &channel_url).await;
        let (state, completion) = match outcome {
            Ok(result) => ("succeeded", result),
            Err((code, message)) => (
                "failed",
                json!({"error": {"code": code, "message": message, "effect": "not_applied"}}),
            ),
        };
        let _ = finish_domain_operation(&runner.db, &operation_id, "", state, &completion);
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

/// Runs one check: read the local identity, fetch + validate the channel
/// manifest, compare — never installing, never restarting, never fetching
/// the package body (009 §3.2).
async fn perform_check(root: &Path, channel_url: &str) -> Result<Value, (String, String)> {
    let identity = layout::installation_identity_at(root);
    let bytes = channel::fetch_manifest(channel_url, CHECK_FETCH_TIMEOUT)
        .await
        .map_err(|error| match error {
            channel::ChannelError::Network(message) => ("UPSTREAM_ERROR".to_owned(), message),
            channel::ChannelError::Integrity(message) => ("INTEGRITY_ERROR".to_owned(), message),
            channel::ChannelError::Cancelled => (
                "UPSTREAM_ERROR".to_owned(),
                "manifest fetch cancelled".to_owned(),
            ),
        })?;
    let manifest = release::parse_release_manifest(&bytes)
        .map_err(|message| ("INTEGRITY_ERROR".to_owned(), message))?;
    let (platform, arch) = release::current_platform_pair();
    let update_available = manifest.build_id != identity.build_id;
    let releases: Vec<Value> = if update_available {
        manifest
            .assets
            .iter()
            .filter(|asset| asset.matches(platform, arch))
            .map(|_asset| {
                json!({
                    "releaseId": manifest.tag,
                    "version": manifest.version,
                    "platform": format!("{platform}/{arch}"),
                    "verified": true,
                    "requiresRestart": true,
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    Ok(json!({
        "currentVersion": identity.version,
        "currentBuildId": identity.build_id,
        "updateAvailable": update_available,
        "releases": releases,
    }))
}

// ---------------------------------------------------------------------------
// installation.apply (009 §3.3) — the staged install transaction
// ---------------------------------------------------------------------------

async fn installation_apply(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let domain = domain_for(&rt)?;
    let release_id = str_field(&input, "releaseId")?;
    let expected_build_id = str_field(&input, "expectedBuildId")?;
    let restart_policy = input
        .get("restartPolicy")
        .and_then(Value::as_str)
        .unwrap_or("defer")
        .to_owned();

    // Idempotency first: a replay of an applied update answers with the
    // stored receipt even while a *different* update holds the lock.
    match lookup_domain_operation(&domain.db, "installation.apply", &input, key.as_deref())? {
        DomainOperationLookup::Replay(receipt) => {
            return Ok((ResponseOutcome::Accepted, receipt));
        }
        DomainOperationLookup::Conflict => {
            return Err(ContractError::new(
                CODE_IDEMPOTENCY_CONFLICT,
                "idempotencyKey already used for installation.apply with different input"
                    .to_owned(),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
            .into());
        }
        DomainOperationLookup::Rejected(code) => {
            return Err(ContractError::new(
                code,
                "installation.apply request previously rejected definitively".to_owned(),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
            .into());
        }
        DomainOperationLookup::Fresh => {}
    }

    // Stage 1: the mkdir lock. EEXIST is lockBusy (UNAVAILABLE, retryable —
    // the key is deliberately not pinned to the failure).
    let root = layout::resolve_install_root(None).map_err(unavailable)?;
    let lock = match layout::UpdateLock::acquire(&root) {
        Ok(lock) => lock,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(ContractError::new(
                CODE_UNAVAILABLE,
                format!(
                    "another update is in flight (lockBusy, active operation: {:?}); if no \
                     update is running, a previous one was interrupted — run `installation \
                     diagnose` and clear {} manually",
                    layout::active_update_operation_id(&root),
                    layout::update_lock_dir(&root).display()
                ),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            )
            .into());
        }
        Err(error) => {
            return Err(unavailable(format!("acquire the update lock: {error}")).into());
        }
    };

    // Data-format precheck (009 §5.2): a state database written by a newer
    // product generation must never be silently downgraded onto.
    {
        let home = domain.rt.store().home().to_path_buf();
        if let Ok(conn) = open_state_db_readonly(&home) {
            let user_version: i64 = conn
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .unwrap_or(0);
            if user_version > SUPPORTED_V1_SCHEMA {
                return Err(ContractError::new(
                    "INVALID_STATE",
                    format!(
                        "the v1 state database schema (user_version {user_version}) is newer \
                         than this build supports ({SUPPORTED_V1_SCHEMA}); rolling back would \
                         lose data — fix forward or restore a backup (t020)"
                    ),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )
                .into());
            }
        }
    }

    let operation = accept_domain_operation(
        &domain.db,
        "installation.apply",
        &input,
        key.as_deref(),
        || format!("op-{}", domain.rt.store().next_id()),
        || Ok(()),
    )?;
    lock.set_owner(&operation.operation_id);

    let runner = Arc::clone(&domain);
    let operation_id = operation.operation_id.clone();
    tokio::spawn(async move {
        let _ = mark_domain_operation_running(&runner.db, &operation_id, "");
        let state = Arc::new(UpdateState {
            phase: Mutex::new(UpdatePhase::Resolving),
            control: channel::DownloadControl::new(),
        });
        runner
            .updates
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(operation_id.clone(), Arc::clone(&state));
        let outcome = run_apply(
            &runner,
            &state,
            &operation_id,
            &root,
            &release_id,
            &expected_build_id,
            &restart_policy,
        )
        .await;
        runner
            .updates
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&operation_id);
        let (record_state, completion, handoff) = match outcome {
            ApplyOutcome::Succeeded {
                completion,
                handoff,
            } => ("succeeded", completion, handoff),
            ApplyOutcome::Failed {
                code,
                message,
                effect,
            } => (
                "failed",
                json!({"error": {"code": code, "message": message, "effect": effect}}),
                false,
            ),
            ApplyOutcome::Cancelled => (
                "cancelled",
                json!({"cancelled": true, "releaseId": release_id}),
                false,
            ),
        };
        let _ = finish_domain_operation(&runner.db, &operation_id, "", record_state, &completion);
        drop(lock);
        if handoff {
            // The completion is durable now; terminate for the successor.
            finish_restart_handoff().await;
        }
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

#[derive(Debug)]
enum ApplyOutcome {
    /// The install transaction completed. `handoff` marks that a restart
    /// handoff was initiated (`whenIdle`): the caller must make the
    /// completion durable and only then terminate the process.
    Succeeded {
        completion: Value,
        handoff: bool,
    },
    Failed {
        code: String,
        message: String,
        effect: &'static str,
    },
    Cancelled,
}

fn apply_failed(code: &str, message: impl Into<String>) -> ApplyOutcome {
    ApplyOutcome::Failed {
        code: code.to_owned(),
        message: message.into(),
        effect: "not_applied",
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_apply(
    domain: &Arc<InstallationDomain>,
    state: &Arc<UpdateState>,
    operation_id: &str,
    root: &Path,
    release_id: &str,
    expected_build_id: &str,
    restart_policy: &str,
) -> ApplyOutcome {
    fn set_phase(state: &Arc<UpdateState>, phase: UpdatePhase) {
        *state.phase.lock().unwrap_or_else(|e| e.into_inner()) = phase;
    }

    let home = domain.rt.store().home().to_path_buf();

    // Stage 2: resolve releaseId → manifest. Local generations first (the
    // rollback path needs no network), then the channel's latest manifest.
    set_phase(state, UpdatePhase::Resolving);
    let (manifest, channel_url) = {
        let local = layout::list_generations(root)
            .into_iter()
            .find(|entry| entry.manifest.tag == release_id);
        match local {
            Some(entry) => (entry.manifest, None),
            None => {
                let config = match layout::load_channel_config(&home) {
                    Ok(config) => config,
                    Err(message) => return apply_failed("INVALID_ARGUMENT", message),
                };
                let Some(channel_url) = config.url_for("stable").map(str::to_owned) else {
                    return apply_failed(
                        "INVALID_ARGUMENT",
                        format!(
                            "releaseId `{release_id}` not found in the local generations and no \
                             channel is configured"
                        ),
                    );
                };
                let fetched = tokio::time::timeout(
                    CHECK_FETCH_TIMEOUT,
                    channel::fetch_manifest(&channel_url, CHECK_FETCH_TIMEOUT),
                )
                .await;
                let bytes = match fetched {
                    Ok(Ok(bytes)) => bytes,
                    Ok(Err(channel::ChannelError::Network(message))) => {
                        return apply_failed("UNAVAILABLE", message);
                    }
                    Ok(Err(channel::ChannelError::Integrity(message))) => {
                        return apply_failed("INTEGRITY_ERROR", message);
                    }
                    Ok(Err(channel::ChannelError::Cancelled)) => return ApplyOutcome::Cancelled,
                    Err(_) => {
                        return apply_failed("UNAVAILABLE", "the channel manifest fetch timed out");
                    }
                };
                match release::parse_release_manifest(&bytes) {
                    Ok(manifest) if manifest.tag == release_id => (manifest, Some(channel_url)),
                    Ok(manifest) => {
                        return apply_failed(
                            "INVALID_ARGUMENT",
                            format!(
                                "releaseId `{release_id}` not found (the channel serves `{}`, \
                                 and the local generations do not contain it)",
                                manifest.tag
                            ),
                        );
                    }
                    Err(message) => return apply_failed("INTEGRITY_ERROR", message),
                }
            }
        }
    };
    if manifest.build_id != expected_build_id {
        return ApplyOutcome::Failed {
            code: "BUILD_CONFLICT".to_owned(),
            message: format!(
                "release `{}` resolves to buildId `{}` but `{expected_build_id}` was expected \
                 (the target changed — re-check and resubmit)",
                manifest.tag, manifest.build_id
            ),
            effect: "not_applied",
        };
    }

    // Stage 3–5: fetch, verify, stage, and commit — skipped when the
    // generation is already retained locally (the rollback/re-apply path).
    let staging_root = layout::staging_dir(root);
    let generation_path = layout::generation_dir(root, &manifest.build_id);
    if !generation_path.exists() {
        let (platform, arch) = release::current_platform_pair();
        let Some(asset) = manifest
            .assets
            .iter()
            .find(|asset| asset.matches(platform, arch))
        else {
            return ApplyOutcome::Failed {
                code: "PLATFORM_UNSUPPORTED".to_owned(),
                message: format!(
                    "release `{}` has no asset for {platform}/{arch}",
                    manifest.tag
                ),
                effect: "not_applied",
            };
        };
        let Some(channel_url) = channel_url.as_deref() else {
            return apply_failed(
                "INVALID_ARGUMENT",
                format!(
                    "releaseId `{release_id}` needs the channel but no URL was resolved for it"
                ),
            );
        };
        set_phase(state, UpdatePhase::Downloading);
        if let Err(error) = std::fs::create_dir_all(&staging_root) {
            return apply_failed("IO_ERROR", format!("prepare the staging dir: {error}"));
        }
        let part = staging_root.join(format!("{operation_id}.part"));
        let download = tokio::time::timeout(
            DOWNLOAD_DEADLINE,
            channel::download_asset(channel_url, asset, &part, Some(Arc::clone(&state.control))),
        )
        .await;
        let download = match download {
            Ok(result) => result,
            Err(_) => Err(channel::ChannelError::Network(
                "the download exceeded the one-hour deadline".to_owned(),
            )),
        };
        match download {
            Ok(_length) => {}
            Err(channel::ChannelError::Network(message)) => {
                let _ = std::fs::remove_file(&part);
                return apply_failed("UNAVAILABLE", message);
            }
            Err(channel::ChannelError::Integrity(message)) => {
                let _ = std::fs::remove_file(&part);
                return apply_failed("INTEGRITY_ERROR", message);
            }
            Err(channel::ChannelError::Cancelled) => {
                let _ = std::fs::remove_file(&part);
                return ApplyOutcome::Cancelled;
            }
        }

        // Stage 4: unpack into staging and verify the generation (the
        // "committed install" boundary — nothing is cancellable past here).
        set_phase(state, UpdatePhase::Verifying);
        let staged = staging_root.join(format!("gen-{operation_id}"));
        let _ = std::fs::remove_dir_all(&staged);
        if let Err(message) = release::unpack_tar_gz(&part, &staged) {
            let _ = std::fs::remove_file(&part);
            let _ = std::fs::remove_dir_all(&staged);
            return apply_failed("INTEGRITY_ERROR", message);
        }
        let _ = std::fs::remove_file(&part);
        if let Err(message) = release::verify_generation(&staged, &manifest) {
            let _ = std::fs::remove_dir_all(&staged);
            return apply_failed("INTEGRITY_ERROR", message);
        }

        // Stage 5: commit the generation directory (a full rename — the
        // running installation is untouched until the switch).
        set_phase(state, UpdatePhase::Committing);
        if let Err(error) = std::fs::create_dir_all(layout::generations_dir(root)) {
            let _ = std::fs::remove_dir_all(&staged);
            return apply_failed("IO_ERROR", format!("prepare the generations dir: {error}"));
        }
        if let Err(error) = std::fs::rename(&staged, &generation_path) {
            let _ = std::fs::remove_dir_all(&staged);
            return apply_failed("IO_ERROR", format!("commit the generation: {error}"));
        }
    }

    // Stage 6: the atomic switch (the tiny applied window; a failure here
    // leaves the previous installation fully intact).
    set_phase(state, UpdatePhase::Switching);
    let previous = layout::current_generation(root).map(|(build_id, _)| build_id);
    if let Err(message) = layout::switch_current(root, &manifest.build_id) {
        return ApplyOutcome::Failed {
            code: "IO_ERROR".to_owned(),
            message: format!("switch current: {message}"),
            effect: "not_applied",
        };
    }

    // Stage 7: finalize — last-update.json, retention, honest report.
    set_phase(state, UpdatePhase::Finalizing);
    let pruned = layout::prune_generations(root, KEEP_GENERATIONS);
    let retained: Vec<String> = layout::list_generations(root)
        .into_iter()
        .map(|entry| entry.build_id)
        .collect();
    let _ = layout::write_last_update(
        root,
        &layout::LastUpdateRecord {
            result: "succeeded".to_owned(),
            release_id: manifest.tag.clone(),
            previous_build_id: previous.clone(),
            new_build_id: manifest.build_id.clone(),
            retained_generations: retained.clone(),
            finished_at: chrono::Utc::now().to_rfc3339(),
        },
    );
    let recovery_available = retained
        .iter()
        .any(|build_id| build_id != &manifest.build_id);
    let completion = json!({
        "releaseId": manifest.tag,
        "buildId": manifest.build_id,
        "previousBuildId": previous,
        "restartRequired": restart_policy == "defer",
        "recoveryAvailable": recovery_available,
        "retainedGenerations": retained,
        "prunedGenerations": pruned,
    });

    if restart_policy != "whenIdle" {
        return ApplyOutcome::Succeeded {
            completion,
            handoff: false,
        };
    }

    // whenIdle (009 §3.3): wait for an idle service, then run the restart
    // drain flow. The wait is honest — a timeout fails the operation with
    // DEADLINE_EXCEEDED while reporting the install as already applied.
    let deadline = Instant::now() + APPLY_WHEN_IDLE_BOUND;
    loop {
        match active_run_count(&home) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => {
                return ApplyOutcome::Failed {
                    code: error.code,
                    message: error.message,
                    effect: "applied",
                };
            }
        }
        if Instant::now() >= deadline {
            return ApplyOutcome::Failed {
                code: "DEADLINE_EXCEEDED".to_owned(),
                message: format!(
                    "the new generation was installed (buildId `{}`) but the service never \
                     became idle within {}s; restart manually with `installation restart`",
                    manifest.build_id,
                    APPLY_WHEN_IDLE_BOUND.as_secs()
                ),
                effect: "applied",
            };
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    if let Err(error) = initiate_restart_handoff(
        domain,
        root,
        &manifest.build_id,
        "installation.apply:whenIdle",
        None,
    ) {
        return ApplyOutcome::Failed {
            code: error.code,
            message: error.message,
            effect: "applied",
        };
    }
    // The caller writes the completion durably first, then terminates; the
    // successor's recovery hook only clears the marker.
    ApplyOutcome::Succeeded {
        completion,
        handoff: true,
    }
}

// ---------------------------------------------------------------------------
// installation.control_update (009 §3.4)
// ---------------------------------------------------------------------------

async fn installation_control_update(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let domain = domain_for(&rt)?;
    let operation_id = str_field(&input, "operationId")?;
    let action = str_field(&input, "action")?;

    let live = domain
        .updates
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&operation_id)
        .cloned();
    if let Some(state) = live {
        let phase = *state.phase.lock().unwrap_or_else(|e| e.into_inner());
        let allowed = allowed_actions(phase);
        if !allowed.contains(&action.as_str()) {
            let supported = if allowed.is_empty() {
                "none".to_owned()
            } else {
                allowed.join(", ")
            };
            return Err(invalid_state(format!(
                "cannot `{action}` an update in phase `{}`; supported here: {supported}",
                phase.as_str()
            ))
            .into());
        }
        let (new_state, next_allowed): (String, Vec<String>) = match action.as_str() {
            "pause" => {
                state.control.pause();
                (
                    "paused".to_owned(),
                    vec!["resume".to_owned(), "cancel".to_owned()],
                )
            }
            "resume" => {
                state.control.resume();
                (
                    phase.as_str().to_owned(),
                    allowed
                        .into_iter()
                        .filter(|candidate| *candidate != "resume")
                        .map(str::to_owned)
                        .collect(),
                )
            }
            "cancel" => {
                state.control.cancel();
                ("cancelling".to_owned(), Vec::new())
            }
            other => return Err(invalid_arg(format!("unknown action `{other}`")).into()),
        };
        return Ok((
            ResponseOutcome::Succeeded,
            json!({
                "operationId": operation_id,
                "accepted": true,
                "state": new_state,
                "allowedActions": next_allowed,
            }),
        ));
    }

    // No live update: the operation record decides (NOT_FOUND vs a terminal
    // / foreign operation, which supports no control actions).
    let home = domain.rt.store().home().to_path_buf();
    let conn = open_state_db_readonly(&home)?;
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT method_id, state FROM v1_domain_operations WHERE operation_id = ?1",
            [&operation_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map(Some)
        .or_else(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
        .map_err(|error| unavailable(format!("look up the operation: {error}")))?;
    match row {
        None => Err(ContractError::new(
            CODE_NOT_FOUND,
            format!("operation `{operation_id}` not found"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
        .into()),
        Some((method_id, record_state)) => {
            if method_id != "installation.apply" {
                return Err(invalid_state(format!(
                    "operation `{operation_id}` is a `{method_id}` operation; control_update \
                     only drives installation.apply"
                ))
                .into());
            }
            Err(invalid_state(format!(
                "update `{operation_id}` is `{record_state}` and no longer controllable \
                 (control_update only drives an in-flight update)"
            ))
            .into())
        }
    }
}

// ---------------------------------------------------------------------------
// installation.restart (009 §7)
// ---------------------------------------------------------------------------

async fn installation_restart(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let domain = domain_for(&rt)?;
    let target = str_field(&input, "target")?;
    if target != "ownedService" {
        // 009 §7.1: the desktop host owns currentHost restarts; the CLI
        // scenario has no such host.
        return Err(unavailable(format!(
            "restart target `{target}` is not owned by this CLI scenario; only `ownedService` \
             is supported here"
        ))
        .into());
    }
    let expected_build_id = str_field(&input, "expectedBuildId")?;
    let drain_timeout_ms = input
        .get("drainTimeoutMs")
        .and_then(Value::as_u64)
        .unwrap_or(30_000);

    let operation = accept_domain_operation(
        &domain.db,
        "installation.restart",
        &input,
        key.as_deref(),
        || format!("op-{}", domain.rt.store().next_id()),
        || Ok(()),
    )?;
    let runner = Arc::clone(&domain);
    let operation_id = operation.operation_id.clone();
    tokio::spawn(async move {
        let _ = mark_domain_operation_running(&runner.db, &operation_id, "");
        let root = match layout::resolve_install_root(None) {
            Ok(root) => root,
            Err(message) => {
                let _ = finish_domain_operation(
                    &runner.db,
                    &operation_id,
                    "",
                    "failed",
                    &json!({"error": {"code": "UNAVAILABLE", "message": message, "effect": "not_applied"}}),
                );
                return;
            }
        };
        match run_restart(
            &runner,
            &operation_id,
            &root,
            &expected_build_id,
            drain_timeout_ms,
        )
        .await
        {
            RestartOutcome::Handoff => {
                // The accepted receipt is durable now; terminate for the
                // successor (009 §7: spawn the successor, then SIGTERM
                // ourselves — the successor's startup hook waits for this
                // pid to exit, claims the instance lock, and completes the
                // operation). Without this the predecessor keeps the
                // single-instance lock alive forever, the successor's
                // predecessor-wait times out, and the operation stays
                // `running` with nobody left to complete it.
                finish_restart_handoff().await;
            }
            RestartOutcome::Failed {
                code,
                message,
                effect,
            } => {
                let _ = finish_domain_operation(
                    &runner.db,
                    &operation_id,
                    "",
                    "failed",
                    &json!({"error": {"code": code, "message": message, "effect": effect}}),
                );
            }
        }
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

enum RestartOutcome {
    /// The handoff is staged: the successor process owns the operation's
    /// completion from here (009 §7).
    Handoff,
    Failed {
        code: String,
        message: String,
        effect: &'static str,
    },
}

/// Runs one restart: drain, then the handoff. A drain timeout leaves the
/// service and every run untouched.
async fn run_restart(
    domain: &Arc<InstallationDomain>,
    operation_id: &str,
    root: &Path,
    expected_build_id: &str,
    drain_timeout_ms: u64,
) -> RestartOutcome {
    let home = domain.rt.store().home().to_path_buf();

    // Drain: wait for active runs; a timeout leaves everything running
    // (deadline-honest, no forced stop — the operator cancels runs first).
    let deadline = Instant::now() + Duration::from_millis(drain_timeout_ms);
    loop {
        match active_run_count(&home) {
            Ok(0) => break,
            Ok(count) => {
                if Instant::now() >= deadline {
                    return RestartOutcome::Failed {
                        code: "DEADLINE_EXCEEDED".to_owned(),
                        message: format!(
                            "{count} active run(s) still running after {drain_timeout_ms}ms; the \
                             service was NOT restarted — cancel the runs or retry with a larger \
                             --drain-timeout-ms"
                        ),
                        effect: "not_applied",
                    };
                }
            }
            Err(error) => {
                return RestartOutcome::Failed {
                    code: error.code,
                    message: error.message,
                    effect: "not_applied",
                };
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    match initiate_restart_handoff(
        domain,
        root,
        expected_build_id,
        "installation.restart",
        Some(operation_id),
    ) {
        Ok(()) => RestartOutcome::Handoff,
        Err(error) => RestartOutcome::Failed {
            code: error.code,
            message: error.message,
            effect: "not_applied",
        },
    }
}

/// The restart handoff (009 §7): verify the target, record the pending
/// marker, spawn the successor from the install root's current generation,
/// give the accepted receipt a moment to flush, then terminate this
/// process. The successor completes the operation
/// ([`recover_pending_restart`]).
fn initiate_restart_handoff(
    domain: &Arc<InstallationDomain>,
    root: &Path,
    expected_build_id: &str,
    origin: &str,
    operation_id: Option<&str>,
) -> Result<(), ContractError> {
    let home = domain.rt.store().home().to_path_buf();
    let (current_build_id, _) = layout::current_generation(root).ok_or_else(|| {
        ContractError::new(
            "BUILD_CONFLICT",
            format!(
                "the install root at {} has no current generation to restart into",
                root.display()
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    if current_build_id != expected_build_id {
        return Err(ContractError::new(
            "BUILD_CONFLICT",
            format!(
                "the current generation is `{current_build_id}` but `{expected_build_id}` was \
                 expected; re-apply before restarting"
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }

    layout::write_restart_pending(
        &home,
        &layout::RestartPending {
            operation_id: operation_id.map(ToOwned::to_owned),
            expected_build_id: expected_build_id.to_owned(),
            origin: origin.to_owned(),
            data_root: home.display().to_string(),
            install_root: root.display().to_string(),
            previous_pid: std::process::id(),
            started_at: chrono::Utc::now().to_rfc3339(),
        },
    )
    .map_err(io_error)?;

    spawn_successor(&home, root).map_err(|message| {
        layout::remove_restart_pending(&home);
        io_error(message)
    })?;

    Ok(())
}

/// After the handoff is staged: a short grace for the accepted receipt to
/// flush, then the graceful self-termination. Called from the restart task
/// (tokio context) at the very end — the process does not return from this.
async fn finish_restart_handoff() {
    tokio::time::sleep(RESTART_RECEIPT_GRACE).await;
    terminate_self();
}

/// Spawns `<installRoot>/current/bin/agentcpm-pd server serve --data-root
/// <home>` detached, with the handoff env pointing at this pid. Logs append
/// to the same gateway logs the auto-start spawner uses.
fn spawn_successor(home: &Path, root: &Path) -> Result<(), String> {
    let exe = layout::current_link(root).join("bin").join("agentcpm-pd");
    if !exe.is_file() {
        return Err(format!(
            "the current generation binary {} does not exist",
            exe.display()
        ));
    }
    let logs_dir = home.join("logs");
    std::fs::create_dir_all(&logs_dir).map_err(|error| format!("mkdir logs: {error}"))?;
    let stdout = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logs_dir.join("gateway.stdout.log"))
        .map_err(|error| format!("open gateway log: {error}"))?;
    let stderr = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logs_dir.join("gateway.stderr.log"))
        .map_err(|error| format!("open gateway log: {error}"))?;
    std::process::Command::new(&exe)
        .arg("server")
        .arg("serve")
        .arg("--data-root")
        .arg(home)
        .env(layout::RESTART_HANDOFF_ENV, std::process::id().to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(stdout))
        .stderr(std::process::Stdio::from(stderr))
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("spawn the successor {}: {error}", exe.display()))
}

/// Graceful self-termination: SIGTERM, which the foreground serve loop
/// catches (its normal shutdown path drains connections and reclaims
/// ready.json). A host that does not catch it still dies to the default
/// disposition — the drain already made the state safe.
fn terminate_self() {
    #[cfg(unix)]
    {
        if let Some(pid) = rustix::process::Pid::from_raw(std::process::id() as i32) {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        }
    }
}

/// The successor-side wait: blocks until the predecessor pid exits
/// (bounded), so the single-instance lock is free before this process
/// claims it. Called from `serve_v1_gateway` before anything touches the
/// store; a no-op without the handoff env.
pub async fn wait_for_predecessor_exit() -> Result<(), ContractError> {
    let Some(raw) = std::env::var_os(layout::RESTART_HANDOFF_ENV) else {
        return Ok(());
    };
    let Ok(pid) = raw.to_string_lossy().parse::<u32>() else {
        return Err(unavailable(format!(
            "malformed {} value `{raw:?}`",
            layout::RESTART_HANDOFF_ENV
        )));
    };
    if pid == std::process::id() {
        return Ok(());
    }
    let deadline = Instant::now() + RESTART_HANDOFF_BOUND;
    while crate::public_v1::bootstrap::is_pid_alive(pid) {
        if Instant::now() >= deadline {
            return Err(unavailable(format!(
                "the predecessor gateway (pid {pid}) did not exit within {}s",
                RESTART_HANDOFF_BOUND.as_secs()
            )));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

/// The successor's startup hook: completes a pending restart operation.
/// `healthy` is true only when the install root's current target matches
/// the buildId the restart requested (009 §7.1: the ready.json identity
/// verification, expressed on the generation the process actually runs).
pub fn recover_pending_restart(rt: &Arc<PublicV1Runtime>, root: &Path) {
    let home = rt.store().home().to_path_buf();
    let Some(pending) = layout::read_restart_pending(&home) else {
        return;
    };
    let complete = |state: &str, completion: &Value| {
        if let Some(operation_id) = &pending.operation_id
            && let Ok(db) = DomainDb::open(&home, &[DDL_DOMAIN_OPERATIONS])
        {
            let _ = finish_domain_operation(&db, operation_id, "", state, completion);
        }
    };
    let current = layout::current_generation(root)
        .map(|(build_id, _)| build_id)
        .unwrap_or_default();
    if current == pending.expected_build_id {
        complete(
            "succeeded",
            &json!({
                "buildId": current,
                "healthy": true,
                "restartInitiated": true,
                "previousPid": pending.previous_pid,
                "newPid": std::process::id(),
                "origin": pending.origin,
            }),
        );
    } else {
        complete(
            "failed",
            &json!({"error": {
                "code": "HEALTH_CHECK_FAILED",
                "message": format!(
                    "after the restart the current generation is `{current}` but `{}` was \
                     expected",
                    pending.expected_build_id
                ),
                "effect": "applied",
            }}),
        );
    }
    layout::remove_restart_pending(&home);
}

// ---------------------------------------------------------------------------
// installation.diagnose (009 §4)
// ---------------------------------------------------------------------------

async fn installation_diagnose(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let domain = domain_for(&rt)?;
    let requested: Vec<String> = input
        .get("checks")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_else(|| {
            ["installation", "service", "runtime"]
                .iter()
                .map(|name| (*name).to_owned())
                .collect()
        });
    let include_logs = input
        .get("includeLogs")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let operation = accept_domain_operation(
        &domain.db,
        "installation.diagnose",
        &input,
        key.as_deref(),
        || format!("op-{}", domain.rt.store().next_id()),
        || Ok(()),
    )?;
    let runner = Arc::clone(&domain);
    let operation_id = operation.operation_id.clone();
    tokio::spawn(async move {
        let _ = mark_domain_operation_running(&runner.db, &operation_id, "");
        let root = layout::resolve_install_root(None).unwrap_or_default();
        let completion = perform_diagnose(&runner, &root, &requested, include_logs);
        let _ = finish_domain_operation(&runner.db, &operation_id, "", "succeeded", &completion);
    });
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

/// One diagnose check result: name / status / code / message (009 §4.2;
/// `skipped` and `unsupported` are first-class, never mapped to `pass`).
struct CheckResult {
    name: String,
    status: &'static str,
    code: Option<String>,
    message: String,
}

impl CheckResult {
    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "status": self.status,
            "code": self.code,
            "message": self.message,
        })
    }

    fn pass(name: &str, message: impl Into<String>) -> Self {
        Self {
            name: name.to_owned(),
            status: "pass",
            code: None,
            message: message.into(),
        }
    }

    fn warning(name: &str, code: &str, message: impl Into<String>) -> Self {
        Self {
            name: name.to_owned(),
            status: "warning",
            code: Some(code.to_owned()),
            message: message.into(),
        }
    }

    fn fail(name: &str, code: &str, message: impl Into<String>) -> Self {
        Self {
            name: name.to_owned(),
            status: "fail",
            code: Some(code.to_owned()),
            message: message.into(),
        }
    }

    fn unsupported(name: &str, message: impl Into<String>) -> Self {
        Self {
            name: name.to_owned(),
            status: "unsupported",
            code: None,
            message: message.into(),
        }
    }

    fn skipped(name: &str, message: impl Into<String>) -> Self {
        Self {
            name: name.to_owned(),
            status: "skipped",
            code: None,
            message: message.into(),
        }
    }
}

fn perform_diagnose(
    domain: &Arc<InstallationDomain>,
    root: &Path,
    requested: &[String],
    include_logs: bool,
) -> Value {
    let home = domain.rt.store().home().to_path_buf();
    let identity = layout::installation_identity_at(root);
    let mut checks: Vec<CheckResult> = Vec::new();

    if requested.iter().any(|name| name == "installation") {
        checks.push(check_installation_manifest(root, &identity));
        checks.push(check_installation_generations(root));
        checks.push(check_installation_targets(&home));
    }
    if requested.iter().any(|name| name == "service") {
        checks.push(check_service_status(&home));
    }
    if requested.iter().any(|name| name == "runtime") {
        checks.push(check_runtime_health(domain));
    }
    if requested.iter().any(|name| name == "channels") {
        checks.push(check_channels_configured(&home));
    }
    if requested.iter().any(|name| name == "mcp") {
        checks.push(check_mcp_configured(&home));
    }

    let mut storage = Value::Null;
    if requested.iter().any(|name| name == "storage") {
        checks.push(check_storage_data_root(&home));
        let (state_db_check, storage_value) = check_storage_state_db(&home);
        checks.push(state_db_check);
        checks.push(check_storage_free_space(&home));
        storage = storage_value;
    }

    // The full report artifact (009 §4.3), redacted (§4.4).
    let report = build_report(&identity, &home, &checks, storage, include_logs);
    let bytes = match serde_json::to_vec_pretty(&report) {
        Ok(bytes) => bytes,
        Err(error) => {
            return json!({
                "checks": checks.iter().map(CheckResult::to_json).collect::<Vec<_>>(),
                "redacted": true,
                "error": format!("serialize the report: {error}"),
            });
        }
    };
    let checks_json: Vec<Value> = checks.iter().map(CheckResult::to_json).collect();
    match crate::public_v1::domains::artifacts::publish_artifact(
        domain.rt.store(),
        "",
        "application/json",
        Some("installation-diagnose-report.json"),
        &bytes,
        "installation.diagnose",
    ) {
        Ok(artifact) => json!({
            "reportArtifactId": artifact["artifactId"],
            "reportArtifact": {
                "artifactId": artifact["artifactId"],
                "length": artifact["byteLength"],
                "sha256": artifact["sha256"],
                "mediaType": "application/json",
            },
            "checks": checks_json,
            "redacted": true,
        }),
        Err(error) => json!({
            "checks": checks_json,
            "redacted": true,
            "error": format!("publish the report artifact: {}", error.message),
        }),
    }
}

fn check_installation_manifest(
    root: &Path,
    identity: &layout::InstallationIdentity,
) -> CheckResult {
    match &identity.manifest {
        Some(manifest) => {
            let dir = layout::current_link(root)
                .canonicalize()
                .unwrap_or_else(|_| layout::current_link(root));
            match release::verify_generation(&dir, manifest) {
                Ok(()) => CheckResult::pass(
                    "installation.manifest",
                    format!(
                        "the current generation `{}` verifies (SHA256SUMS re-checked)",
                        manifest.build_id
                    ),
                ),
                Err(message) => {
                    CheckResult::fail("installation.manifest", "INTEGRITY_ERROR", message)
                }
            }
        }
        None => CheckResult::unsupported(
            "installation.manifest",
            format!(
                "installation mode `{}` has no managed generation layout",
                identity.installation_mode.as_str()
            ),
        ),
    }
}

fn check_installation_generations(root: &Path) -> CheckResult {
    let generations = layout::list_generations(root);
    let current_ok = layout::current_generation(root).is_some();
    // A root with no `current` link at all was never managed (a source-mode
    // install) — a clean state, not dirty residue. Only a link that exists
    // but fails to resolve is a broken managed layout worth a warning.
    let current_link_exists = layout::current_link(root).symlink_metadata().is_ok();
    let last_update = layout::read_last_update(root);
    let mut problems: Vec<String> = Vec::new();
    if current_link_exists && !current_ok {
        problems.push("the current symlink does not resolve".to_owned());
    }
    if layout::update_lock_dir(root).is_dir() {
        let owner = layout::active_update_operation_id(root);
        problems.push(format!(
            "a stale update lock exists (owner: {owner:?}); no automatic cleanup — remove {} \
             manually once no update is running",
            layout::update_lock_dir(root).display()
        ));
    }
    if let Some(leftovers) = std::fs::read_dir(layout::staging_dir(root))
        .ok()
        .map(|entries| entries.filter_map(Result::ok).count())
        .filter(|count| *count > 0)
    {
        problems.push(format!(
            "{leftovers} staging leftover(s) from an interrupted update; safe to remove"
        ));
    }
    let message = format!(
        "{} generation(s) retained; last update: {}",
        generations.len(),
        last_update
            .map(|record| {
                format!(
                    "`{}` → `{}` ({})",
                    record.release_id, record.new_build_id, record.result
                )
            })
            .unwrap_or_else(|| "never".to_owned())
    );
    if problems.is_empty() {
        CheckResult::pass("installation.generations", message)
    } else {
        CheckResult::warning(
            "installation.generations",
            "UPDATE_STATE_DIRTY",
            format!("{message}; {}", problems.join("; ")),
        )
    }
}

fn check_installation_targets(home: &Path) -> CheckResult {
    let targets_path = home.join("targets.json");
    if !targets_path.exists() {
        return CheckResult::pass("installation.targets", "no target profiles configured");
    }
    let parsed = std::fs::read_to_string(&targets_path)
        .map_err(|error| error.to_string())
        .and_then(|text| serde_json::from_str::<Value>(&text).map_err(|error| error.to_string()));
    let targets = match parsed {
        Ok(targets) => targets,
        Err(message) => {
            return CheckResult::fail(
                "installation.targets",
                "CONFIG_UNREADABLE",
                format!("targets.json: {message}"),
            );
        }
    };
    let credentials = std::fs::read_to_string(home.join("credentials.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    let entries = targets
        .get("targets")
        .and_then(Value::as_object)
        .map_or(0, serde_json::Map::len);
    let mut missing: Vec<String> = Vec::new();
    if let Some(map) = targets.get("targets").and_then(Value::as_object) {
        for (target_id, record) in map {
            if let Some(reference) = record.get("credentialRef").and_then(Value::as_str)
                && credentials
                    .as_ref()
                    .and_then(|value| value.get(reference))
                    .is_none()
            {
                missing.push(target_id.clone());
            }
        }
    }
    if missing.is_empty() {
        CheckResult::pass(
            "installation.targets",
            format!("{entries} target profile(s); all credentialRefs resolve"),
        )
    } else {
        CheckResult::warning(
            "installation.targets",
            "CREDENTIAL_REF_UNRESOLVED",
            format!(
                "{entries} target profile(s); credentialRef unresolved for: {}",
                missing.join(", ")
            ),
        )
    }
}

fn check_service_status(home: &Path) -> CheckResult {
    // Diagnose runs inside the serving gateway: ready.json on disk must
    // describe *this* process.
    let parsed = std::fs::read_to_string(home.join("run").join("ready.json"))
        .map_err(|error| error.to_string())
        .and_then(|text| serde_json::from_str::<Value>(&text).map_err(|error| error.to_string()));
    match parsed {
        Ok(ready) => {
            let pid = ready.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32;
            let state = if pid == std::process::id() {
                "ready"
            } else if crate::public_v1::bootstrap::is_pid_alive(pid) {
                "unresponsive"
            } else {
                "stopped"
            };
            if state == "ready" {
                CheckResult::pass(
                    "service.status",
                    format!(
                        "ready (pid {pid}, generation `{}`)",
                        ready
                            .get("generation")
                            .and_then(Value::as_str)
                            .unwrap_or("?")
                    ),
                )
            } else {
                CheckResult::fail(
                    "service.status",
                    "GATEWAY_STATE_MISMATCH",
                    format!(
                        "ready.json reports pid {pid} (state `{state}`) but this gateway is pid \
                         {}",
                        std::process::id()
                    ),
                )
            }
        }
        Err(message) => CheckResult::fail(
            "service.status",
            "GATEWAY_UNREADY",
            format!("run/ready.json: {message}"),
        ),
    }
}

fn check_runtime_health(domain: &Arc<InstallationDomain>) -> CheckResult {
    let diagnostics = domain.rt.store().diagnostics();
    if diagnostics.is_empty() {
        CheckResult::pass("runtime.health", "alive and ready")
    } else {
        let details: Vec<String> = diagnostics
            .iter()
            .map(|diagnostic| format!("{}: {}", diagnostic.code, diagnostic.message))
            .collect();
        CheckResult::warning("runtime.health", "RUNTIME_DIAGNOSTICS", details.join("; "))
    }
}

fn check_channels_configured(home: &Path) -> CheckResult {
    match open_state_db_readonly(home) {
        Ok(conn) => {
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM v1_channels", [], |row| row.get(0))
                .unwrap_or(0);
            if count == 0 {
                CheckResult::pass("channels.configured", "no channels configured")
            } else {
                CheckResult::pass(
                    "channels.configured",
                    format!(
                        "{count} channel(s) configured (read-only listing; live probes are not \
                         part of diagnose)"
                    ),
                )
            }
        }
        Err(error) => {
            CheckResult::fail("channels.configured", "STORAGE_UNAVAILABLE", error.message)
        }
    }
}

fn check_mcp_configured(home: &Path) -> CheckResult {
    let mcp_path = home.join("mcp.json");
    if !mcp_path.exists() {
        return CheckResult::pass("mcp.configured", "no global mcp.json");
    }
    match std::fs::read_to_string(&mcp_path)
        .map_err(|error| error.to_string())
        .and_then(|text| serde_json::from_str::<Value>(&text).map_err(|error| error.to_string()))
    {
        Ok(value) if value.is_object() => {
            let count = value.as_object().map_or(0, serde_json::Map::len);
            CheckResult::pass(
                "mcp.configured",
                format!("{count} mcp server entry/entries (structural check only)"),
            )
        }
        Ok(_) => CheckResult::fail(
            "mcp.configured",
            "CONFIG_UNREADABLE",
            "mcp.json is not a JSON object",
        ),
        Err(message) => CheckResult::fail(
            "mcp.configured",
            "CONFIG_UNREADABLE",
            format!("mcp.json: {message}"),
        ),
    }
}

fn check_storage_data_root(home: &Path) -> CheckResult {
    let mut mode = String::new();
    let readable;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match std::fs::metadata(home) {
            Ok(meta) => {
                readable = meta.is_dir();
                mode = format!("{:o}", meta.mode() & 0o777);
            }
            Err(_) => readable = false,
        }
    }
    #[cfg(not(unix))]
    {
        readable = std::fs::metadata(home)
            .map(|meta| meta.is_dir())
            .unwrap_or(false);
    }
    let local_meta = std::fs::read_to_string(home.join("local.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    if !readable {
        CheckResult::fail(
            "storage.dataRoot",
            "DATA_ROOT_MISSING",
            format!("the data root {} is missing", home.display()),
        )
    } else if local_meta.is_none() {
        CheckResult::fail(
            "storage.dataRoot",
            "CONFIG_UNREADABLE",
            "local.json is missing or unparseable",
        )
    } else if mode != "700" {
        CheckResult::warning(
            "storage.dataRoot",
            "DATA_ROOT_MODE",
            format!("the data root mode is {mode} (expected 700)"),
        )
    } else {
        CheckResult::pass(
            "storage.dataRoot",
            format!("mode {mode}, local.json parses"),
        )
    }
}

/// Returns the `storage.stateDb` check plus the report's `storage` section.
fn check_storage_state_db(home: &Path) -> (CheckResult, Value) {
    match open_state_db_readonly(home) {
        Ok(conn) => {
            let quick_check: String = conn
                .query_row("PRAGMA quick_check", [], |row| row.get(0))
                .unwrap_or_else(|error| format!("quick_check failed: {error}"));
            let user_version: i64 = conn
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .unwrap_or(0);
            let check = if quick_check != "ok" {
                CheckResult::fail(
                    "storage.stateDb",
                    "STATE_DB_CORRUPT",
                    format!("PRAGMA quick_check: {quick_check}"),
                )
            } else if user_version > SUPPORTED_V1_SCHEMA {
                CheckResult::warning(
                    "storage.stateDb",
                    "SCHEMA_NEWER_THAN_BINARY",
                    format!(
                        "state db user_version {user_version} > supported {SUPPORTED_V1_SCHEMA}; \
                         writes are refused, read-only service only"
                    ),
                )
            } else {
                CheckResult::pass(
                    "storage.stateDb",
                    format!("quick_check ok, user_version {user_version}"),
                )
            };
            let storage = json!({
                "dataRoot": redact_path(home, home),
                "stateDb": quick_check,
                "schemaVersion": user_version,
                "freeBytes": free_space_bytes(home),
            });
            (check, storage)
        }
        Err(error) => (
            CheckResult::fail("storage.stateDb", "STATE_DB_UNAVAILABLE", error.message),
            Value::Null,
        ),
    }
}

fn check_storage_free_space(home: &Path) -> CheckResult {
    match free_space_bytes(home) {
        Some(bytes) if bytes < FREE_SPACE_WARNING_BYTES => CheckResult::warning(
            "storage.freeSpace",
            "FREE_SPACE_LOW",
            format!(
                "{} bytes free on the data root volume (threshold {FREE_SPACE_WARNING_BYTES})",
                bytes
            ),
        ),
        Some(_) => CheckResult::pass(
            "storage.freeSpace",
            "the data root volume has at least 1 GiB free",
        ),
        None => CheckResult::skipped(
            "storage.freeSpace",
            "free space could not be determined on this platform",
        ),
    }
}

fn free_space_bytes(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        let stat = rustix::fs::statvfs(path).ok()?;
        Some(stat.f_bavail.saturating_mul(stat.f_frsize.max(1)))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Home-path abbreviation (redaction rule 3): the data root and anything
/// under $HOME render as `~…`.
fn redact_path(home: &Path, path: &Path) -> String {
    let text = path.display().to_string();
    let home_text = home.display().to_string();
    if let Some(rest) = text.strip_prefix(&home_text) {
        return format!("~{rest}");
    }
    if let Some(home_env) = std::env::var_os("HOME") {
        let prefix = Path::new(&home_env).display().to_string();
        if let Some(rest) = text.strip_prefix(&prefix) {
            return format!("~{rest}");
        }
    }
    text
}

fn build_report(
    identity: &layout::InstallationIdentity,
    home: &Path,
    checks: &[CheckResult],
    storage: Value,
    include_logs: bool,
) -> Value {
    let mut recent_errors: Vec<Value> = Vec::new();
    if let Ok(conn) = open_state_db_readonly(home) {
        let mut stmt = conn
            .prepare(
                "SELECT run_id, state, outcome_code, updated_at FROM runs
                 WHERE state IN ('failed', 'interrupted', 'result_unknown')
                 ORDER BY updated_at DESC LIMIT 10",
            )
            .ok();
        if let Some(stmt) = stmt.as_mut() {
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            });
            if let Ok(rows) = rows {
                for row in rows.flatten() {
                    let (run_id, state, code, at) = row;
                    recent_errors.push(json!({
                        "runId": run_id,
                        "state": state,
                        "code": code,
                        "at": at,
                    }));
                }
            }
        }
    }

    let mut redaction_rules = vec!["credential-values", "secret-env", "home-paths"];
    if include_logs {
        redaction_rules.push("log-excerpt-truncation");
    }

    json!({
        "schemaVersion": "agentcpm-pd.diagnose-report/1",
        "capturedAt": chrono::Utc::now().to_rfc3339(),
        "identity": {
            "productName": identity.product_name,
            "version": identity.version,
            "buildId": identity.build_id,
            "platform": format!("{}/{}", identity.platform, identity.arch),
            "installationMode": identity.installation_mode.as_str(),
            "tagName": identity.manifest.as_ref().map(|manifest| manifest.tag.clone()),
        },
        "platform": {
            "os": identity.platform,
            "arch": identity.arch,
            "installationMode": identity.installation_mode.as_str(),
        },
        "storage": storage,
        "recentErrors": recent_errors,
        "checks": checks.iter().map(CheckResult::to_json).collect::<Vec<_>>(),
        "redaction": {
            "applied": true,
            "rules": redaction_rules,
        },
        "includeLogs": include_logs,
    })
}

// ---------------------------------------------------------------------------
// Tests (contract positives and negatives; the e2e suite drives the real
// CLI binary + embedded gateway in tests/e2e_v1_installation.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::store::{InstanceMeta, V1Store};
    use crate::public_v1::test_support::TempDir;

    use serde_json::json;
    use sha2::{Digest as _, Sha256, Sha512};

    struct Fixture {
        _tmp: TempDir,
        home: PathBuf,
        root: PathBuf,
        rt: Arc<PublicV1Runtime>,
        domain: Arc<InstallationDomain>,
    }

    async fn fixture(tag: &str) -> Fixture {
        let tmp = TempDir::new(tag);
        let home = tmp.0.join("pd-home");
        let root = tmp.0.join("install-root");
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
        let rt = PublicV1Runtime::new(store);
        let domain = domain_for(&rt).unwrap();
        Fixture {
            _tmp: tmp,
            home,
            root,
            rt,
            domain,
        }
    }

    /// The controlled local channel: serves `release.json` and assets at
    /// their manifest names. `trickle_ms` > 0 trickles the asset body out
    /// slowly (pause/cancel determinism).
    async fn spawn_channel(
        manifest_bytes: Vec<u8>,
        assets: Vec<(String, Vec<u8>)>,
        trickle_ms: u64,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let manifest_bytes = manifest_bytes.clone();
                let assets = assets.clone();
                let trickle_ms = trickle_ms;
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut head = Vec::new();
                    let mut chunk = [0u8; 2048];
                    loop {
                        let read = socket.read(&mut chunk).await.unwrap_or(0);
                        if read == 0 {
                            return;
                        }
                        head.extend_from_slice(&chunk[..read]);
                        if head.windows(4).any(|window| window == b"\r\n\r\n")
                            || head.len() > 64 * 1024
                        {
                            break;
                        }
                    }
                    let request_line = String::from_utf8_lossy(&head);
                    let path = request_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .trim_start_matches('/')
                        .to_owned();
                    let (body, content_type) = if path == "release.json" {
                        (manifest_bytes.clone(), "application/json")
                    } else {
                        match assets.iter().find(|(name, _)| *name == path) {
                            Some((_, bytes)) => (bytes.clone(), "application/gzip"),
                            None => (Vec::new(), "application/octet-stream"),
                        }
                    };
                    let status = if body.is_empty() && path != "release.json" {
                        "404 Not Found"
                    } else {
                        "200 OK"
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    if socket.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                    if trickle_ms > 0 && path != "release.json" {
                        for piece in body.chunks(16 * 1024) {
                            if socket.write_all(piece).await.is_err() {
                                return;
                            }
                            tokio::time::sleep(Duration::from_millis(trickle_ms)).await;
                        }
                    } else if socket.write_all(&body).await.is_err() {
                        return;
                    }
                    let _ = socket.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), task)
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn sha512_base64(bytes: &[u8]) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(Sha512::digest(bytes))
    }

    struct ChannelPackage {
        manifest_json: Value,
        archive: Vec<u8>,
        asset_name: String,
        build_id: String,
        tag: String,
    }

    /// Builds one valid package: a tar.gz with bin/agentcpm-pd,
    /// manifest/release.json and SHA256SUMS.txt, plus the channel manifest
    /// describing it for the CURRENT platform.
    fn build_package(
        version: &str,
        tag: &str,
        source_sha: &str,
        contract_digest: &str,
    ) -> ChannelPackage {
        let dir = std::env::temp_dir().join(format!(
            "t021-pkg-{}-{}",
            std::process::id(),
            source_sha.chars().take(6).collect::<String>()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::create_dir_all(dir.join("manifest")).unwrap();
        let binary = dir.join("bin").join("agentcpm-pd");
        std::fs::write(&binary, b"#!/bin/sh\necho agentcpm-pd t021\n").unwrap();
        let build_id = release::build_id(version, source_sha, contract_digest);
        let (platform, arch) = release::current_platform_pair();
        let asset_name = format!("agentcpm-pd-{version}-{platform}-{arch}.tar.gz");
        // The in-package manifest carries the same asset row as the channel
        // one, with placeholder digests: the archive contains this very
        // manifest, so it cannot embed the digest of itself. Consumer
        // validation only requires the row to be well-formed (009 §2.3);
        // the download digest checks run against the channel manifest.
        let manifest_json = json!({
            "schemaVersion": 2,
            "product": "agentcpm-pd",
            "version": version,
            "buildId": build_id,
            "tag": tag,
            "date": "2026-11-18",
            "buildTime": "2026-11-18T00:00:00Z",
            "sourceSha": source_sha,
            "contractDigest": contract_digest,
            "assets": [{
                "name": asset_name.clone(),
                "size": 1000,
                "sha256": "c".repeat(64),
                "sha512": "d".repeat(86) + "==",
                "platform": platform,
                "arch": arch,
                "kind": "archive",
            }],
        });
        std::fs::write(
            dir.join("manifest").join("release.json"),
            manifest_json.to_string(),
        )
        .unwrap();
        let sums = release::render_sha256sums(&[
            ("bin/agentcpm-pd".to_owned(), binary.clone()),
            (
                "manifest/release.json".to_owned(),
                dir.join("manifest").join("release.json"),
            ),
        ])
        .unwrap();
        std::fs::write(dir.join("SHA256SUMS.txt"), &sums).unwrap();

        let archive_path = dir.join(&asset_name);
        release::write_release_package(
            &archive_path,
            &[
                release::PackageFile {
                    relative_path: "bin/agentcpm-pd".to_owned(),
                    source: binary,
                    mode: 0o755,
                },
                release::PackageFile {
                    relative_path: "manifest/release.json".to_owned(),
                    source: dir.join("manifest").join("release.json"),
                    mode: 0o644,
                },
                release::PackageFile {
                    relative_path: "SHA256SUMS.txt".to_owned(),
                    source: dir.join("SHA256SUMS.txt"),
                    mode: 0o644,
                },
            ],
        )
        .unwrap();
        let archive = std::fs::read(&archive_path).unwrap();

        let manifest_json = json!({
            "schemaVersion": 2,
            "product": "agentcpm-pd",
            "version": version,
            "buildId": build_id,
            "tag": tag,
            "date": "2026-11-18",
            "buildTime": "2026-11-18T00:00:00Z",
            "sourceSha": source_sha,
            "contractDigest": contract_digest,
            "assets": [{
                "name": asset_name,
                "size": archive.len(),
                "sha256": sha256_hex(&archive),
                "sha512": sha512_base64(&archive),
                "platform": platform,
                "arch": arch,
                "kind": "archive",
            }],
        });
        ChannelPackage {
            manifest_json,
            archive,
            asset_name,
            build_id,
            tag: tag.to_owned(),
        }
    }

    fn update_state() -> Arc<UpdateState> {
        Arc::new(UpdateState {
            phase: Mutex::new(UpdatePhase::Resolving),
            control: channel::DownloadControl::new(),
        })
    }

    async fn configure_channel(fx: &Fixture, url: &str) {
        let mut config = layout::ChannelConfig::default();
        config.channels.insert("stable".to_owned(), url.to_owned());
        layout::save_channel_config(&fx.home, &config).unwrap();
    }

    #[tokio::test]
    async fn check_reports_update_and_completion_shape() {
        let fx = fixture("check").await;
        let package = build_package(
            "9.9.9",
            "v2026.11.18",
            "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            "ffffffffffff",
        );
        let (base, _server) = spawn_channel(
            package.manifest_json.to_string().into_bytes(),
            vec![(package.asset_name.clone(), package.archive.clone())],
            0,
        )
        .await;

        let completion = perform_check(&fx.root, &base)
            .await
            .expect("check succeeds");
        assert_eq!(
            completion["currentVersion"],
            json!(env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(completion["updateAvailable"], true);
        let releases = completion["releases"].as_array().unwrap();
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0]["releaseId"], json!(package.tag));
        assert_eq!(releases[0]["version"], json!("9.9.9"));
        assert_eq!(releases[0]["verified"], true);
        assert_eq!(releases[0]["requiresRestart"], true);
        let (platform, arch) = release::current_platform_pair();
        assert_eq!(releases[0]["platform"], json!(format!("{platform}/{arch}")));

        // A channel serving the locally installed buildId reports no
        // update. The no-update path is exercised through a managed install
        // whose identity comes from the current generation's manifest: a
        // source-mode buildId (sourceSha `dev`) can never appear in a valid
        // manifest, since sourceSha must be a 40-hex commit (009 §1).
        let own = build_package(
            "9.9.9",
            "v2026.11.19",
            "5555555555555555555555555555555555555555",
            "ffffffffffff",
        );
        let generation = layout::generation_dir(&fx.root, &own.build_id);
        std::fs::create_dir_all(generation.join("bin")).unwrap();
        std::fs::create_dir_all(generation.join("manifest")).unwrap();
        std::fs::write(generation.join("bin").join("agentcpm-pd"), b"#!/bin/sh\n").unwrap();
        std::fs::write(
            generation.join("manifest").join("release.json"),
            own.manifest_json.to_string(),
        )
        .unwrap();
        layout::switch_current(&fx.root, &own.build_id).unwrap();
        assert_eq!(
            own.build_id,
            layout::installation_identity_at(&fx.root).build_id
        );
        let (base, _server) = spawn_channel(
            own.manifest_json.to_string().into_bytes(),
            vec![(own.asset_name.clone(), own.archive.clone())],
            0,
        )
        .await;
        let completion = perform_check(&fx.root, &base)
            .await
            .expect("check succeeds");
        assert_eq!(completion["updateAvailable"], false);
        assert_eq!(completion["releases"], json!([]));
    }

    #[tokio::test]
    async fn check_fails_honestly_on_network_and_manifest_errors() {
        let fx = fixture("check-net").await;
        // Port 1 refuses connections: UPSTREAM_ERROR.
        let (code, message) = perform_check(&fx.root, "http://127.0.0.1:1")
            .await
            .unwrap_err();
        assert_eq!(code, "UPSTREAM_ERROR");
        assert!(!message.is_empty());

        // A manifest failing consumer validation: INTEGRITY_ERROR.
        let broken = json!({"schemaVersion": 1, "product": "agentcpm-pd"});
        let (base, _server) = spawn_channel(broken.to_string().into_bytes(), vec![], 0).await;
        let (code, _) = perform_check(&fx.root, &base).await.unwrap_err();
        assert_eq!(code, "INTEGRITY_ERROR");
    }

    #[tokio::test]
    async fn apply_installs_verifies_and_switches_atomically() {
        let fx = fixture("apply-ok").await;
        let package = build_package(
            "9.9.9",
            "v2026.11.18",
            "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            "ffffffffffff",
        );
        let (base, _server) = spawn_channel(
            package.manifest_json.to_string().into_bytes(),
            vec![(package.asset_name.clone(), package.archive.clone())],
            0,
        )
        .await;
        configure_channel(&fx, &base).await;

        let state = update_state();
        let outcome = run_apply(
            &fx.domain,
            &state,
            "op-apply-1",
            &fx.root,
            &package.tag,
            &package.build_id,
            "defer",
        )
        .await;
        let completion = match outcome {
            ApplyOutcome::Succeeded { completion, .. } => completion,
            other => panic!("apply must succeed, got {other:?}"),
        };
        assert_eq!(completion["buildId"], json!(package.build_id));
        assert_eq!(completion["restartRequired"], true);
        assert_eq!(completion["recoveryAvailable"], false);
        let generation = layout::generation_dir(&fx.root, &package.build_id);
        assert!(generation.join("bin").join("agentcpm-pd").is_file());
        let (current, manifest) = layout::current_generation(&fx.root).unwrap();
        assert_eq!(current, package.build_id);
        assert_eq!(manifest.tag, package.tag);
        assert!(layout::read_last_update(&fx.root).is_some());
        // Nothing staged is left behind.
        assert!(
            layout::staging_dir(&fx.root)
                .read_dir()
                .map(|entries| entries.count() == 0)
                .unwrap_or(true)
        );
    }

    #[tokio::test]
    async fn apply_rejects_build_conflict_platform_and_integrity() {
        let fx = fixture("apply-bad").await;
        let package = build_package(
            "9.9.9",
            "v2026.11.18",
            "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            "ffffffffffff",
        );
        let (base, _server) = spawn_channel(
            package.manifest_json.to_string().into_bytes(),
            vec![(package.asset_name.clone(), package.archive.clone())],
            0,
        )
        .await;
        configure_channel(&fx, &base).await;

        // Stage 2 CAS: a different expectedBuildId fails BUILD_CONFLICT.
        let state = update_state();
        let outcome = run_apply(
            &fx.domain,
            &state,
            "op-bc",
            &fx.root,
            &package.tag,
            "0.0.0+000000000000.000000000000",
            "defer",
        )
        .await;
        match outcome {
            ApplyOutcome::Failed { code, .. } => assert_eq!(code, "BUILD_CONFLICT"),
            other => panic!("expected BUILD_CONFLICT, got {other:?}"),
        }

        // Stage 3 integrity: a corrupted body fails INTEGRITY_ERROR and
        // keeps the installation untouched.
        let mut corrupted = package.archive.clone();
        let last = corrupted.len() - 1;
        corrupted[last] = corrupted[last].wrapping_add(1);
        let (base, _server) = spawn_channel(
            package.manifest_json.to_string().into_bytes(),
            vec![(package.asset_name.clone(), corrupted)],
            0,
        )
        .await;
        configure_channel(&fx, &base).await;
        let state = update_state();
        let outcome = run_apply(
            &fx.domain,
            &state,
            "op-int",
            &fx.root,
            &package.tag,
            &package.build_id,
            "defer",
        )
        .await;
        match outcome {
            ApplyOutcome::Failed { code, .. } => assert_eq!(code, "INTEGRITY_ERROR"),
            other => panic!("expected INTEGRITY_ERROR, got {other:?}"),
        }
        assert!(layout::current_generation(&fx.root).is_none());
        assert!(
            layout::staging_dir(&fx.root)
                .read_dir()
                .map(|entries| entries.count() == 0)
                .unwrap_or(true)
        );

        // No asset for this platform: PLATFORM_UNSUPPORTED. The asset row
        // stays inside the documented darwin/linux matrix (009 §2.1) so the
        // manifest itself passes consumer validation — only the local
        // platform match fails.
        let (local_platform, local_arch) = release::current_platform_pair();
        let foreign_platform = if local_platform == "darwin" {
            "linux"
        } else {
            "darwin"
        };
        let foreign = json!({
            "schemaVersion": 2, "product": "agentcpm-pd", "version": "9.9.9",
            "buildId": package.build_id, "tag": "v2026.11.18",
            "date": "2026-11-18", "buildTime": "2026-11-18T00:00:00Z",
            "sourceSha": "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            "contractDigest": "ffffffffffff",
            "assets": [{
                "name": format!("agentcpm-pd-9.9.9-{foreign_platform}-{local_arch}.tar.gz"),
                "size": 10, "sha256": "0".repeat(64), "sha512": "0".repeat(86) + "==",
                "platform": foreign_platform, "arch": local_arch, "kind": "archive",
            }],
        });
        let (base, _server) = spawn_channel(foreign.to_string().into_bytes(), vec![], 0).await;
        configure_channel(&fx, &base).await;
        let state = update_state();
        let outcome = run_apply(
            &fx.domain,
            &state,
            "op-plat",
            &fx.root,
            &package.tag,
            &package.build_id,
            "defer",
        )
        .await;
        match outcome {
            ApplyOutcome::Failed { code, .. } => assert_eq!(code, "PLATFORM_UNSUPPORTED"),
            other => panic!("expected PLATFORM_UNSUPPORTED, got {other:?}"),
        }
    }

    /// Rollback = apply of a retained local generation (009 §5.1): no
    /// channel is configured, the old generation is still on disk, and the
    /// switch goes back to it.
    #[tokio::test]
    async fn apply_rolls_back_to_a_retained_local_generation() {
        let fx = fixture("apply-rollback").await;
        let older = build_package(
            "8.8.8",
            "v2026.10.01",
            "dddddddddddddddddddddddddddddddddddddddd",
            "ffffffffffff",
        );
        let newer = build_package(
            "9.9.9",
            "v2026.11.18",
            "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            "ffffffffffff",
        );
        let (base, _server) = spawn_channel(
            newer.manifest_json.to_string().into_bytes(),
            vec![(newer.asset_name.clone(), newer.archive.clone())],
            0,
        )
        .await;
        configure_channel(&fx, &base).await;

        // First apply (from the channel).
        let state = update_state();
        let outcome = run_apply(
            &fx.domain,
            &state,
            "op-fwd",
            &fx.root,
            &newer.tag,
            &newer.build_id,
            "defer",
        )
        .await;
        match outcome {
            ApplyOutcome::Succeeded { completion, .. } => {
                assert_eq!(completion["recoveryAvailable"], false);
            }
            other => panic!("forward apply must succeed, got {other:?}"),
        }

        // Seed the older generation locally (the retained-rollback state).
        let older_dir = layout::generation_dir(&fx.root, &older.build_id);
        std::fs::create_dir_all(older_dir.join("bin")).unwrap();
        std::fs::create_dir_all(older_dir.join("manifest")).unwrap();
        std::fs::write(older_dir.join("bin").join("agentcpm-pd"), b"#!/bin/sh\n").unwrap();
        std::fs::write(
            older_dir.join("manifest").join("release.json"),
            older.manifest_json.to_string(),
        )
        .unwrap();

        // Roll back: no channel configured at all, releaseId resolves
        // locally, expectedBuildId is the OLD generation.
        let empty = layout::ChannelConfig::default();
        layout::save_channel_config(&fx.home, &empty).unwrap();
        let state = update_state();
        let outcome = run_apply(
            &fx.domain,
            &state,
            "op-back",
            &fx.root,
            &older.tag,
            &older.build_id,
            "defer",
        )
        .await;
        match outcome {
            ApplyOutcome::Succeeded { completion, .. } => {
                assert_eq!(completion["buildId"], json!(older.build_id));
                assert_eq!(completion["previousBuildId"], json!(newer.build_id));
                assert_eq!(completion["recoveryAvailable"], true);
            }
            other => panic!("rollback apply must succeed, got {other:?}"),
        }
        let (current, _) = layout::current_generation(&fx.root).unwrap();
        assert_eq!(current, older.build_id);
    }

    /// The control plane: pause parks the download, resume continues it,
    /// cancel aborts with no half installation (009 §3.4). Driven through
    /// the real dispatch path so the phase matrix is what callers see.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn control_update_pauses_resumes_and_cancels_the_download() {
        let fx = fixture("control").await;
        let package = build_package(
            "9.9.9",
            "v2026.11.18",
            "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            "ffffffffffff",
        );
        // A large body trickled out slowly: the download lasts seconds.
        let big_archive = [&package.archive[..], &vec![0u8; 256 * 1024][..]].concat();
        let mut trickled = package.manifest_json.clone();
        trickled["assets"][0]["size"] = json!(big_archive.len());
        trickled["assets"][0]["sha256"] = json!(sha256_hex(&big_archive));
        trickled["assets"][0]["sha512"] = json!(sha512_base64(&big_archive));
        let (base, _server) = spawn_channel(
            trickled.to_string().into_bytes(),
            vec![(package.asset_name.clone(), big_archive.clone())],
            40,
        )
        .await;
        configure_channel(&fx, &base).await;

        let state = update_state();
        fx.domain
            .updates
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("op-ctl-1".to_owned(), Arc::clone(&state));
        let runner = Arc::clone(&fx.domain);
        let root = fx.root.clone();
        let tag = package.tag.clone();
        let build_id = package.build_id.clone();
        let task = tokio::spawn({
            let runner = Arc::clone(&runner);
            let state = Arc::clone(&state);
            async move { run_apply(&runner, &state, "op-ctl-1", &root, &tag, &build_id, "defer").await }
        });

        // Wait for the download phase, then pause mid-flight.
        wait_for_phase(&state, UpdatePhase::Downloading).await;
        let (_, paused) = fx
            .rt
            .dispatch(
                "installation.control_update",
                json!({"operationId": "op-ctl-1", "action": "pause"}),
                Some("key-pause-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(paused["accepted"], true);
        assert_eq!(paused["state"], "paused");
        assert_eq!(paused["allowedActions"], json!(["resume", "cancel"]));

        // A pause of a paused update converges; a resume continues it.
        let (_, replay) = fx
            .rt
            .dispatch(
                "installation.control_update",
                json!({"operationId": "op-ctl-1", "action": "pause"}),
                Some("key-pause-2"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(replay["state"], "paused");

        let (_, resumed) = fx
            .rt
            .dispatch(
                "installation.control_update",
                json!({"operationId": "op-ctl-1", "action": "resume"}),
                Some("key-resume-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(resumed["state"], "downloading");

        // Cancel mid-download: the install aborts, nothing is half-applied.
        wait_for_phase(&state, UpdatePhase::Downloading).await;
        let (_, cancelled) = fx
            .rt
            .dispatch(
                "installation.control_update",
                json!({"operationId": "op-ctl-1", "action": "cancel"}),
                Some("key-cancel-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(cancelled["accepted"], true);
        assert_eq!(cancelled["state"], "cancelling");
        assert_eq!(cancelled["allowedActions"], json!([]));

        let outcome = task.await.unwrap();
        match outcome {
            ApplyOutcome::Cancelled => {}
            other => panic!("expected a cancelled apply, got {other:?}"),
        }
        assert!(layout::current_generation(&fx.root).is_none());
        assert!(
            layout::generations_dir(&fx.root)
                .read_dir()
                .map(|entries| entries.count() == 0)
                .unwrap_or(true)
        );
        assert!(
            layout::staging_dir(&fx.root)
                .read_dir()
                .map(|entries| entries.count() == 0)
                .unwrap_or(true)
        );
    }

    async fn wait_for_phase(state: &Arc<UpdateState>, phase: UpdatePhase) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let current = *state.phase.lock().unwrap_or_else(|e| e.into_inner());
            if current == phase {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the update never reached phase `{}` (now `{}`)",
                phase.as_str(),
                current.as_str()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The stage matrix beyond downloading: nothing is cancellable once the
    /// verification started; unknown ids and foreign operations answer
    /// NOT_FOUND / INVALID_STATE (009 §3.4).
    #[tokio::test]
    async fn control_update_enforces_the_stage_matrix() {
        let fx = fixture("matrix").await;

        // Unknown operationId: NOT_FOUND.
        let error = dispatch_error(
            &fx.rt,
            "installation.control_update",
            json!({"operationId": "op-none", "action": "cancel"}),
        )
        .await;
        assert_eq!(error, "NOT_FOUND");

        // A non-apply operation is not controllable.
        let accepted = accept_domain_operation(
            &fx.domain.db,
            "installation.check",
            &json!({"channel": "stable"}),
            Some("key-foreign-1"),
            || "op-check-foreign".to_owned(),
            || Ok(()),
        )
        .unwrap();
        let error = dispatch_error(
            &fx.rt,
            "installation.control_update",
            json!({"operationId": accepted.operation_id, "action": "cancel"}),
        )
        .await;
        assert_eq!(error, "INVALID_STATE");

        // Verification and later phases accept no actions at all.
        for phase in [
            UpdatePhase::Verifying,
            UpdatePhase::Committing,
            UpdatePhase::Switching,
            UpdatePhase::Finalizing,
        ] {
            let state = update_state();
            *state.phase.lock().unwrap_or_else(|e| e.into_inner()) = phase;
            fx.domain
                .updates
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert("op-phase".to_owned(), Arc::clone(&state));
            for action in ["pause", "resume", "cancel"] {
                let error = dispatch_error(
                    &fx.rt,
                    "installation.control_update",
                    json!({"operationId": "op-phase", "action": action}),
                )
                .await;
                assert_eq!(error, "INVALID_STATE", "action {action} in {phase:?}");
            }
            fx.domain
                .updates
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove("op-phase");
        }
    }

    async fn dispatch_error(rt: &Arc<PublicV1Runtime>, method: &str, input: Value) -> String {
        match rt.dispatch(method, input, Some("key-err"), None).await {
            Err(DispatchError::Application(error)) => error.code,
            other => panic!("expected an application error, got {other:?}"),
        }
    }

    /// Drain deadline: an active run blocks the restart, which fails with
    /// DEADLINE_EXCEEDED and leaves everything running (009 §7.1).
    #[tokio::test]
    async fn restart_drain_deadline_leaves_the_service_running() {
        let fx = fixture("restart-drain").await;
        // Seed one active run directly in the store.
        {
            let conn = rusqlite::Connection::open(fx.home.join("v1").join("state.sqlite")).unwrap();
            conn.execute(
                "INSERT INTO runs (run_id, session_id, project_id, state, revision, \
                 waiting_for_json, effect_status, config_json, input_json, terminal, queue_paused, \
                 created_at, updated_at)
                 VALUES ('run-active-1', 's-1', 'p-1', 'running', 'rev-1', '', 'known', '{}', '{}', 0, 0, '2026-11-18', '2026-11-18')",
                [],
            )
            .unwrap();
        }
        let outcome = run_restart(
            &fx.domain,
            "op-restart-1",
            &fx.root,
            "9.9.9+eeeeeeeeeeee.ffffffffffff",
            300,
        )
        .await;
        match outcome {
            RestartOutcome::Failed { code, effect, .. } => {
                assert_eq!(code, "DEADLINE_EXCEEDED");
                assert_eq!(effect, "not_applied");
            }
            RestartOutcome::Handoff => panic!("the restart must not hand off while draining"),
        }
    }

    /// BUILD_CONFLICT: the expected buildId must match the current
    /// generation, or the handoff refuses (009 §7.1).
    #[tokio::test]
    async fn restart_refuses_a_mismatched_target() {
        let fx = fixture("restart-conflict").await;
        let package = build_package(
            "9.9.9",
            "v2026.11.18",
            "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            "ffffffffffff",
        );
        // Seed a current generation.
        let generation = layout::generation_dir(&fx.root, &package.build_id);
        std::fs::create_dir_all(generation.join("bin")).unwrap();
        std::fs::create_dir_all(generation.join("manifest")).unwrap();
        std::fs::write(generation.join("bin").join("agentcpm-pd"), b"#!/bin/sh\n").unwrap();
        std::fs::write(
            generation.join("manifest").join("release.json"),
            package.manifest_json.to_string(),
        )
        .unwrap();
        layout::switch_current(&fx.root, &package.build_id).unwrap();

        let error = initiate_restart_handoff(
            &fx.domain,
            &fx.root,
            "0.0.0+000000000000.000000000000",
            "installation.restart",
            Some("op-restart-2"),
        )
        .unwrap_err();
        assert_eq!(error.code, "BUILD_CONFLICT");
        // No handoff marker was left behind.
        assert!(layout::read_restart_pending(&fx.home).is_none());
    }

    /// The successor's recovery hook completes a pending restart honestly.
    #[tokio::test]
    async fn recover_pending_restart_completes_the_operation() {
        let fx = fixture("recover").await;
        let package = build_package(
            "9.9.9",
            "v2026.11.18",
            "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            "ffffffffffff",
        );
        let generation = layout::generation_dir(&fx.root, &package.build_id);
        std::fs::create_dir_all(generation.join("bin")).unwrap();
        std::fs::create_dir_all(generation.join("manifest")).unwrap();
        std::fs::write(generation.join("bin").join("agentcpm-pd"), b"#!/bin/sh\n").unwrap();
        std::fs::write(
            generation.join("manifest").join("release.json"),
            package.manifest_json.to_string(),
        )
        .unwrap();
        layout::switch_current(&fx.root, &package.build_id).unwrap();

        let accepted = accept_domain_operation(
            &fx.domain.db,
            "installation.restart",
            &json!({"target": "ownedService", "expectedBuildId": package.build_id}),
            Some("key-recover-1"),
            || "op-restart-3".to_owned(),
            || Ok(()),
        )
        .unwrap();
        mark_domain_operation_running(&fx.domain.db, &accepted.operation_id, "").unwrap();

        layout::write_restart_pending(
            &fx.home,
            &layout::RestartPending {
                operation_id: Some(accepted.operation_id.clone()),
                expected_build_id: package.build_id.clone(),
                origin: "installation.restart".to_owned(),
                data_root: fx.home.display().to_string(),
                install_root: fx.root.display().to_string(),
                previous_pid: 1,
                started_at: "2026-11-18T00:00:00Z".to_owned(),
            },
        )
        .unwrap();

        recover_pending_restart(&fx.rt, &fx.root);
        assert!(layout::read_restart_pending(&fx.home).is_none());
        let conn = rusqlite::Connection::open(fx.home.join("v1").join("state.sqlite")).unwrap();
        let (state, completion): (String, String) = conn
            .query_row(
                "SELECT state, completion_json FROM v1_domain_operations WHERE operation_id = ?1",
                [&accepted.operation_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "succeeded");
        let completion: Value = serde_json::from_str(&completion).unwrap();
        assert_eq!(completion["healthy"], true);
        assert_eq!(completion["buildId"], json!(package.build_id));

        // A mismatched target completes the operation as HEALTH_CHECK_FAILED.
        let accepted = accept_domain_operation(
            &fx.domain.db,
            "installation.restart",
            &json!({"target": "ownedService", "expectedBuildId": "0.0.0+000000000000.000000000000"}),
            Some("key-recover-2"),
            || "op-restart-4".to_owned(),
            || Ok(()),
        )
        .unwrap();
        layout::write_restart_pending(
            &fx.home,
            &layout::RestartPending {
                operation_id: Some(accepted.operation_id.clone()),
                expected_build_id: "0.0.0+000000000000.000000000000".to_owned(),
                origin: "installation.restart".to_owned(),
                data_root: fx.home.display().to_string(),
                install_root: fx.root.display().to_string(),
                previous_pid: 1,
                started_at: "2026-11-18T00:00:00Z".to_owned(),
            },
        )
        .unwrap();
        recover_pending_restart(&fx.rt, &fx.root);
        let (state, completion): (String, String) = conn
            .query_row(
                "SELECT state, completion_json FROM v1_domain_operations WHERE operation_id = ?1",
                [&accepted.operation_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "failed");
        let completion: Value = serde_json::from_str(&completion).unwrap();
        assert_eq!(completion["error"]["code"], "HEALTH_CHECK_FAILED");
    }

    /// Diagnose: the full check set, statuses honest (unsupported for a
    /// source installation, pass for the service we are), the report
    /// published as a redacted artifact (009 §4).
    #[tokio::test]
    async fn diagnose_reports_checks_and_publishes_the_redacted_report() {
        let fx = fixture("diagnose").await;
        // check_service_status reads run/ready.json and compares the pid —
        // this fixture runs the domain in-process, so seed a ready.json
        // that describes this very process.
        std::fs::create_dir_all(fx.home.join("run")).unwrap();
        std::fs::write(
            fx.home.join("run").join("ready.json"),
            json!({
                "pid": std::process::id(),
                "instanceId": "inst-diagnose-fixture",
                "generation": "gen-test-1",
                "protocolVersion": "agentcpm-pd.operation/1",
                "wsUrl": "ws://127.0.0.1:1/v1/ws",
                "startedAt": "2026-11-18T00:00:00Z",
                "serverRevision": "srv-test-1",
            })
            .to_string(),
        )
        .unwrap();
        let completion = perform_diagnose(
            &fx.domain,
            &fx.root,
            &[
                "installation".to_owned(),
                "service".to_owned(),
                "runtime".to_owned(),
                "storage".to_owned(),
                "channels".to_owned(),
                "mcp".to_owned(),
            ],
            false,
        );
        assert_eq!(completion["redacted"], true);
        let artifact_id = completion["reportArtifactId"].as_str().unwrap().to_owned();
        assert!(!artifact_id.is_empty());
        assert_eq!(
            completion["reportArtifact"]["mediaType"],
            json!("application/json")
        );
        let checks = completion["checks"].as_array().unwrap().clone();
        let by_name = |name: &str| {
            checks
                .iter()
                .find(|check| check["name"] == json!(name))
                .unwrap_or_else(|| panic!("missing check `{name}`: {checks:?}"))
                .clone()
        };
        // A source-mode installation reports unsupported, never a green
        // placeholder.
        assert_eq!(by_name("installation.manifest")["status"], "unsupported");
        assert_eq!(by_name("installation.generations")["status"], "pass");
        assert_eq!(by_name("service.status")["status"], "pass");
        assert_eq!(by_name("runtime.health")["status"], "pass");
        assert_eq!(by_name("storage.dataRoot")["status"], "pass");
        assert_eq!(by_name("storage.stateDb")["status"], "pass");
        assert_eq!(by_name("storage.freeSpace")["status"], "pass");
        assert_eq!(by_name("channels.configured")["status"], "pass");
        assert_eq!(by_name("mcp.configured")["status"], "pass");

        // The artifact bytes parse and carry the identity + redaction.
        let bytes =
            crate::public_v1::domains::artifacts::read_artifact_bytes(fx.rt.store(), &artifact_id)
                .unwrap();
        let report: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(report["schemaVersion"], "agentcpm-pd.diagnose-report/1");
        assert_eq!(report["identity"]["productName"], "agentcpm-pd");
        assert_eq!(report["redaction"]["applied"], true);
        // The data root path in the report is abbreviated to `~`.
        let data_root = report["storage"]["dataRoot"].as_str().unwrap();
        assert!(data_root.starts_with('~'), "redacted dataRoot: {data_root}");

        // A stale lock and staging leftovers surface as warnings, and the
        // installation.manifest check turns pass on a managed root.
        let package = build_package(
            "9.9.9",
            "v2026.11.18",
            "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            "ffffffffffff",
        );
        let generation = layout::generation_dir(&fx.root, &package.build_id);
        std::fs::create_dir_all(generation.join("bin")).unwrap();
        std::fs::create_dir_all(generation.join("manifest")).unwrap();
        std::fs::write(generation.join("bin").join("agentcpm-pd"), b"#!/bin/sh\n").unwrap();
        // verify_generation requires the main binary to be executable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                generation.join("bin").join("agentcpm-pd"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        std::fs::write(
            generation.join("manifest").join("release.json"),
            package.manifest_json.to_string(),
        )
        .unwrap();
        // Give the generation a SHA256SUMS so verification passes.
        let sums = release::render_sha256sums(&[
            (
                "bin/agentcpm-pd".to_owned(),
                generation.join("bin").join("agentcpm-pd"),
            ),
            (
                "manifest/release.json".to_owned(),
                generation.join("manifest").join("release.json"),
            ),
        ])
        .unwrap();
        std::fs::write(generation.join("SHA256SUMS.txt"), &sums).unwrap();
        layout::switch_current(&fx.root, &package.build_id).unwrap();
        let _ = std::fs::create_dir_all(layout::update_lock_dir(&fx.root));

        let completion =
            perform_diagnose(&fx.domain, &fx.root, &["installation".to_owned()], false);
        let checks = completion["checks"].as_array().unwrap().clone();
        let manifest_check = checks
            .iter()
            .find(|check| check["name"] == json!("installation.manifest"))
            .unwrap();
        assert_eq!(manifest_check["status"], "pass");
        let generations_check = checks
            .iter()
            .find(|check| check["name"] == json!("installation.generations"))
            .unwrap();
        assert_eq!(generations_check["status"], "warning");
        assert_eq!(generations_check["code"], "UPDATE_STATE_DIRTY");
    }

    /// `installation.inspect` is CLI-local: the dispatch table refuses it
    /// while the capability set still lists it (the availability-only row).
    #[tokio::test]
    async fn local_inspect_is_refused_over_the_wire_but_registered() {
        let fx = fixture("inspect-gate").await;
        assert!(
            crate::public_v1::domains::DomainRegistry::global()
                .is_registered("installation.inspect")
        );
        match fx
            .rt
            .dispatch("installation.inspect", json!({}), None, None)
            .await
        {
            Err(DispatchError::Rpc { code, .. }) => assert_eq!(code, -32601),
            other => panic!("local op must be refused over /v1/ws, got {other:?}"),
        }
    }
}
