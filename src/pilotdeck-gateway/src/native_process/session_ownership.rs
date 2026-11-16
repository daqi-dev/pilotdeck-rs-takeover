//! P7 session ownership enforcement, no-fallback failure recording, and
//! stationary `.jsonl` import / cross-backend rollback helpers for Gateway.
use crate::{
    runtime::{
        interaction::GatewayInteractionCoordinator,
        project::RuntimeError,
        session::{SessionOwner, factory::try_writer_lease, transcript_locks},
    },
    storage::sanitize_session_id,
};
use pilotdeck_protocol::{JsString, JsValue, parse_json, stringify_json};
use pilotdeck_session_migration::{
    BackendOwner, ContextStateAuthority, ImportOptions, LiveSessionActivity, MigrationError,
    SessionIdentityMapping, SessionOwnershipRecord, SessionOwnershipStore,
    audit_jsonl_transcript_for_session, derive_import_target_session_id,
    derive_rollback_target_session_id, import_for_rollback, import_stationary_session_with_options,
};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

#[cfg(feature = "kernel-managed-model")]
use std::sync::Arc;

#[cfg(feature = "kernel-managed-model")]
use crate::{
    host::{
        Cancellation, ExecuteRequest, ExecuteResult, HostError, HostErrorKind, HostFuture,
        HostHandler,
    },
    runtime::turn::execution_backend::{
        TurnExecutionBackend, TurnExecutionClient, TurnExecutionFacts,
    },
};
#[cfg(feature = "kernel-managed-model")]
use agentcpm_iface::types::{Error as IfaceError, ErrorCategory};
#[cfg(feature = "kernel-managed-model")]
use pilotdeck_session_migration::{KernelTurnFailure, handle_kernel_turn_failure};

pub const OWNERSHIP_DIR: &str = "kernel-ownership";
pub const OWNERSHIP_FILE: &str = "sessions.json";

static OWNERSHIP_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static TEMP_FILE_SEQ: AtomicU64 = AtomicU64::new(1);

pub struct GatewayMigrationOutcome {
    pub mapping: SessionIdentityMapping,
    pub audit_entry_count: usize,
    pub audit_message_count: usize,
    pub source_transcript_path: PathBuf,
    pub target_transcript_path: PathBuf,
    pub ledger: ContextStateAuthority,
}

impl std::fmt::Debug for GatewayMigrationOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayMigrationOutcome")
            .field("mapping", &self.mapping)
            .field("audit_entry_count", &self.audit_entry_count)
            .field("audit_message_count", &self.audit_message_count)
            .field("source_transcript_path", &self.source_transcript_path)
            .field("target_transcript_path", &self.target_transcript_path)
            .finish_non_exhaustive()
    }
}

pub fn ownership_store_path(pilot_home: &Path) -> PathBuf {
    pilot_home.join(OWNERSHIP_DIR).join(OWNERSHIP_FILE)
}

fn lock_ownership() -> std::sync::MutexGuard<'static, ()> {
    OWNERSHIP_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

fn validate_canonical_id(id: &str) -> bool {
    !id.trim().is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
}

/// Normalize a Gateway `sessionKey` into an ownership-store key.
///
/// Kernel sessions require clean UTF-8 within 256 bytes without control
/// characters (`fail-closed`). Legacy TS-parity probes containing lone UTF-16
/// surrogates or control characters map deterministically into a `legacy-raw:`
/// digest key so baseline probes keep their exact TS error behavior while still
/// enforcing cross-backend isolation.
pub fn normalize_session_id_for_ownership(
    session_key: &JsString,
    owner: BackendOwner,
) -> Result<String, RuntimeError> {
    if let Ok(utf8) = session_key.to_utf8()
        && validate_canonical_id(&utf8)
    {
        return Ok(utf8);
    }
    if owner.is_kernel() {
        return Err(RuntimeError::new(
            "MIGRATION_INVALID_SESSION_ID: Kernel sessions require valid UTF-8 session_id <= 256 bytes without control characters.",
        ));
    }
    let mut hasher = Sha256::new();
    hasher.update(b"pilotdeck.legacy_raw_session_key.v1");
    for unit in session_key.units() {
        hasher.update(unit.to_le_bytes());
    }
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(format!("legacy-raw:{digest}"))
}

