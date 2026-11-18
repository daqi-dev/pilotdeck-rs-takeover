//! Idempotent, lossless import of verified stationary `.jsonl` transcripts into
//! [`AuthoritativeContextLedger`] and cross-backend rollback session recovery.

use std::fs;
use std::path::Path;

use pilotdeck_context_iface::{
    AuthoritativeContextLedger, ContextControl, ContextOperationKind, LedgerTransition,
};
use pilotdeck_protocol::{JsValue, boundary_codec};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{MigrationError, hash_str, hex_encode, validate_session_id};
use crate::ownership::{BackendOwner, ImportPin, SessionOwnershipStore};
use crate::stationary::{LiveSessionActivity, audit_jsonl_transcript};

/// Type alias matching the `pilotdeck-context-iface` authoritative ledger contract.
pub type ContextStateAuthority = AuthoritativeContextLedger;

const IMPORT_KEY_DOMAIN: &[u8] = b"pilotdeck.session_migration.import_key.v1";
const TARGET_ID_DOMAIN: &[u8] = b"pilotdeck.session_migration.target_id.v1";
const ROLLBACK_ID_DOMAIN: &[u8] = b"pilotdeck.session_migration.rollback_id.v1";
const BOUNDARY_CONTENT_TYPE: &str = "application/vnd.pilotdeck.context.boundary.v1";
const PROVENANCE_PROFILE: &str = "pilotdeck.migration.provenance.json";

/// Optional control metadata applied when seeding [`AuthoritativeContextLedger`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportOptions {
    pub control: ContextControl,
}

/// Immutable cross-backend identity and provenance record emitted by stationary import.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionIdentityMapping {
    pub source_session_id: String,
    pub target_session_id: String,
    pub source_sha256: String,
    pub imported_revision: u64,
    pub imported_entries: usize,
    pub imported_messages: usize,
    pub idempotency_key: String,
    pub target_owner: BackendOwner,
    pub is_rollback: bool,
}

#[derive(Serialize)]
struct ProvenanceFact<'a> {
    schema_version: u32,
    kind: &'static str,
    source_session_id: &'a str,
    target_session_id: &'a str,
    source_sha256: &'a str,
    imported_entries: usize,
    imported_messages: usize,
    last_sequence: Option<u64>,
    target_owner: BackendOwner,
}

/// Compute a deterministic, <=256-byte transaction idempotency key for stationary import.
pub fn compute_import_idempotency_key(
    source_session_id: &str,
    target_session_id: &str,
    source_sha256: &str,
    target_owner: BackendOwner,
    is_rollback: bool,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(IMPORT_KEY_DOMAIN);
    hasher.update([u8::from(is_rollback)]);
    hash_str(&mut hasher, source_session_id);
    hash_str(&mut hasher, target_session_id);
    hash_str(&mut hasher, source_sha256);
    hash_str(&mut hasher, target_owner.as_str());
    let digest = hex_encode(&hasher.finalize());
    let prefix = if is_rollback {
        "mig-rollback-v1"
    } else {
        "mig-import-v1"
    };
    format!("{prefix}:{digest}")
}

/// Derive a deterministic, valid target session identifier distinct from `source_session_id`.
pub fn derive_import_target_session_id(
    source_session_id: &str,
    source_sha256: &str,
    target_owner: BackendOwner,
) -> Result<String, MigrationError> {
    validate_session_id(source_session_id)?;
    let mut hasher = Sha256::new();
    hasher.update(TARGET_ID_DOMAIN);
    hash_str(&mut hasher, source_session_id);
    hash_str(&mut hasher, source_sha256);
    hash_str(&mut hasher, target_owner.as_str());
    let tag = hex_encode(&hasher.finalize());
    let candidate = format!(
        "mig:{}:{}:{}",
        target_owner.short_tag(),
        sanitize_prefix(source_session_id, 48),
        &tag[..16]
    );
    ensure_distinct(candidate, source_session_id)
}

