//! Session backend ownership pinning, atomic SHA-256 ledger store, and
//! no-fallback Kernel turn failure handling (`P7` migration rules).

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agentcpm_iface::types::{Error, ErrorCategory};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{MigrationError, hash_opt_str, hash_str, hex_encode, validate_session_id};

/// Current on-disk schema version for [`SessionOwnershipStore`].
pub const OWNERSHIP_LEDGER_SCHEMA_VERSION: u32 = 1;

const OWNERSHIP_LEDGER_DOMAIN: &[u8] = b"pilotdeck.session_ownership.ledger.v1";

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Authoritative execution backend owner pinned to a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendOwner {
    LegacyRuntime,
    KernelScopedResident,
    KernelFullLate,
}

impl BackendOwner {
    /// Whether this owner executes through an AgentCPM Kernel runtime.
    pub const fn is_kernel(self) -> bool {
        matches!(self, Self::KernelScopedResident | Self::KernelFullLate)
    }

    /// Whether this owner executes on the legacy PilotDeck runtime.
    pub const fn is_legacy(self) -> bool {
        matches!(self, Self::LegacyRuntime)
    }

    /// Canonical snake_case identifier used in persistence and digests.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LegacyRuntime => "legacy_runtime",
            Self::KernelScopedResident => "kernel_scoped_resident",
            Self::KernelFullLate => "kernel_full_late",
        }
    }

    /// Compact ASCII tag used when deriving target session identifiers.
    pub const fn short_tag(self) -> &'static str {
        match self {
            Self::LegacyRuntime => "legacy",
            Self::KernelScopedResident => "scoped",
            Self::KernelFullLate => "fulllate",
        }
    }
}

impl fmt::Display for BackendOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Immutable per-session ownership pin persisted in [`SessionOwnershipStore`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOwnershipRecord {
    pub session_id: String,
    pub owner: BackendOwner,
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_from_session_id: Option<String>,
}

/// Outcome classification returned by [`handle_kernel_turn_failure`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KernelFailureDisposition {
    /// Turn failed without unknown side effects; surfaced without fallback or replay.
    FailedNoFallback,
    /// Execution outcome is unknown (`ResultUnknown`); reconciliation required, never fallback.
    UnknownReconcileRequired,
}

/// Authentic structured Kernel turn failure returned without silent cross-backend fallback.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KernelTurnFailure {
    pub session_id: String,
    pub turn_id: String,
    pub owner: BackendOwner,
    pub disposition: KernelFailureDisposition,
    pub error_category: String,
    pub error_code: i32,
    pub reason_code: String,
    pub message: String,
    pub retryable: bool,
    pub resource_released: Option<bool>,
    /// Always `false`: P7 forbids silent fallback from Kernel to `LegacyRuntime`.
    pub fallback_attempted: bool,
    /// Always `false`: P7 forbids replaying partial operations across backends.
    pub partial_replay_attempted: bool,
}

impl fmt::Display for KernelTurnFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (session={}, turn={}, owner={}, fallback_attempted={}, partial_replay_attempted={}): {}",
            self.reason_code,
            self.session_id,
            self.turn_id,
            self.owner,
            self.fallback_attempted,
            self.partial_replay_attempted,
            self.message
        )
    }
}

impl std::error::Error for KernelTurnFailure {}

impl KernelTurnFailure {
    /// Convert back to [`agentcpm_iface::types::Error`] preserving category, code, and message.
    pub fn to_iface_error(&self) -> Error {
        let mut err = Error::new(
            parse_category(&self.error_category),
            self.error_code,
            self.message.clone(),
        );
        err.retryable = self.retryable;
        err
    }
}

