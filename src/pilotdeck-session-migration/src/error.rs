//! Shared fail-closed errors and canonical digest/ID validation helpers.

use std::fmt;

use agentcpm_iface::types::{Error, ErrorCategory};
use pilotdeck_context_iface::LedgerError;
use sha2::{Digest, Sha256};

use crate::ownership::BackendOwner;

/// Maximum length in bytes accepted for session identifiers.
pub const MAX_SESSION_ID_BYTES: usize = 256;

/// Fail-closed error surface for session ownership, stationary audit, and transcript import.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MigrationError {
    /// Session identifier is empty, blank, overlong, or contains control characters.
    InvalidSessionId(String),
    /// Existing session is pinned to a different [`BackendOwner`] or rollback target collides.
    OwnershipConflict {
        session_id: String,
        recorded: BackendOwner,
        requested: BackendOwner,
    },
    /// Ownership ledger JSON failed parsing, schema checks, or SHA-256 verification.
    OwnershipLedgerCorrupt(String),
    /// Live activity or unclosed transcript state shows the session is not stationary.
    SessionNotStationary(String),
    /// JSONL transcript contains invalid UTF-8, truncated/malformed JSON, or invalid structure.
    CorruptedTranscript(String),
    /// Authoritative context state authority import failed or conflicted.
    ImportFailed(String),
    /// Underlying [`LedgerError`] surfaced by [`pilotdeck_context_iface::AuthoritativeContextLedger`].
    Ledger(LedgerError),
    /// Filesystem I/O failure while reading transcripts or persisting the ownership ledger.
    Io(String),
}

impl fmt::Display for MigrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSessionId(detail) => {
                write!(f, "MIGRATION_INVALID_SESSION_ID: {detail}")
            }
            Self::OwnershipConflict {
                session_id,
                recorded,
                requested,
            } => {
                write!(
                    f,
                    "MIGRATION_OWNERSHIP_CONFLICT: session {session_id:?} is pinned to {recorded}, cannot route or reassign to {requested}"
                )
            }
            Self::OwnershipLedgerCorrupt(detail) => {
                write!(f, "MIGRATION_OWNERSHIP_CORRUPT: {detail}")
            }
            Self::SessionNotStationary(detail) => {
                write!(f, "MIGRATION_NOT_STATIONARY: {detail}")
            }
            Self::CorruptedTranscript(detail) => {
                write!(f, "MIGRATION_TRANSCRIPT_CORRUPT: {detail}")
            }
            Self::ImportFailed(detail) => {
                write!(f, "MIGRATION_IMPORT_FAILED: {detail}")
            }
            Self::Ledger(err) => write!(f, "MIGRATION_LEDGER_ERROR: {err}"),
            Self::Io(detail) => write!(f, "MIGRATION_IO_ERROR: {detail}"),
        }
    }
}

impl std::error::Error for MigrationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Ledger(err) => Some(err),
            _ => None,
        }
    }
}

impl From<LedgerError> for MigrationError {
    fn from(error: LedgerError) -> Self {
        Self::Ledger(error)
    }
}

impl MigrationError {
    /// Convert to a stable reason-prefixed [`agentcpm_iface::types::Error`] (`-32000`).
    pub fn to_iface_error(&self) -> Error {
        match self {
            Self::InvalidSessionId(detail) => Error::app(
                ErrorCategory::Protocol,
                "MIGRATION_INVALID_SESSION_ID",
                detail,
            ),
            Self::OwnershipConflict { .. } => Error::app(
                ErrorCategory::Policy,
                "MIGRATION_OWNERSHIP_CONFLICT",
                self.to_string(),
            ),
            Self::OwnershipLedgerCorrupt(detail) => Error::app(
                ErrorCategory::Internal,
                "MIGRATION_OWNERSHIP_CORRUPT",
                detail,
            ),
            Self::SessionNotStationary(detail) => {
                Error::app(ErrorCategory::Policy, "MIGRATION_NOT_STATIONARY", detail)
            }
            Self::CorruptedTranscript(detail) => Error::app(
                ErrorCategory::Protocol,
                "MIGRATION_TRANSCRIPT_CORRUPT",
                detail,
            ),
            Self::ImportFailed(detail) => {
                Error::app(ErrorCategory::Execution, "MIGRATION_IMPORT_FAILED", detail)
            }
            Self::Ledger(err) => err.to_iface_error(),
            Self::Io(detail) => Error::app(ErrorCategory::Internal, "MIGRATION_IO_ERROR", detail),
        }
    }
}

/// Validate a session identifier against empty/blank, length, and control-character bounds.
pub(crate) fn validate_session_id(session_id: &str) -> Result<(), MigrationError> {
    if session_id.trim().is_empty() {
        return Err(MigrationError::InvalidSessionId(
            "session_id must not be empty or whitespace".into(),
        ));
    }
    if session_id.len() > MAX_SESSION_ID_BYTES {
        return Err(MigrationError::InvalidSessionId(format!(
            "session_id length {} exceeds {MAX_SESSION_ID_BYTES} bytes",
            session_id.len()
        )));
    }
    if session_id.chars().any(char::is_control) {
        return Err(MigrationError::InvalidSessionId(
            "session_id must not contain control characters".into(),
        ));
    }
    Ok(())
}

/// Lowercase hexadecimal SHA-256 digest of `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex_encode(&digest)
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

pub(crate) fn hash_str(hasher: &mut Sha256, value: &str) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}

pub(crate) fn hash_opt_str(hasher: &mut Sha256, value: Option<&str>) {
    match value {
        Some(text) => {
            hasher.update([1]);
            hash_str(hasher, text);
        }
        None => hasher.update([0]),
    }
}