/// Derive a deterministic, explicit NEW target session identifier for rollback recovery.
pub fn derive_rollback_target_session_id(
    source_session_id: &str,
    source_sha256: &str,
    target_owner: BackendOwner,
    attempt: u64,
) -> Result<String, MigrationError> {
    validate_session_id(source_session_id)?;
    let mut hasher = Sha256::new();
    hasher.update(ROLLBACK_ID_DOMAIN);
    hash_str(&mut hasher, source_session_id);
    hash_str(&mut hasher, source_sha256);
    hash_str(&mut hasher, target_owner.as_str());
    hasher.update(attempt.to_le_bytes());
    let tag = hex_encode(&hasher.finalize());
    let candidate = format!(
        "rb:{}:{}:{attempt}:{}",
        target_owner.short_tag(),
        sanitize_prefix(source_session_id, 40),
        &tag[..16]
    );
    ensure_distinct(candidate, source_session_id)
}

/// Audit a `.jsonl` transcript and idempotently import its verified stationary
/// context into `target_ledger`, pinning `target_ledger.session_id()` to `target_owner`.
pub fn import_stationary_session(
    raw_jsonl: &[u8],
    activity: &LiveSessionActivity,
    target_ledger: &AuthoritativeContextLedger,
    ownership_store: &mut SessionOwnershipStore,
    target_owner: BackendOwner,
) -> Result<SessionIdentityMapping, MigrationError> {
    import_stationary_session_with_options(
        raw_jsonl,
        activity,
        target_ledger,
        ownership_store,
        target_owner,
        &ImportOptions::default(),
    )
}

/// Variant of [`import_stationary_session`] accepting explicit [`ImportOptions`].
pub fn import_stationary_session_with_options(
    raw_jsonl: &[u8],
    activity: &LiveSessionActivity,
    target_ledger: &AuthoritativeContextLedger,
    ownership_store: &mut SessionOwnershipStore,
    target_owner: BackendOwner,
    options: &ImportOptions,
) -> Result<SessionIdentityMapping, MigrationError> {
    execute_import(
        false,
        raw_jsonl,
        activity,
        target_ledger,
        ownership_store,
        target_owner,
        options,
    )
}

/// Read a stationary `.jsonl` transcript file read-only and import it into `target_ledger`.
pub fn import_stationary_transcript_file(
    transcript_path: impl AsRef<Path>,
    activity: &LiveSessionActivity,
    target_ledger: &AuthoritativeContextLedger,
    ownership_store: &mut SessionOwnershipStore,
    target_owner: BackendOwner,
) -> Result<SessionIdentityMapping, MigrationError> {
    let path = transcript_path.as_ref();
    let raw_jsonl = fs::read(path).map_err(|err| {
        MigrationError::Io(format!(
            "failed to read transcript {}: {err}",
            path.display()
        ))
    })?;
    import_stationary_session(
        &raw_jsonl,
        activity,
        target_ledger,
        ownership_store,
        target_owner,
    )
}

/// Perform cross-backend rollback recovery by importing a stationary transcript into
/// an explicit NEW `target_ledger` session ID without mutating the source transcript
/// or existing source session ownership record.
pub fn import_for_rollback(
    raw_jsonl: &[u8],
    activity: &LiveSessionActivity,
    target_ledger: &AuthoritativeContextLedger,
    ownership_store: &mut SessionOwnershipStore,
    target_owner: BackendOwner,
) -> Result<SessionIdentityMapping, MigrationError> {
    import_with_default_options(
        raw_jsonl,
        activity,
        target_ledger,
        ownership_store,
        target_owner,
        true,
    )
}

/// Variant of [`import_for_rollback`] accepting explicit [`ImportOptions`].
pub fn import_for_rollback_with_options(
    raw_jsonl: &[u8],
    activity: &LiveSessionActivity,
    target_ledger: &AuthoritativeContextLedger,
    ownership_store: &mut SessionOwnershipStore,
    target_owner: BackendOwner,
    options: &ImportOptions,
) -> Result<SessionIdentityMapping, MigrationError> {
    execute_import(
        true,
        raw_jsonl,
        activity,
        target_ledger,
        ownership_store,
        target_owner,
        options,
    )
}