/// Enforce the P7 no-fallback rule on Kernel turn failure.
///
/// Validates that `owner` is a Kernel backend and agrees with any pinned
/// record in `store`, never mutates `store` or switches ownership to
/// [`BackendOwner::LegacyRuntime`], and returns the authentic structured failure.
pub fn handle_kernel_turn_failure(
    store: &SessionOwnershipStore,
    session_id: &str,
    turn_id: &str,
    owner: BackendOwner,
    error: &Error,
    outcome_unknown: bool,
    resource_released: Option<bool>,
) -> Result<KernelTurnFailure, MigrationError> {
    validate_session_id(session_id)?;
    if turn_id.trim().is_empty() {
        return Err(MigrationError::InvalidSessionId(
            "turn_id must not be empty or whitespace".into(),
        ));
    }
    if !owner.is_kernel() {
        return Err(MigrationError::OwnershipConflict {
            session_id: session_id.to_string(),
            recorded: owner,
            requested: BackendOwner::KernelScopedResident,
        });
    }
    if let Some(record) = store.get(session_id)?
        && record.owner != owner
    {
        return Err(MigrationError::OwnershipConflict {
            session_id: session_id.to_string(),
            recorded: record.owner,
            requested: owner,
        });
    }

    let reason_code = error
        .app_reason()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("KERNEL_ERR_{}", error.code));
    let disposition = if outcome_unknown {
        KernelFailureDisposition::UnknownReconcileRequired
    } else {
        KernelFailureDisposition::FailedNoFallback
    };

    Ok(KernelTurnFailure {
        session_id: session_id.to_string(),
        turn_id: turn_id.to_string(),
        owner,
        disposition,
        error_category: category_name(error.category).to_string(),
        error_code: error.code,
        reason_code,
        message: error.message.clone(),
        retryable: error.retryable,
        resource_released,
        fallback_attempted: false,
        partial_replay_attempted: false,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct OwnershipLedgerFile {
    schema_version: u32,
    default_owner: BackendOwner,
    records: BTreeMap<String, SessionOwnershipRecord>,
    ledger_sha256: String,
}

/// Atomic temp+fsync+rename JSON store preserving per-session [`BackendOwner`] pins
/// with SHA-256 ledger integrity.
#[derive(Clone, Debug)]
pub struct SessionOwnershipStore {
    path: PathBuf,
    default_owner: BackendOwner,
    records: BTreeMap<String, SessionOwnershipRecord>,
    ledger_sha256: String,
}

impl SessionOwnershipStore {
    /// Open or initialize an ownership ledger at `path`.
    ///
    /// - If `path` exists, verifies SHA-256 ledger integrity and preserves every
    ///   existing session pin untouched, adopting `default_owner` solely for
    ///   future new sessions.
    /// - If `path` does not exist, creates parent directories if needed and
    ///   persists an empty SHA-256-verified ledger atomically.
    pub fn open(
        path: impl AsRef<Path>,
        default_owner: BackendOwner,
    ) -> Result<Self, MigrationError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent).map_err(|err| {
                MigrationError::Io(format!(
                    "failed to create ownership store parent dir {}: {err}",
                    parent.display()
                ))
            })?;
        }
        match fs::metadata(&path) {
            Ok(_) => {
                let mut loaded = Self::load(&path)?;
                loaded.default_owner = default_owner;
                Ok(loaded)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let records = BTreeMap::new();
                let ledger_sha256 =
                    compute_ledger_sha256(OWNERSHIP_LEDGER_SCHEMA_VERSION, default_owner, &records);
                let doc = OwnershipLedgerFile {
                    schema_version: OWNERSHIP_LEDGER_SCHEMA_VERSION,
                    default_owner,
                    records: records.clone(),
                    ledger_sha256: ledger_sha256.clone(),
                };
                write_atomic_ledger(&path, &doc)?;
                Ok(Self {
                    path,
                    default_owner,
                    records,
                    ledger_sha256,
                })
            }
            Err(err) => Err(MigrationError::Io(format!(
                "failed to stat ownership store {}: {err}",
                path.display()
            ))),
        }
    }

    /// Load and integrity-verify an existing ownership ledger without modifying it.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, MigrationError> {
        let path = path.as_ref().to_path_buf();
        let bytes = fs::read(&path).map_err(|err| {
            MigrationError::Io(format!(
                "failed to read ownership store {}: {err}",
                path.display()
            ))
        })?;
        let file_doc: OwnershipLedgerFile = serde_json::from_slice(&bytes).map_err(|err| {
            MigrationError::OwnershipLedgerCorrupt(format!(
                "invalid ownership JSON in {}: {err}",
                path.display()
            ))
        })?;

        if file_doc.schema_version != OWNERSHIP_LEDGER_SCHEMA_VERSION {
            return Err(MigrationError::OwnershipLedgerCorrupt(format!(
                "unsupported schema_version {} in {}",
                file_doc.schema_version,
                path.display()
            )));
        }
        if !is_valid_sha256_hex(&file_doc.ledger_sha256) {
            return Err(MigrationError::OwnershipLedgerCorrupt(format!(
                "invalid ledger_sha256 format in {}",
                path.display()
            )));
        }

        for (key, record) in &file_doc.records {
            validate_session_id(key).map_err(|err| {
                MigrationError::OwnershipLedgerCorrupt(format!(
                    "invalid session key {key:?} in {}: {err}",
                    path.display()
                ))
            })?;
            if key != &record.session_id {
                return Err(MigrationError::OwnershipLedgerCorrupt(format!(
                    "key {key:?} != record.session_id {:?} in {}",
                    record.session_id,
                    path.display()
                )));
            }
            if record.generation == 0 {
                return Err(MigrationError::OwnershipLedgerCorrupt(format!(
                    "record {key:?} has generation 0 in {}",
                    path.display()
                )));
            }
            if let Some(src) = &record.source_session_id {
                validate_session_id(src).map_err(|err| {
                    MigrationError::OwnershipLedgerCorrupt(format!(
                        "record {key:?} has invalid source_session_id: {err}"
                    ))
                })?;
            }
            if let Some(digest) = &record.source_sha256
                && !is_valid_sha256_hex(digest)
            {
                return Err(MigrationError::OwnershipLedgerCorrupt(format!(
                    "record {key:?} has invalid source_sha256 digest"
                )));
            }
            if let Some(rb) = &record.rollback_from_session_id {
                validate_session_id(rb).map_err(|err| {
                    MigrationError::OwnershipLedgerCorrupt(format!(
                        "record {key:?} has invalid rollback_from_session_id: {err}"
                    ))
                })?;
            }
        }

        let recomputed = compute_ledger_sha256(
            file_doc.schema_version,
            file_doc.default_owner,
            &file_doc.records,
        );
        if recomputed != file_doc.ledger_sha256 {
            return Err(MigrationError::OwnershipLedgerCorrupt(format!(
                "ledger_sha256 mismatch in {}: recorded={}, recomputed={recomputed}",
                path.display(),
                file_doc.ledger_sha256
            )));
        }

        Ok(Self {
            path,
            default_owner: file_doc.default_owner,
            records: file_doc.records,
            ledger_sha256: file_doc.ledger_sha256,
        })
    }

    /// Default [`BackendOwner`] assigned to newly routed sessions.
    pub fn default_owner(&self) -> BackendOwner {
        self.default_owner
    }

    /// Update `default_owner` for *future* new sessions without altering any existing pin.
    pub fn set_default_owner(&mut self, owner: BackendOwner) -> Result<(), MigrationError> {
        let next_sha = compute_ledger_sha256(OWNERSHIP_LEDGER_SCHEMA_VERSION, owner, &self.records);
        let doc = OwnershipLedgerFile {
            schema_version: OWNERSHIP_LEDGER_SCHEMA_VERSION,
            default_owner: owner,
            records: self.records.clone(),
            ledger_sha256: next_sha.clone(),
        };
        write_atomic_ledger(&self.path, &doc)?;
        self.default_owner = owner;
        self.ledger_sha256 = next_sha;
        Ok(())
    }

    /// Lookup an existing pinned ownership record without mutating the store.
    pub fn get(&self, session_id: &str) -> Result<Option<&SessionOwnershipRecord>, MigrationError> {
        validate_session_id(session_id)?;
        Ok(self.records.get(session_id))
    }

    /// All pinned session records in canonical key order.
    pub fn records(&self) -> &BTreeMap<String, SessionOwnershipRecord> {
        &self.records
    }

    /// Latest verified SHA-256 ledger digest.
    pub fn ledger_sha256(&self) -> &str {
        &self.ledger_sha256
    }

    /// Path to the on-disk ownership ledger file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Route `session_id`: returns its existing pinned owner unchanged, or
    /// pins a brand-new session to `self.default_owner`.
    pub fn route_session(
        &mut self,
        session_id: &str,
    ) -> Result<SessionOwnershipRecord, MigrationError> {
        validate_session_id(session_id)?;
        if let Some(existing) = self.records.get(session_id) {
            return Ok(existing.clone());
        }
        let record = SessionOwnershipRecord {
            session_id: session_id.to_string(),
            owner: self.default_owner,
            generation: 1,
            source_session_id: None,
            source_sha256: None,
            rollback_from_session_id: None,
        };
        self.persist_new_record(record.clone())?;
        Ok(record)
    }

    /// Explicitly pin a new session to `owner`, or verify an existing pin matches `owner`.
    ///
    /// Fails closed with [`MigrationError::OwnershipConflict`] if `session_id` is
    /// already pinned to a different [`BackendOwner`].
    pub fn pin_session(
        &mut self,
        session_id: &str,
        owner: BackendOwner,
    ) -> Result<SessionOwnershipRecord, MigrationError> {
        validate_session_id(session_id)?;
        if let Some(existing) = self.records.get(session_id) {
            if existing.owner != owner {
                return Err(MigrationError::OwnershipConflict {
                    session_id: session_id.to_string(),
                    recorded: existing.owner,
                    requested: owner,
                });
            }
            return Ok(existing.clone());
        }
        let record = SessionOwnershipRecord {
            session_id: session_id.to_string(),
            owner,
            generation: 1,
            source_session_id: None,
            source_sha256: None,
            rollback_from_session_id: None,
        };
        self.persist_new_record(record.clone())?;
        Ok(record)
    }

    /// Verify that if `session_id` is already pinned, it belongs exclusively to `expected`.
    pub fn assert_owner(
        &self,
        session_id: &str,
        expected: BackendOwner,
    ) -> Result<(), MigrationError> {
        if let Some(existing) = self.get(session_id)?
            && existing.owner != expected
        {
            return Err(MigrationError::OwnershipConflict {
                session_id: session_id.to_string(),
                recorded: existing.owner,
                requested: expected,
            });
        }
        Ok(())
    }

    /// Verify `session_id` is already pinned and belongs exclusively to `expected`.
    pub fn require_owner(
        &self,
        session_id: &str,
        expected: BackendOwner,
    ) -> Result<&SessionOwnershipRecord, MigrationError> {
        let Some(existing) = self.get(session_id)? else {
            return Err(MigrationError::OwnershipLedgerCorrupt(format!(
                "session {session_id:?} has no pinned ownership record"
            )));
        };
        if existing.owner != expected {
            return Err(MigrationError::OwnershipConflict {
                session_id: session_id.to_string(),
                recorded: existing.owner,
                requested: expected,
            });
        }
        Ok(existing)
    }

    /// Pre-flight check before importing into `target_session_id`.
    ///
    /// Returns `Ok(true)` if an identical idempotent pin already exists,
    /// `Ok(false)` if `target_session_id` is fresh and ready to be pinned, or
    /// `Err(OwnershipConflict)` if it conflicts with existing ownership or rollback rules.
    pub(crate) fn check_import_target(
        &self,
        target_session_id: &str,
        source_session_id: &str,
        source_sha256: &str,
        target_owner: BackendOwner,
        is_rollback: bool,
    ) -> Result<bool, MigrationError> {
        validate_session_id(target_session_id)?;
        validate_session_id(source_session_id)?;

        if target_session_id == source_session_id {
            if is_rollback {
                return Err(MigrationError::OwnershipConflict {
                    session_id: target_session_id.to_string(),
                    recorded: self
                        .records
                        .get(source_session_id)
                        .map_or(target_owner, |r| r.owner),
                    requested: target_owner,
                });
            }
            if let Some(src_rec) = self.records.get(source_session_id)
                && src_rec.owner != target_owner
            {
                return Err(MigrationError::OwnershipConflict {
                    session_id: target_session_id.to_string(),
                    recorded: src_rec.owner,
                    requested: target_owner,
                });
            }
        }

        if let Some(existing) = self.records.get(target_session_id) {
            if existing.owner != target_owner {
                return Err(MigrationError::OwnershipConflict {
                    session_id: target_session_id.to_string(),
                    recorded: existing.owner,
                    requested: target_owner,
                });
            }
            let expected_rb = is_rollback.then_some(source_session_id);
            let same_provenance = existing.source_session_id.as_deref() == Some(source_session_id)
                && existing.source_sha256.as_deref() == Some(source_sha256)
                && existing.rollback_from_session_id.as_deref() == expected_rb;
            if same_provenance {
                return Ok(true);
            }
            return Err(MigrationError::OwnershipConflict {
                session_id: target_session_id.to_string(),
                recorded: existing.owner,
                requested: target_owner,
            });
        }

        Ok(false)
    }

    pub(crate) fn commit_import_pin(
        &mut self,
        target_session_id: &str,
        source_session_id: &str,
        source_sha256: &str,
        target_owner: BackendOwner,
        is_rollback: bool,
    ) -> Result<SessionOwnershipRecord, MigrationError> {
        let already_pinned = self.check_import_target(
            target_session_id,
            source_session_id,
            source_sha256,
            target_owner,
            is_rollback,
        )?;
        if already_pinned {
            return Ok(self.records[target_session_id].clone());
        }
        let record = SessionOwnershipRecord {
            session_id: target_session_id.to_string(),
            owner: target_owner,
            generation: 1,
            source_session_id: Some(source_session_id.to_string()),
            source_sha256: Some(source_sha256.to_string()),
            rollback_from_session_id: is_rollback.then(|| source_session_id.to_string()),
        };
        self.persist_new_record(record.clone())?;
        Ok(record)
    }

    fn persist_new_record(&mut self, record: SessionOwnershipRecord) -> Result<(), MigrationError> {
        let mut next_records = self.records.clone();
        next_records.insert(record.session_id.clone(), record);
        let next_sha = compute_ledger_sha256(
            OWNERSHIP_LEDGER_SCHEMA_VERSION,
            self.default_owner,
            &next_records,
        );
        let doc = OwnershipLedgerFile {
            schema_version: OWNERSHIP_LEDGER_SCHEMA_VERSION,
            default_owner: self.default_owner,
            records: next_records.clone(),
            ledger_sha256: next_sha.clone(),
        };
        write_atomic_ledger(&self.path, &doc)?;
        self.records = next_records;
        self.ledger_sha256 = next_sha;
        Ok(())
    }
}

