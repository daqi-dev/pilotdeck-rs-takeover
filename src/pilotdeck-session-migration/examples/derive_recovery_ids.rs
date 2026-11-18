//! Minimal usage of the session-migration identity derivation APIs: derive a
//! deterministic import target session id and its idempotency key for
//! cross-backend session recovery.
//!
//! Run with `cargo run -p pilotdeck-session-migration --example derive_recovery_ids`.
#![forbid(unsafe_code)]

use pilotdeck_session_migration::{
    BackendOwner, compute_import_idempotency_key, derive_import_target_session_id,
    derive_rollback_target_session_id,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = "source-session-1";
    let source_sha256 = "5920a58fcd4e1ba1f1b2970cb3e6c2b1d3e4f5a6b7c8d9e0f1a2b3c4d5e6f708";
    let owner = BackendOwner::KernelFullLate;

    // 1) Deterministic import target distinct from the source session.
    let import_target = derive_import_target_session_id(source, source_sha256, owner)?;
    println!("import target session: {import_target}");

    // 2) Explicit NEW rollback recovery target, scoped per attempt.
    let rollback_target = derive_rollback_target_session_id(source, source_sha256, owner, 1)?;
    println!("rollback target session: {rollback_target}");

    // 3) Idempotency key binding the import to its exact identity tuple.
    let key = compute_import_idempotency_key(source, &import_target, source_sha256, owner, false);
    println!("idempotency key: {key}");

    Ok(())
}