/// Shared body of the default-`ImportOptions` import wrappers.
fn import_with_default_options(
    raw_jsonl: &[u8],
    activity: &LiveSessionActivity,
    target_ledger: &AuthoritativeContextLedger,
    ownership_store: &mut SessionOwnershipStore,
    target_owner: BackendOwner,
    is_rollback: bool,
) -> Result<SessionIdentityMapping, MigrationError> {
    execute_import(
        is_rollback,
        raw_jsonl,
        activity,
        target_ledger,
        ownership_store,
        target_owner,
        &ImportOptions::default(),
    )
}

fn execute_import(
    is_rollback: bool,
    raw_jsonl: &[u8],
    activity: &LiveSessionActivity,
    target_ledger: &AuthoritativeContextLedger,
    ownership_store: &mut SessionOwnershipStore,
    target_owner: BackendOwner,
    options: &ImportOptions,
) -> Result<SessionIdentityMapping, MigrationError> {
    let audit = audit_jsonl_transcript(raw_jsonl, activity)?;
    let target_session_id = target_ledger.session_id();

    let pin = ImportPin {
        target_session_id,
        source_session_id: &audit.source_session_id,
        source_sha256: &audit.source_sha256,
        target_owner,
        is_rollback,
    };

    let _already_pinned = ownership_store.check_import_target(&pin)?;

    let encoded_messages = boundary_codec::encode(&JsValue::Array(audit.messages.clone()))
        .map_err(|err| {
            MigrationError::ImportFailed(format!("boundary_codec encode failed: {err:?}"))
        })?;

    let template_ledger = AuthoritativeContextLedger::new(target_session_id);
    let mut replacement = template_ledger.snapshot().payload;
    replacement.profile = boundary_codec::PROFILE.to_string();
    replacement.content_type = BOUNDARY_CONTENT_TYPE.to_string();
    replacement.bytes = encoded_messages;

    let fact_bytes = serde_json::to_vec(&ProvenanceFact {
        schema_version: 1,
        kind: if is_rollback {
            "rollback_import"
        } else {
            "stationary_import"
        },
        source_session_id: &audit.source_session_id,
        target_session_id,
        source_sha256: &audit.source_sha256,
        imported_entries: audit.entry_count,
        imported_messages: audit.message_count,
        last_sequence: audit.last_sequence,
        target_owner,
    })
    .map_err(|err| MigrationError::ImportFailed(format!("provenance JSON encode failed: {err}")))?;

    let mut fact_payload = template_ledger.snapshot().payload;
    fact_payload.profile = PROVENANCE_PROFILE.to_string();
    fact_payload.content_type = "application/json".to_string();
    fact_payload.bytes = fact_bytes;

    let idempotency_key = compute_import_idempotency_key(
        &audit.source_session_id,
        target_session_id,
        &audit.source_sha256,
        target_owner,
        is_rollback,
    );
    let consumed_input_id = format!(
        "{}:{}",
        if is_rollback { "mig-rb-src" } else { "mig-src" },
        audit.source_sha256
    );

    let transition = LedgerTransition::new(
        idempotency_key.clone(),
        0,
        ContextOperationKind::PrepareForModel.into(),
    )
    .with_control(options.control.clone())
    .with_replacement(replacement)
    .with_consumed_inputs(vec![consumed_input_id])
    .with_facts(vec![fact_payload]);

    let receipt = target_ledger.commit_transition(transition)?;

    ownership_store.commit_import_pin(&pin)?;

    Ok(SessionIdentityMapping {
        source_session_id: audit.source_session_id,
        target_session_id: target_session_id.to_string(),
        source_sha256: audit.source_sha256,
        imported_revision: receipt.revision,
        imported_entries: audit.entry_count,
        imported_messages: audit.message_count,
        idempotency_key,
        target_owner,
        is_rollback,
    })
}