fn compute_ledger_sha256(
    schema_version: u32,
    default_owner: BackendOwner,
    records: &BTreeMap<String, SessionOwnershipRecord>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(OWNERSHIP_LEDGER_DOMAIN);
    hasher.update(schema_version.to_le_bytes());
    hash_str(&mut hasher, default_owner.as_str());
    hasher.update((records.len() as u64).to_le_bytes());
    for (key, rec) in records {
        hash_str(&mut hasher, key);
        hash_str(&mut hasher, &rec.session_id);
        hash_str(&mut hasher, rec.owner.as_str());
        hasher.update(rec.generation.to_le_bytes());
        hash_opt_str(&mut hasher, rec.source_session_id.as_deref());
        hash_opt_str(&mut hasher, rec.source_sha256.as_deref());
        hash_opt_str(&mut hasher, rec.rollback_from_session_id.as_deref());
    }
    hex_encode(&hasher.finalize())
}

fn is_valid_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

struct TempFileGuard {
    path: PathBuf,
    armed: bool,
}

impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn write_atomic_ledger(path: &Path, doc: &OwnershipLedgerFile) -> Result<(), MigrationError> {
    let mut bytes = serde_json::to_vec_pretty(doc).map_err(|err| {
        MigrationError::Io(format!("failed to serialize ownership ledger JSON: {err}"))
    })?;
    bytes.push(b'\n');

    let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("ownership");
    let temp_path = path.with_file_name(format!(".{file_name}.{pid}.{seq}.tmp"));

    let mut open = OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    open.mode(0o600);

    let mut file = open.open(&temp_path).map_err(|err| {
        MigrationError::Io(format!(
            "failed to create staging ownership file {}: {err}",
            temp_path.display()
        ))
    })?;
    let mut guard = TempFileGuard::new(temp_path.clone());

    file.write_all(&bytes).map_err(|err| {
        MigrationError::Io(format!(
            "failed to write staging ownership file {}: {err}",
            temp_path.display()
        ))
    })?;
    file.sync_all().map_err(|err| {
        MigrationError::Io(format!(
            "failed to fsync staging ownership file {}: {err}",
            temp_path.display()
        ))
    })?;
    drop(file);

    fs::rename(&temp_path, path).map_err(|err| {
        MigrationError::Io(format!(
            "failed to rename staging ownership file {} to {}: {err}",
            temp_path.display(),
            path.display()
        ))
    })?;
    guard.disarm();
    Ok(())
}

