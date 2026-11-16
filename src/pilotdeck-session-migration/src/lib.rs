//! Session ownership pinning, stationary `.jsonl` transcript audit, and
//! lossless cross-backend migration / rollback import (`P7` migration rules).
//!
//! # Honest Scope & Boundary
//! - [`SessionOwnershipStore`] persists per-session [`BackendOwner`] pins via an
//!   atomic temp+fsync+rename JSON ledger protected by a domain-separated SHA-256
//!   integrity digest.
//! - [`handle_kernel_turn_failure`] enforces the P7 no-fallback guarantee: Kernel
//!   failures return authentic structured errors without silently falling back to
//!   or replaying partial operations on [`BackendOwner::LegacyRuntime`].
//! - [`audit_jsonl_transcript`] verifies live session quiescence and `.jsonl`
//!   transcript domain/tool-pair stationary invariants before any import proceeds.
//! - [`import_stationary_session`] and [`import_for_rollback`] seed
//!   [`pilotdeck_context_iface::AuthoritativeContextLedger`] (aliased as
//!   [`ContextStateAuthority`]) idempotently and losslessly while preserving the
//!   original transcript and existing session ownership pins.
#![forbid(unsafe_code)]

pub mod error;
pub mod importer;
pub mod ownership;
pub mod stationary;

pub use error::MigrationError;
pub use importer::{
    ContextStateAuthority, ImportOptions, SessionIdentityMapping, compute_import_idempotency_key,
    derive_import_target_session_id, derive_rollback_target_session_id, import_for_rollback,
    import_for_rollback_with_options, import_stationary_session,
    import_stationary_session_with_options, import_stationary_transcript_file,
};
pub use ownership::{
    BackendOwner, KernelFailureDisposition, KernelTurnFailure, OWNERSHIP_LEDGER_SCHEMA_VERSION,
    SessionOwnershipRecord, SessionOwnershipStore, handle_kernel_turn_failure,
};
pub use stationary::{
    LiveSessionActivity, MAX_TRANSCRIPT_BYTES, StationaryAudit, TranscriptTurnSummary,
    audit_jsonl_transcript, audit_jsonl_transcript_for_session,
};