/// Fail closed immediately if a pre-existing ownership ledger on disk has been
/// tampered with or truncated. Never creates files when no ledger exists yet.
pub fn validate_existing_ownership_ledger(pilot_home: &Path) -> Result<(), RuntimeError> {
    let path = ownership_store_path(pilot_home);
    let _guard = lock_ownership();
    match fs::metadata(&path) {
        Ok(_) => {
            SessionOwnershipStore::load(&path).map_err(|err| RuntimeError::new(err.to_string()))?;
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(RuntimeError::new(format!(
            "MIGRATION_OWNERSHIP_CORRUPT: cannot inspect {}: {err}",
            path.display()
        ))),
    }
}

/// Read an existing session ownership record without creating or mutating state.
pub fn load_session_owner(
    pilot_home: &Path,
    session_id: &str,
) -> Result<Option<SessionOwnershipRecord>, RuntimeError> {
    if !validate_canonical_id(session_id) {
        return Err(RuntimeError::new(
            MigrationError::InvalidSessionId("session_id is invalid for ownership lookup".into())
                .to_string(),
        ));
    }
    let path = ownership_store_path(pilot_home);
    let _guard = lock_ownership();
    match fs::metadata(&path) {
        Ok(_) => {
            let store = SessionOwnershipStore::load(&path)
                .map_err(|err| RuntimeError::new(err.to_string()))?;
            let found = store
                .get(session_id)
                .map_err(|err| RuntimeError::new(err.to_string()))?
                .cloned();
            Ok(found)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(RuntimeError::new(format!(
            "MIGRATION_IO_ERROR: cannot read ownership store {}: {err}",
            path.display()
        ))),
    }
}

/// Enforce P7 ownership isolation and pin newly created sessions.
///
/// - Existing record with a different owner -> fails closed with `MIGRATION_OWNERSHIP_CONFLICT`.
/// - Unpinned non-ephemeral session running on Kernel that already has a
///   non-empty legacy `.jsonl` transcript on disk -> fails closed until audited
///   and imported via stationary migration.
/// - Fresh session -> atomically pinned to `owner`.
pub(crate) fn admit_or_pin_session(
    pilot_home: &Path,
    project_root: &Path,
    session_key: &JsString,
    ephemeral: bool,
    owner: BackendOwner,
) -> Result<SessionOwnershipRecord, RuntimeError> {
    let session_id = normalize_session_id_for_ownership(session_key, owner)?;
    let path = ownership_store_path(pilot_home);
    let _guard = lock_ownership();
    let mut store = SessionOwnershipStore::open(&path, owner)
        .map_err(|err| RuntimeError::new(err.to_string()))?;

    if let Some(existing) = store
        .get(&session_id)
        .map_err(|err| RuntimeError::new(err.to_string()))?
    {
        if existing.owner != owner {
            return Err(RuntimeError::new(
                MigrationError::OwnershipConflict {
                    session_id,
                    recorded: existing.owner,
                    requested: owner,
                }
                .to_string(),
            ));
        }
        return Ok(existing.clone());
    }

    if owner.is_kernel() && !ephemeral {
        let legacy_path =
            transcript_locks::session_transcript_path(pilot_home, project_root, session_key);
        if fs::metadata(&legacy_path).is_ok_and(|meta| meta.is_file() && meta.len() > 0) {
            return Err(RuntimeError::new(format!(
                "MIGRATION_NOT_STATIONARY: session {session_id:?} already has an un-imported legacy transcript at {} and cannot silently mix onto {owner}; run stationary migration first.",
                legacy_path.display()
            )));
        }
    }

    store
        .pin_session(&session_id, owner)
        .map_err(|err| RuntimeError::new(err.to_string()))
}

/// Observe live Gateway activity flags for `session_key` before stationary import/rollback.
pub fn observe_live_session_activity(
    owner: &SessionOwner,
    interactions: &GatewayInteractionCoordinator,
    pilot_home: &Path,
    project_root: &Path,
    session_key: &JsString,
) -> Result<LiveSessionActivity, RuntimeError> {
    let transcript_path =
        transcript_locks::session_transcript_path(pilot_home, project_root, session_key);
    let lease_busy = if fs::metadata(&transcript_path).is_ok_and(|meta| meta.is_file()) {
        try_writer_lease(&transcript_path)?.is_none()
    } else {
        false
    };
    let has_active_writer_lease = owner.current_handle(session_key).is_some()
        || owner.has_active_turn(session_key)
        || lease_busy;
    Ok(LiveSessionActivity {
        has_active_writer_lease,
        pending_permission_count: interactions.permission_bus().pending_count(session_key),
        pending_question_count: interactions.elicitation_bus().pending_count(session_key),
    })
}

fn write_atomic_new_jsonl(path: &Path, bytes: &[u8]) -> Result<(), RuntimeError> {
    let Some(parent) = path.parent() else {
        return Err(RuntimeError::new(
            "transcript target path requires parent directory",
        ));
    };
    fs::create_dir_all(parent).map_err(|err| {
        RuntimeError::new(format!(
            "failed to create transcript directory {}: {err}",
            parent.display()
        ))
    })?;
    let seq = TEMP_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
    let temp = path.with_file_name(format!(".mig.{}.{seq}.tmp", std::process::id()));
    let mut open = OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(0o600);
    }
    let mut file = open.open(&temp).map_err(|err| {
        RuntimeError::new(format!(
            "failed to create staging transcript {}: {err}",
            temp.display()
        ))
    })?;
    if let Err(err) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(&temp);
        return Err(RuntimeError::new(format!(
            "failed to write staging transcript {}: {err}",
            temp.display()
        )));
    }
    drop(file);
    match fs::hard_link(&temp, path) {
        Ok(()) => {
            let _ = fs::remove_file(&temp);
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&temp);
        }
        Err(_) => {
            if let Err(err) = fs::rename(&temp, path) {
                let _ = fs::remove_file(&temp);
                return Err(RuntimeError::new(format!(
                    "failed to persist migrated transcript {}: {err}",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

fn rewrite_transcript_session_id(
    raw_jsonl: &[u8],
    target_session_id: &str,
) -> Result<Vec<u8>, RuntimeError> {
    let text = std::str::from_utf8(raw_jsonl)
        .map_err(|err| RuntimeError::new(format!("transcript is not valid UTF-8: {err}")))?;
    let target_js = JsValue::String(JsString::from(target_session_id));
    let mut out = Vec::with_capacity(raw_jsonl.len() + 64);
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut parsed = parse_json(line)
            .map_err(|err| RuntimeError::new(format!("failed to parse JSONL entry: {err}")))?;
        let JsValue::Object(ref mut obj) = parsed else {
            return Err(RuntimeError::new("JSONL entry must be a JSON object"));
        };
        obj.insert("sessionId".into(), target_js.clone());
        let encoded = stringify_json(&parsed)
            .map_err(|err| RuntimeError::new(format!("failed to serialize JSONL entry: {err}")))?
            .ok_or_else(|| RuntimeError::new("serialized JSONL entry yielded undefined"))?;
        out.extend_from_slice(encoded.as_bytes());
        out.push(b'\n');
    }
    Ok(out)
}

/// Audit a stationary `.jsonl` session and idempotently import it into an
/// isolated target Kernel session identity (`mig:...`), preserving source
/// transcript and source pin untouched.
pub fn import_stationary_session_into_gateway(
    pilot_home: &Path,
    project_root: &Path,
    source_session_id: &str,
    activity: &LiveSessionActivity,
    target_owner: BackendOwner,
) -> Result<GatewayMigrationOutcome, RuntimeError> {
    if !target_owner.is_kernel() {
        return Err(RuntimeError::new(
            "stationary import targets a Kernel backend owner; use rollback_session_in_gateway for LegacyRuntime",
        ));
    }
    let source_js = JsString::from(source_session_id);
    let source_path =
        transcript_locks::session_transcript_path(pilot_home, project_root, &source_js);
    let raw_jsonl = fs::read(&source_path).map_err(|err| {
        RuntimeError::new(format!(
            "MIGRATION_IO_ERROR: cannot read stationary source transcript {}: {err}",
            source_path.display()
        ))
    })?;
    let audit = audit_jsonl_transcript_for_session(&raw_jsonl, source_session_id, activity)
        .map_err(|err| RuntimeError::new(err.to_string()))?;

    let target_id =
        derive_import_target_session_id(source_session_id, &audit.source_sha256, target_owner)
            .map_err(|err| RuntimeError::new(err.to_string()))?;
    let ledger = ContextStateAuthority::new(target_id.clone());

    let store_path = ownership_store_path(pilot_home);
    let mapping = {
        let _guard = lock_ownership();
        let mut store = SessionOwnershipStore::open(&store_path, target_owner)
            .map_err(|err| RuntimeError::new(err.to_string()))?;
        if let Some(existing) = store
            .get(source_session_id)
            .map_err(|err| RuntimeError::new(err.to_string()))?
            && existing.owner == target_owner
        {
            return Err(RuntimeError::new(
                MigrationError::OwnershipConflict {
                    session_id: source_session_id.to_owned(),
                    recorded: existing.owner,
                    requested: target_owner,
                }
                .to_string(),
            ));
        }
        import_stationary_session_with_options(
            &raw_jsonl,
            activity,
            &ledger,
            &mut store,
            target_owner,
            &ImportOptions::default(),
        )
        .map_err(|err| RuntimeError::new(err.to_string()))?
    };

    let target_js = JsString::from(mapping.target_session_id.as_str());
    let target_path =
        transcript_locks::session_transcript_path(pilot_home, project_root, &target_js);
    if !target_path.exists() {
        let rewritten = rewrite_transcript_session_id(&raw_jsonl, &mapping.target_session_id)?;
        write_atomic_new_jsonl(&target_path, &rewritten)?;
    }

    Ok(GatewayMigrationOutcome {
        mapping,
        audit_entry_count: audit.entry_count,
        audit_message_count: audit.message_count,
        source_transcript_path: source_path,
        target_transcript_path: target_path,
        ledger,
    })
}

/// Audit a stationary `.jsonl` session and idempotently create a distinct
/// cross-backend rollback session (`rb:...`), pinning `target_session_id` to
/// `target_owner` (typically `BackendOwner::LegacyRuntime`) without modifying
/// the source transcript or source Kernel pin.
pub fn rollback_session_in_gateway(
    pilot_home: &Path,
    project_root: &Path,
    source_session_id: &str,
    activity: &LiveSessionActivity,
    target_owner: BackendOwner,
    attempt: u64,
) -> Result<GatewayMigrationOutcome, RuntimeError> {
    let source_js = JsString::from(source_session_id);
    let source_path =
        transcript_locks::session_transcript_path(pilot_home, project_root, &source_js);
    let raw_jsonl = fs::read(&source_path).map_err(|err| {
        RuntimeError::new(format!(
            "MIGRATION_IO_ERROR: cannot read stationary rollback transcript {}: {err}",
            source_path.display()
        ))
    })?;
    let audit = audit_jsonl_transcript_for_session(&raw_jsonl, source_session_id, activity)
        .map_err(|err| RuntimeError::new(err.to_string()))?;

    let target_id = derive_rollback_target_session_id(
        source_session_id,
        &audit.source_sha256,
        target_owner,
        attempt,
    )
    .map_err(|err| RuntimeError::new(err.to_string()))?;
    let ledger = ContextStateAuthority::new(target_id.clone());

    let store_path = ownership_store_path(pilot_home);
    let mapping = {
        let _guard = lock_ownership();
        let mut store = SessionOwnershipStore::open(&store_path, target_owner)
            .map_err(|err| RuntimeError::new(err.to_string()))?;
        import_for_rollback(&raw_jsonl, activity, &ledger, &mut store, target_owner)
            .map_err(|err| RuntimeError::new(err.to_string()))?
    };

    let target_js = JsString::from(mapping.target_session_id.as_str());
    let target_path =
        transcript_locks::session_transcript_path(pilot_home, project_root, &target_js);
    if !target_path.exists() {
        let rewritten = rewrite_transcript_session_id(&raw_jsonl, &mapping.target_session_id)?;
        write_atomic_new_jsonl(&target_path, &rewritten)?;
    }

    let _ = sanitize_session_id(&source_js);
    Ok(GatewayMigrationOutcome {
        mapping,
        audit_entry_count: audit.entry_count,
        audit_message_count: audit.message_count,
        source_transcript_path: source_path,
        target_transcript_path: target_path,
        ledger,
    })
}

#[cfg(feature = "kernel-managed-model")]
fn iface_error_from_host_error(error: &HostError) -> IfaceError {
    let category = match error.kind {
        HostErrorKind::Disconnected => ErrorCategory::Transport,
        HostErrorKind::Protocol | HostErrorKind::Limit => ErrorCategory::Protocol,
        HostErrorKind::Io | HostErrorKind::Timeout => ErrorCategory::Environment,
        HostErrorKind::ResultUnknown | HostErrorKind::LoopFailure => ErrorCategory::Execution,
        HostErrorKind::Callback | HostErrorKind::Session => ErrorCategory::Internal,
    };
    let message = if error.diagnostic.is_empty() {
        error.message.clone()
    } else {
        format!("{} ({})", error.message, error.diagnostic)
    };
    let mut iface = IfaceError::new(category, -32000, message);
    iface.retryable = false;
    iface
}

/// Enforce the P7 no-fallback contract when any Kernel turn or prepare step fails.
#[cfg(feature = "kernel-managed-model")]
pub(crate) fn record_kernel_turn_failure(
    pilot_home: &Path,
    session_key: &JsString,
    turn_id: &str,
    owner: BackendOwner,
    error: &HostError,
) -> Result<KernelTurnFailure, RuntimeError> {
    let session_id = normalize_session_id_for_ownership(session_key, owner)?;
    let iface_error = iface_error_from_host_error(error);
    let outcome_unknown = matches!(error.kind, HostErrorKind::ResultUnknown);
    let turn_id = if turn_id.trim().is_empty() {
        "kernel-turn"
    } else {
        turn_id
    };

    let path = ownership_store_path(pilot_home);
    let _guard = lock_ownership();
    let store = SessionOwnershipStore::open(&path, owner)
        .map_err(|err| RuntimeError::new(err.to_string()))?;
    handle_kernel_turn_failure(
        &store,
        &session_id,
        turn_id,
        owner,
        &iface_error,
        outcome_unknown,
        None,
    )
    .map_err(|err| RuntimeError::new(err.to_string()))
}

#[cfg(feature = "kernel-managed-model")]
pub(crate) struct OwnershipEnforcedTurnBackend {
    pub inner: Arc<dyn TurnExecutionBackend>,
    pub pilot_home: PathBuf,
    pub owner: BackendOwner,
    pub session_key: JsString,
}

#[cfg(feature = "kernel-managed-model")]
impl TurnExecutionBackend for OwnershipEnforcedTurnBackend {
    fn prepare(&self, facts: TurnExecutionFacts) -> HostFuture<Box<dyn TurnExecutionClient>> {
        let inner = self.inner.clone();
        let pilot_home = self.pilot_home.clone();
        let owner = self.owner;
        let session_key = self.session_key.clone();
        let turn_id = facts.identity.turn_id.to_utf8().unwrap_or_default();
        Box::pin(async move {
            match inner.prepare(facts).await {
                Ok(client) => Ok(Box::new(OwnershipEnforcedTurnClient {
                    inner: client,
                    pilot_home,
                    owner,
                    session_key,
                }) as Box<dyn TurnExecutionClient>),
                Err(mut err) => {
                    if let Err(ownership_err) =
                        record_kernel_turn_failure(&pilot_home, &session_key, &turn_id, owner, &err)
                    {
                        err.message = format!("{}; {ownership_err}", err.message);
                    }
                    Err(err)
                }
            }
        })
    }
}

#[cfg(feature = "kernel-managed-model")]
struct OwnershipEnforcedTurnClient {
    inner: Box<dyn TurnExecutionClient>,
    pilot_home: PathBuf,
    owner: BackendOwner,
    session_key: JsString,
}

#[cfg(feature = "kernel-managed-model")]
impl TurnExecutionClient for OwnershipEnforcedTurnClient {
    fn module_id(&self) -> JsString {
        self.inner.module_id()
    }

    fn execute(
        self: Box<Self>,
        request: ExecuteRequest,
        handler: Arc<dyn HostHandler>,
        cancellation: Cancellation,
    ) -> HostFuture<ExecuteResult> {
        let turn_id = request
            .identity
            .turn_id
            .to_utf8()
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| request.identity.run_id.to_utf8().ok())
            .unwrap_or_else(|| "kernel-turn".to_owned());
        let pilot_home = self.pilot_home;
        let owner = self.owner;
        let session_key = self.session_key;
        let inner = self.inner;
        Box::pin(async move {
            match inner.execute(request, handler, cancellation).await {
                Ok(result) => Ok(result),
                Err(mut err) => {
                    match record_kernel_turn_failure(
                        &pilot_home,
                        &session_key,
                        &turn_id,
                        owner,
                        &err,
                    ) {
                        Ok(failure) => {
                            if err.diagnostic.is_empty() {
                                err.diagnostic = format!(
                                    "owner={} disposition={:?} reason={}",
                                    failure.owner, failure.disposition, failure.reason_code
                                );
                            }
                        }
                        Err(ownership_err) => {
                            err.message = format!("{}; {ownership_err}", err.message);
                        }
                    }
                    Err(err)
                }
            }
        })
    }
}