fn sanitize_prefix(raw: &str, max_chars: usize) -> String {
    let cleaned: String = raw
        .chars()
        .take(max_chars)
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "session".to_string()
    } else {
        cleaned
    }
}

fn ensure_distinct(
    mut candidate: String,
    source_session_id: &str,
) -> Result<String, MigrationError> {
    if candidate == source_session_id {
        candidate.push_str("-1");
    }
    validate_session_id(&candidate)?;
    Ok(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(1);

    fn scratch_dir(label: &str) -> PathBuf {
        let seq = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("pd-mig-imp-{label}-{}-{seq}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn deterministic_idempotency_key_and_target_ids_are_stable_and_bounded() {
        let long_source = "s".repeat(250);
        let target = derive_import_target_session_id(
            &long_source,
            &"a".repeat(64),
            BackendOwner::KernelScopedResident,
        )
        .unwrap();
        assert!(target.len() <= 256);
        assert_ne!(target, long_source);

        let rb = derive_rollback_target_session_id(
            &long_source,
            &"a".repeat(64),
            BackendOwner::LegacyRuntime,
            1,
        )
        .unwrap();
        assert!(rb.len() <= 256);
        assert_ne!(rb, long_source);
        assert_ne!(rb, target);

        let k1 = compute_import_idempotency_key(
            &long_source,
            &target,
            &"a".repeat(64),
            BackendOwner::KernelScopedResident,
            false,
        );
        let k2 = compute_import_idempotency_key(
            &long_source,
            &target,
            &"a".repeat(64),
            BackendOwner::KernelScopedResident,
            false,
        );
        assert_eq!(k1, k2);
        assert!(k1.len() <= 256);
    }

    #[test]
    fn stationary_import_is_idempotent_and_preserves_messages() {
        let dir = scratch_dir("idem");
        let mut store = SessionOwnershipStore::open(
            dir.join("ownership.json"),
            BackendOwner::KernelScopedResident,
        )
        .unwrap();
        store
            .pin_session("src-1", BackendOwner::LegacyRuntime)
            .unwrap();

        let jsonl = concat!(
            r#"{"type":"turn_started","sessionId":"src-1","turnId":"t1","sequence":1}"#,
            "\n",
            r#"{"type":"accepted_input","sessionId":"src-1","turnId":"t1","sequence":2,"messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}]}"#,
            "\n",
            r#"{"type":"assistant_message","sessionId":"src-1","turnId":"t1","sequence":3,"message":{"role":"assistant","content":[{"type":"text","text":"world"}]}}"#,
            "\n",
            r#"{"type":"turn_result","sessionId":"src-1","turnId":"t1","sequence":4,"result":{"type":"success"}}"#,
            "\n"
        );
        let ledger = AuthoritativeContextLedger::new("target-1");

        let first = import_stationary_session(
            jsonl.as_bytes(),
            &LiveSessionActivity::default(),
            &ledger,
            &mut store,
            BackendOwner::KernelScopedResident,
        )
        .unwrap();
        assert_eq!(first.source_session_id, "src-1");
        assert_eq!(first.target_session_id, "target-1");
        assert_eq!(first.imported_revision, 1);
        assert_eq!(first.imported_entries, 4);
        assert_eq!(first.imported_messages, 2);

        // Re-running import must be idempotent and non-duplicating.
        let second = import_stationary_session(
            jsonl.as_bytes(),
            &LiveSessionActivity::default(),
            &ledger,
            &mut store,
            BackendOwner::KernelScopedResident,
        )
        .unwrap();
        assert_eq!(second, first);
        assert_eq!(ledger.snapshot().revision, 1);
        assert_eq!(
            store.get("src-1").unwrap().unwrap().owner,
            BackendOwner::LegacyRuntime
        );
        assert_eq!(
            store.get("target-1").unwrap().unwrap().owner,
            BackendOwner::KernelScopedResident
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