fn category_name(category: ErrorCategory) -> &'static str {
    match category {
        ErrorCategory::Transport => "transport",
        ErrorCategory::Protocol => "protocol",
        ErrorCategory::Policy => "policy",
        ErrorCategory::Execution => "execution",
        ErrorCategory::Environment => "environment",
        ErrorCategory::Resource => "resource",
        ErrorCategory::Internal => "internal",
        _ => "internal",
    }
}

fn parse_category(name: &str) -> ErrorCategory {
    match name {
        "transport" => ErrorCategory::Transport,
        "protocol" => ErrorCategory::Protocol,
        "policy" => ErrorCategory::Policy,
        "execution" => ErrorCategory::Execution,
        "environment" => ErrorCategory::Environment,
        "resource" => ErrorCategory::Resource,
        _ => ErrorCategory::Internal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(label: &str) -> PathBuf {
        let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("pd-mig-own-{label}-{}-{seq}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn new_sessions_route_to_configurable_default_owner() {
        let dir = scratch_dir("route");
        let path = dir.join("ownership.json");

        let mut store =
            SessionOwnershipStore::open(&path, BackendOwner::KernelScopedResident).unwrap();
        let r1 = store.route_session("sess-new-1").unwrap();
        assert_eq!(r1.owner, BackendOwner::KernelScopedResident);
        assert_eq!(r1.generation, 1);

        store
            .set_default_owner(BackendOwner::KernelFullLate)
            .unwrap();
        let r2 = store.route_session("sess-new-2").unwrap();
        assert_eq!(r2.owner, BackendOwner::KernelFullLate);

        // Existing session stays pinned to its recorded owner despite default flip.
        let r1_again = store.route_session("sess-new-1").unwrap();
        assert_eq!(r1_again.owner, BackendOwner::KernelScopedResident);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn existing_pins_survive_reopen_with_new_default_and_reject_cross_owner() {
        let dir = scratch_dir("pin");
        let path = dir.join("ownership.json");

        {
            let mut store =
                SessionOwnershipStore::open(&path, BackendOwner::LegacyRuntime).unwrap();
            let rec = store.route_session("sess-legacy").unwrap();
            assert_eq!(rec.owner, BackendOwner::LegacyRuntime);
        }

        // Re-open with KernelScopedResident as default for future new sessions.
        let mut reopened =
            SessionOwnershipStore::open(&path, BackendOwner::KernelScopedResident).unwrap();
        let existing = reopened.route_session("sess-legacy").unwrap();
        assert_eq!(existing.owner, BackendOwner::LegacyRuntime);

        let new_sess = reopened.route_session("sess-fresh").unwrap();
        assert_eq!(new_sess.owner, BackendOwner::KernelScopedResident);

        let err = reopened
            .pin_session("sess-legacy", BackendOwner::KernelScopedResident)
            .unwrap_err();
        assert!(matches!(
            err,
            MigrationError::OwnershipConflict {
                recorded: BackendOwner::LegacyRuntime,
                requested: BackendOwner::KernelScopedResident,
                ..
            }
        ));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tampered_or_truncated_ownership_ledger_fails_closed() {
        let dir = scratch_dir("tamper");
        let path = dir.join("ownership.json");

        {
            let mut store =
                SessionOwnershipStore::open(&path, BackendOwner::KernelScopedResident).unwrap();
            store.route_session("sess-a").unwrap();
        }

        let raw = fs::read_to_string(&path).unwrap();
        let tampered = raw.replace("kernel_scoped_resident", "legacy_runtime");
        fs::write(&path, tampered).unwrap();

        let err = SessionOwnershipStore::load(&path).unwrap_err();
        assert!(matches!(err, MigrationError::OwnershipLedgerCorrupt(_)));

        // Truncated JSON also fails closed.
        fs::write(&path, &raw[..raw.len() / 2]).unwrap();
        let err = SessionOwnershipStore::open(&path, BackendOwner::LegacyRuntime).unwrap_err();
        assert!(matches!(err, MigrationError::OwnershipLedgerCorrupt(_)));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn kernel_turn_failure_never_falls_back_or_replays_to_legacy() {
        let dir = scratch_dir("nofallback");
        let path = dir.join("ownership.json");

        let mut store =
            SessionOwnershipStore::open(&path, BackendOwner::KernelScopedResident).unwrap();
        store.route_session("sess-k").unwrap();

        let iface_err = Error::app(
            ErrorCategory::Execution,
            "MODEL_TIMEOUT",
            "upstream timeout",
        );
        let failure = handle_kernel_turn_failure(
            &store,
            "sess-k",
            "turn-1",
            BackendOwner::KernelScopedResident,
            &iface_err,
            true,
            Some(false),
        )
        .unwrap();

        assert_eq!(
            failure.disposition,
            KernelFailureDisposition::UnknownReconcileRequired
        );
        assert_eq!(failure.reason_code, "MODEL_TIMEOUT");
        assert!(!failure.fallback_attempted);
        assert!(!failure.partial_replay_attempted);
        assert_eq!(failure.resource_released, Some(false));
        // Store remains strictly pinned to KernelScopedResident.
        assert_eq!(
            store.get("sess-k").unwrap().unwrap().owner,
            BackendOwner::KernelScopedResident
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
