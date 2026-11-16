//! Integration tests for `pilotdeck-session-migration` (`P7` ownership pinning,
//! no-fallback Kernel failure handling, stationary audit, idempotent import,
//! lossless payload fidelity, and cross-backend rollback).

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use agentcpm_iface::types::{ERR_APP, Error, ErrorCategory};
use pilotdeck_context_iface::{
    ContextControl, ContextOperationKind, LedgerError, LedgerTransition,
};
use pilotdeck_protocol::{JsValue, boundary_codec, parse_json, stringify_json};
use pilotdeck_session_migration::{
    BackendOwner, ContextStateAuthority, ImportOptions, KernelFailureDisposition,
    LiveSessionActivity, MigrationError, SessionOwnershipStore, audit_jsonl_transcript,
    audit_jsonl_transcript_for_session, compute_import_idempotency_key,
    derive_import_target_session_id, derive_rollback_target_session_id, handle_kernel_turn_failure,
    import_for_rollback, import_stationary_session, import_stationary_session_with_options,
    import_stationary_transcript_file,
};

static TEST_SEQ: AtomicU64 = AtomicU64::new(1);

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(label: &str) -> Self {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("pd-mig-int-{label}-{}-{seq}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &PathBuf {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn minimal_stationary_jsonl(session_id: &str, text: &str) -> String {
    format!(
        concat!(
            r#"{{"type":"turn_started","sessionId":"{sid}","turnId":"turn-1","sequence":1,"createdAt":"2026-11-16T00:00:00.000Z"}}"#,
            "\n",
            r#"{{"type":"accepted_input","sessionId":"{sid}","turnId":"turn-1","sequence":2,"createdAt":"2026-11-16T00:00:01.000Z","messages":[{{"role":"user","content":[{{"type":"text","text":"{txt}"}}]}}]}}"#,
            "\n",
            r#"{{"type":"step_started","sessionId":"{sid}","turnId":"turn-1","step":1,"sequence":3,"createdAt":"2026-11-16T00:00:02.000Z"}}"#,
            "\n",
            r#"{{"type":"assistant_message","sessionId":"{sid}","turnId":"turn-1","sequence":4,"createdAt":"2026-11-16T00:00:03.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"reply:{txt}"}}]}}}}"#,
            "\n",
            r#"{{"type":"step_completed","sessionId":"{sid}","turnId":"turn-1","step":1,"outcome":"completed","sequence":5,"createdAt":"2026-11-16T00:00:04.000Z"}}"#,
            "\n",
            r#"{{"type":"turn_result","sessionId":"{sid}","turnId":"turn-1","sequence":6,"createdAt":"2026-11-16T00:00:05.000Z","result":{{"type":"success","usage":{{"inputTokens":3,"outputTokens":2}},"permissionDenials":[]}}}}"#,
            "\n"
        ),
        sid = session_id,
        txt = text,
    )
}

#[test]
fn new_sessions_route_to_configurable_default_across_all_three_backends() {
    let dir = ScratchDir::new("default-owners");
    let path = dir.path().join("nested").join("ownership.json");

    let mut store = SessionOwnershipStore::open(&path, BackendOwner::LegacyRuntime).unwrap();
    assert_eq!(store.default_owner(), BackendOwner::LegacyRuntime);
    assert_eq!(
        store.route_session("s-legacy").unwrap().owner,
        BackendOwner::LegacyRuntime
    );

    store
        .set_default_owner(BackendOwner::KernelScopedResident)
        .unwrap();
    assert_eq!(
        store.route_session("s-scoped").unwrap().owner,
        BackendOwner::KernelScopedResident
    );

    store
        .set_default_owner(BackendOwner::KernelFullLate)
        .unwrap();
    assert_eq!(
        store.route_session("s-full").unwrap().owner,
        BackendOwner::KernelFullLate
    );

    // Previously pinned sessions never drift when default_owner changes.
    assert_eq!(
        store.route_session("s-legacy").unwrap().owner,
        BackendOwner::LegacyRuntime
    );
    assert_eq!(
        store.route_session("s-scoped").unwrap().owner,
        BackendOwner::KernelScopedResident
    );
}

#[test]
fn existing_session_pins_persist_across_reload_and_enforce_isolation() {
    let dir = ScratchDir::new("reload-pins");
    let path = dir.path().join("ownership.json");

    {
        let mut store = SessionOwnershipStore::open(&path, BackendOwner::LegacyRuntime).unwrap();
        store.route_session("legacy-1").unwrap();
        store
            .pin_session("kernel-1", BackendOwner::KernelScopedResident)
            .unwrap();
    }

    let mut reopened = SessionOwnershipStore::open(&path, BackendOwner::KernelFullLate).unwrap();
    assert_eq!(
        reopened.route_session("legacy-1").unwrap().owner,
        BackendOwner::LegacyRuntime
    );
    assert_eq!(
        reopened.route_session("kernel-1").unwrap().owner,
        BackendOwner::KernelScopedResident
    );
    assert_eq!(
        reopened.route_session("fresh-1").unwrap().owner,
        BackendOwner::KernelFullLate
    );

    reopened
        .assert_owner("legacy-1", BackendOwner::LegacyRuntime)
        .unwrap();
    reopened
        .require_owner("kernel-1", BackendOwner::KernelScopedResident)
        .unwrap();

    let err = reopened
        .assert_owner("legacy-1", BackendOwner::KernelFullLate)
        .unwrap_err();
    assert!(matches!(
        err,
        MigrationError::OwnershipConflict {
            recorded: BackendOwner::LegacyRuntime,
            requested: BackendOwner::KernelFullLate,
            ..
        }
    ));
}

#[test]
fn pin_session_rejects_cross_backend_reassignment_across_all_pairs() {
    let dir = ScratchDir::new("cross-pin");
    let path = dir.path().join("ownership.json");
    let mut store = SessionOwnershipStore::open(&path, BackendOwner::LegacyRuntime).unwrap();

    let owners = [
        BackendOwner::LegacyRuntime,
        BackendOwner::KernelScopedResident,
        BackendOwner::KernelFullLate,
    ];
    for (idx, &recorded) in owners.iter().enumerate() {
        let sid = format!("sess-{idx}");
        let rec = store.pin_session(&sid, recorded).unwrap();
        assert_eq!(rec.owner, recorded);
        // Same-owner pin is idempotent.
        assert_eq!(store.pin_session(&sid, recorded).unwrap(), rec);

        for &other in &owners {
            if other != recorded {
                let err = store.pin_session(&sid, other).unwrap_err();
                assert!(matches!(
                    err,
                    MigrationError::OwnershipConflict {
                        recorded: r,
                        requested: q,
                        ..
                    } if r == recorded && q == other
                ));
            }
        }
    }
}

#[test]
fn atomic_store_persistence_leaves_no_staging_temp_files() {
    let dir = ScratchDir::new("atomic-clean");
    let path = dir.path().join("ownership.json");

    let mut store = SessionOwnershipStore::open(&path, BackendOwner::KernelScopedResident).unwrap();
    for i in 0..8 {
        store.route_session(&format!("sess-{i}")).unwrap();
    }

    let entries: Vec<String> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, vec!["ownership.json".to_string()]);

    let reloaded = SessionOwnershipStore::load(&path).unwrap();
    assert_eq!(reloaded.ledger_sha256(), store.ledger_sha256());
    assert_eq!(reloaded.records().len(), 8);
}

#[test]
fn tampered_ownership_ledger_variants_fail_closed() {
    let dir = ScratchDir::new("tamper-variants");
    let path = dir.path().join("ownership.json");

    {
        let mut store =
            SessionOwnershipStore::open(&path, BackendOwner::KernelScopedResident).unwrap();
        store.route_session("sess-1").unwrap();
    }
    let baseline = fs::read_to_string(&path).unwrap();

    let cases = [
        baseline.replace("\"schema_version\": 1", "\"schema_version\": 2"),
        baseline.replace("kernel_scoped_resident", "legacy_runtime"),
        baseline.replace("\"generation\": 1", "\"generation\": 0"),
        baseline.replace("\"sess-1\": {", "\"sess-renamed\": {"),
        baseline.replace(&baseline[baseline.len() - 20..], ""),
        "{not valid json".to_string(),
    ];

    for (idx, mutated) in cases.iter().enumerate() {
        fs::write(&path, mutated).unwrap();
        let err = SessionOwnershipStore::load(&path).unwrap_err();
        assert!(
            matches!(err, MigrationError::OwnershipLedgerCorrupt(_)),
            "case {idx} expected OwnershipLedgerCorrupt, got {err:?}"
        );
    }
}

#[test]
fn invalid_session_ids_fail_closed() {
    let dir = ScratchDir::new("bad-sid");
    let path = dir.path().join("ownership.json");
    let mut store = SessionOwnershipStore::open(&path, BackendOwner::LegacyRuntime).unwrap();

    let overlong = "x".repeat(257);
    for bad in ["", "   ", "\t", "bad\nsid", "bad\0sid", &overlong] {
        assert!(matches!(
            store.route_session(bad),
            Err(MigrationError::InvalidSessionId(_))
        ));
        assert!(matches!(
            store.get(bad),
            Err(MigrationError::InvalidSessionId(_))
        ));
    }
}

#[test]
fn kernel_turn_failure_returns_authentic_error_with_no_fallback() {
    let dir = ScratchDir::new("kernel-fail");
    let path = dir.path().join("ownership.json");
    let mut store = SessionOwnershipStore::open(&path, BackendOwner::KernelScopedResident).unwrap();
    store.route_session("sess-scoped").unwrap();
    store
        .pin_session("sess-full", BackendOwner::KernelFullLate)
        .unwrap();

    let known_err = Error::app(ErrorCategory::Protocol, "SCHEMA_REJECTED", "bad input");
    let f1 = handle_kernel_turn_failure(
        &store,
        "sess-scoped",
        "turn-10",
        BackendOwner::KernelScopedResident,
        &known_err,
        false,
        Some(true),
    )
    .unwrap();
    assert_eq!(f1.disposition, KernelFailureDisposition::FailedNoFallback);
    assert_eq!(f1.reason_code, "SCHEMA_REJECTED");
    assert!(!f1.fallback_attempted);
    assert!(!f1.partial_replay_attempted);
    assert_eq!(f1.resource_released, Some(true));
    assert_eq!(f1.to_iface_error(), known_err);

    let mut transport_err = Error::new(ErrorCategory::Transport, -32099, "uds socket dropped");
    transport_err.retryable = true;
    let f2 = handle_kernel_turn_failure(
        &store,
        "sess-full",
        "turn-11",
        BackendOwner::KernelFullLate,
        &transport_err,
        true,
        None,
    )
    .unwrap();
    assert_eq!(
        f2.disposition,
        KernelFailureDisposition::UnknownReconcileRequired
    );
    assert_eq!(f2.reason_code, "KERNEL_ERR_-32099");
    assert!(f2.retryable);
    assert!(!f2.fallback_attempted);
    assert!(!f2.partial_replay_attempted);
    assert_eq!(f2.resource_released, None);

    // Store pins remain untouched on Kernel backends (no fallback to LegacyRuntime).
    assert_eq!(
        store.get("sess-scoped").unwrap().unwrap().owner,
        BackendOwner::KernelScopedResident
    );
    assert_eq!(
        store.get("sess-full").unwrap().unwrap().owner,
        BackendOwner::KernelFullLate
    );
}

#[test]
fn kernel_turn_failure_rejects_legacy_owner_or_mismatched_store_pin() {
    let dir = ScratchDir::new("kernel-fail-bad");
    let mut store = SessionOwnershipStore::open(
        dir.path().join("ownership.json"),
        BackendOwner::LegacyRuntime,
    )
    .unwrap();
    store.route_session("sess-leg").unwrap();

    let err = Error::app(ErrorCategory::Execution, "FAIL", "boom");
    let res = handle_kernel_turn_failure(
        &store,
        "sess-leg",
        "turn-1",
        BackendOwner::LegacyRuntime,
        &err,
        false,
        None,
    )
    .unwrap_err();
    assert!(matches!(res, MigrationError::OwnershipConflict { .. }));

    let res = handle_kernel_turn_failure(
        &store,
        "sess-leg",
        "turn-1",
        BackendOwner::KernelScopedResident,
        &err,
        false,
        None,
    )
    .unwrap_err();
    assert!(matches!(res, MigrationError::OwnershipConflict { .. }));
}

#[test]
fn audit_accepts_full_pilotdeck_transcript_shape_with_compaction_and_metadata() {
    let jsonl = concat!(
        r#"{"type":"turn_started","sessionId":"web:parity","turnId":"run-1","sequence":1,"createdAt":"2026-10-14T02:20:37.655Z","entryId":"e1","parentEntryId":null}"#,
        "\n",
        r#"{"type":"accepted_input","messages":[{"role":"user","content":[{"type":"text","text":"GP_TEXT"}]}],"sessionId":"web:parity","turnId":"run-1","sequence":2,"createdAt":"2026-10-14T02:20:37.656Z","entryId":"e2","parentEntryId":"e1"}"#,
        "\n",
        r#"{"type":"session_metadata","metadata":{"title":"t"},"sessionId":"web:parity","turnId":"run-1","sequence":3,"createdAt":"2026-10-14T02:20:37.657Z","entryId":"e3","parentEntryId":"e2"}"#,
        "\n",
        r#"{"type":"step_started","step":1,"sessionId":"web:parity","turnId":"run-1","sequence":4,"createdAt":"2026-10-14T02:20:37.667Z","entryId":"e4","parentEntryId":"e3"}"#,
        "\n",
        r#"{"type":"context_snapshot","step":1,"contexts":[],"sessionId":"web:parity","turnId":"run-1","sequence":5,"createdAt":"2026-10-14T02:20:37.670Z","entryId":"e5","parentEntryId":"e4"}"#,
        "\n",
        r#"{"type":"compaction_started","operationId":"op-c1","trigger":"auto","messageCount":1,"sessionId":"web:parity","turnId":"run-1","sequence":6,"createdAt":"2026-10-14T02:20:37.670Z","entryId":"e6","parentEntryId":"e5"}"#,
        "\n",
        r#"{"type":"compaction_completed","operationId":"op-c1","status":"skipped","messageCount":1,"sessionId":"web:parity","turnId":"run-1","sequence":7,"createdAt":"2026-10-14T02:20:37.903Z","entryId":"e7","parentEntryId":"e6"}"#,
        "\n",
        r#"{"type":"model_request","step":1,"request":{},"sessionId":"web:parity","turnId":"run-1","sequence":8,"createdAt":"2026-10-14T02:20:37.912Z","entryId":"e8","parentEntryId":"e7"}"#,
        "\n",
        r#"{"type":"model_stream_event","step":1,"event":{"type":"text_delta","text":"OK"},"sessionId":"web:parity","turnId":"run-1","sequence":9,"createdAt":"2026-10-14T02:20:37.918Z","entryId":"e9","parentEntryId":"e8"}"#,
        "\n",
        r#"{"type":"assistant_message","message":{"role":"assistant","content":[{"type":"text","text":"OK"}]},"sessionId":"web:parity","turnId":"run-1","sequence":10,"createdAt":"2026-10-14T02:20:37.921Z","entryId":"e10","parentEntryId":"e9"}"#,
        "\n",
        r#"{"type":"step_completed","step":1,"outcome":"completed","sessionId":"web:parity","turnId":"run-1","sequence":11,"createdAt":"2026-10-14T02:20:37.922Z","entryId":"e11","parentEntryId":"e10"}"#,
        "\n",
        r#"{"type":"turn_result","result":{"type":"success","stopReason":"completed","usage":{},"permissionDenials":[]},"sessionId":"web:parity","turnId":"run-1","sequence":12,"createdAt":"2026-10-14T02:20:37.922Z","entryId":"e12","parentEntryId":"e11"}"#,
        "\n",
        r#"{"type":"session_metadata","metadata":{"isSnapshot":true},"sessionId":"web:parity","turnId":"run-1","sequence":13,"createdAt":"2026-10-14T02:20:37.923Z","entryId":"e13","parentEntryId":"e12"}"#,
        "\n"
    );

    let audit = audit_jsonl_transcript_for_session(
        jsonl.as_bytes(),
        "web:parity",
        &LiveSessionActivity::default(),
    )
    .unwrap();
    assert_eq!(audit.entry_count, 13);
    assert_eq!(audit.message_count, 2);
    assert_eq!(audit.last_sequence, Some(13));
    assert_eq!(audit.turns[0].terminal_type.as_deref(), Some("success"));
}

#[test]
fn audit_accepts_aborted_and_error_turns_when_closed() {
    let jsonl = concat!(
        r#"{"type":"turn_started","sessionId":"s1","turnId":"t1","sequence":1}"#,
        "\n",
        r#"{"type":"accepted_input","sessionId":"s1","turnId":"t1","sequence":2,"messages":[{"role":"user","content":[{"type":"text","text":"a"}]}]}"#,
        "\n",
        r#"{"type":"step_started","sessionId":"s1","turnId":"t1","step":1,"sequence":3}"#,
        "\n",
        r#"{"type":"step_completed","sessionId":"s1","turnId":"t1","step":1,"outcome":"aborted","sequence":4}"#,
        "\n",
        r#"{"type":"turn_result","sessionId":"s1","turnId":"t1","sequence":5,"result":{"type":"aborted"}}"#,
        "\n",
        r#"{"type":"turn_started","sessionId":"s1","turnId":"t2","sequence":6}"#,
        "\n",
        r#"{"type":"accepted_input","sessionId":"s1","turnId":"t2","sequence":7,"messages":[{"role":"user","content":[{"type":"text","text":"b"}]}]}"#,
        "\n",
        r#"{"type":"turn_result","sessionId":"s1","turnId":"t2","sequence":8,"result":{"type":"error","message":"failed"}}"#,
        "\n"
    );

    let audit = audit_jsonl_transcript(jsonl.as_bytes(), &LiveSessionActivity::default()).unwrap();
    assert_eq!(audit.turn_count, 2);
    assert_eq!(audit.turns[0].terminal_type.as_deref(), Some("aborted"));
    assert_eq!(audit.turns[1].terminal_type.as_deref(), Some("error"));
}

#[test]
fn audit_rejects_in_transcript_pending_permission_question_and_compaction() {
    let pending_perm = concat!(
        r#"{"type":"turn_started","sessionId":"s1","turnId":"t1","sequence":1}"#,
        "\n",
        r#"{"type":"step_started","sessionId":"s1","turnId":"t1","step":1,"sequence":2}"#,
        "\n",
        r#"{"type":"permission_started","sessionId":"s1","turnId":"t1","step":1,"operationId":"perm-1","sequence":3}"#,
        "\n"
    );
    assert!(matches!(
        audit_jsonl_transcript(pending_perm.as_bytes(), &LiveSessionActivity::default()),
        Err(MigrationError::SessionNotStationary(_))
    ));

    let pending_question = concat!(
        r#"{"type":"turn_started","sessionId":"s1","turnId":"t1","sequence":1}"#,
        "\n",
        r#"{"type":"question_started","sessionId":"s1","turnId":"t1","operationId":"q-1","sequence":2}"#,
        "\n"
    );
    assert!(matches!(
        audit_jsonl_transcript(pending_question.as_bytes(), &LiveSessionActivity::default()),
        Err(MigrationError::SessionNotStationary(_))
    ));

    let pending_compact = concat!(
        r#"{"type":"turn_started","sessionId":"s1","turnId":"t1","sequence":1}"#,
        "\n",
        r#"{"type":"compaction_started","sessionId":"s1","turnId":"t1","operationId":"c-1","sequence":2}"#,
        "\n"
    );
    assert!(matches!(
        audit_jsonl_transcript(pending_compact.as_bytes(), &LiveSessionActivity::default()),
        Err(MigrationError::SessionNotStationary(_))
    ));
}

#[test]
fn audit_rejects_unclosed_turn_unclosed_step_and_queued_turn() {
    let unclosed_turn = concat!(
        r#"{"type":"turn_started","sessionId":"s1","turnId":"t1","sequence":1}"#,
        "\n",
        r#"{"type":"accepted_input","sessionId":"s1","turnId":"t1","sequence":2,"messages":[{"role":"user","content":[{"type":"text","text":"q"}]}]}"#,
        "\n",
        r#"{"type":"assistant_message","sessionId":"s1","turnId":"t1","sequence":3,"message":{"role":"assistant","content":[{"type":"text","text":"partial"}]}}"#,
        "\n"
    );
    assert!(matches!(
        audit_jsonl_transcript(unclosed_turn.as_bytes(), &LiveSessionActivity::default()),
        Err(MigrationError::SessionNotStationary(_))
    ));

    let unclosed_step_before_result = concat!(
        r#"{"type":"turn_started","sessionId":"s1","turnId":"t1","sequence":1}"#,
        "\n",
        r#"{"type":"step_started","sessionId":"s1","turnId":"t1","step":1,"sequence":2}"#,
        "\n",
        r#"{"type":"turn_result","sessionId":"s1","turnId":"t1","sequence":3,"result":{"type":"success"}}"#,
        "\n"
    );
    assert!(matches!(
        audit_jsonl_transcript(
            unclosed_step_before_result.as_bytes(),
            &LiveSessionActivity::default()
        ),
        Err(MigrationError::SessionNotStationary(_))
    ));

    let queued = concat!(
        r#"{"type":"turn_started","sessionId":"s1","turnId":"t1","sequence":1}"#,
        "\n",
        r#"{"type":"turn_result","sessionId":"s1","turnId":"t1","sequence":2,"result":{"type":"success"}}"#,
        "\n",
        r#"{"type":"agent_turn_enqueued","sessionId":"s1","turnId":"t2","itemId":"item-2","sequence":3}"#,
        "\n"
    );
    assert!(matches!(
        audit_jsonl_transcript(queued.as_bytes(), &LiveSessionActivity::default()),
        Err(MigrationError::SessionNotStationary(_))
    ));
}

#[test]
fn audit_rejects_inflight_domain_tool_call_and_unpaired_message_blocks() {
    let inflight_domain = concat!(
        r#"{"type":"turn_started","sessionId":"s1","turnId":"t1","sequence":1}"#,
        "\n",
        r#"{"type":"step_started","sessionId":"s1","turnId":"t1","step":1,"sequence":2}"#,
        "\n",
        r#"{"type":"tool_call","sessionId":"s1","turnId":"t1","step":1,"call":{"id":"c1","name":"bash"},"sequence":3}"#,
        "\n"
    );
    assert!(matches!(
        audit_jsonl_transcript(inflight_domain.as_bytes(), &LiveSessionActivity::default()),
        Err(MigrationError::SessionNotStationary(_))
    ));

    // Closed turn in envelope, but assistant message contains an unpaired Anthropic tool_use!
    let unpaired_msg_block = concat!(
        r#"{"type":"turn_started","sessionId":"s1","turnId":"t1","sequence":1}"#,
        "\n",
        r#"{"type":"assistant_message","sessionId":"s1","turnId":"t1","sequence":2,"message":{"role":"assistant","content":[{"type":"tool_use","id":"call_unpaired","name":"read_file","input":{}}]}}"#,
        "\n",
        r#"{"type":"turn_result","sessionId":"s1","turnId":"t1","sequence":3,"result":{"type":"aborted"}}"#,
        "\n"
    );
    assert!(matches!(
        audit_jsonl_transcript(
            unpaired_msg_block.as_bytes(),
            &LiveSessionActivity::default()
        ),
        Err(MigrationError::SessionNotStationary(_))
    ));

    // Orphaned tool_result without any preceding tool_call is transcript corruption.
    let orphaned_result = concat!(
        r#"{"type":"turn_started","sessionId":"s1","turnId":"t1","sequence":1}"#,
        "\n",
        r#"{"type":"tool_result_message","sessionId":"s1","turnId":"t1","sequence":2,"message":{"role":"user","content":[{"type":"tool_result","toolCallId":"ghost","content":[]}]}}"#,
        "\n",
        r#"{"type":"turn_result","sessionId":"s1","turnId":"t1","sequence":3,"result":{"type":"success"}}"#,
        "\n"
    );
    assert!(matches!(
        audit_jsonl_transcript(orphaned_result.as_bytes(), &LiveSessionActivity::default()),
        Err(MigrationError::CorruptedTranscript(_))
    ));
}

#[test]
fn audit_rejects_result_unknown_turn_and_operation_ledger_states() {
    let unknown_turn = concat!(
        r#"{"type":"turn_started","sessionId":"s1","turnId":"t1","sequence":1}"#,
        "\n",
        r#"{"type":"turn_result","sessionId":"s1","turnId":"t1","sequence":2,"result":{"type":"result_unknown"}}"#,
        "\n"
    );
    assert!(matches!(
        audit_jsonl_transcript(unknown_turn.as_bytes(), &LiveSessionActivity::default()),
        Err(MigrationError::SessionNotStationary(_))
    ));

    let unknown_op = concat!(
        r#"{"type":"turn_started","sessionId":"s1","turnId":"t1","sequence":1}"#,
        "\n",
        r#"{"type":"agent_loop_operation_started","sessionId":"s1","turnId":"t1","operationId":"op1","sequence":2}"#,
        "\n",
        r#"{"type":"agent_loop_operation_terminal","sessionId":"s1","turnId":"t1","operationId":"op1","outcome":"result_unknown","sequence":3}"#,
        "\n",
        r#"{"type":"turn_result","sessionId":"s1","turnId":"t1","sequence":4,"result":{"type":"error"}}"#,
        "\n"
    );
    assert!(matches!(
        audit_jsonl_transcript(unknown_op.as_bytes(), &LiveSessionActivity::default()),
        Err(MigrationError::SessionNotStationary(_))
    ));
}

#[test]
fn audit_rejects_corrupt_utf8_truncated_lines_and_bad_session_ids() {
    assert!(matches!(
        audit_jsonl_transcript(&[0xff, 0xfe, 0xfd], &LiveSessionActivity::default()),
        Err(MigrationError::CorruptedTranscript(_))
    ));
    assert!(matches!(
        audit_jsonl_transcript(b"   \n\r\n  ", &LiveSessionActivity::default()),
        Err(MigrationError::CorruptedTranscript(_))
    ));
    assert!(matches!(
        audit_jsonl_transcript(b"[1, 2, 3]\n", &LiveSessionActivity::default()),
        Err(MigrationError::CorruptedTranscript(_))
    ));

    let truncated_second_line = concat!(
        r#"{"type":"turn_started","sessionId":"s1","turnId":"t1","sequence":1}"#,
        "\n",
        r#"{"type":"turn_result","sessionId":"s1","turnId":"t1","sequence":2"#
    );
    assert!(matches!(
        audit_jsonl_transcript(
            truncated_second_line.as_bytes(),
            &LiveSessionActivity::default()
        ),
        Err(MigrationError::CorruptedTranscript(_))
    ));

    let mismatched_sid = concat!(
        r#"{"type":"turn_started","sessionId":"s1","turnId":"t1","sequence":1}"#,
        "\n",
        r#"{"type":"turn_result","sessionId":"s2","turnId":"t1","sequence":2}"#,
        "\n"
    );
    assert!(matches!(
        audit_jsonl_transcript(mismatched_sid.as_bytes(), &LiveSessionActivity::default()),
        Err(MigrationError::CorruptedTranscript(_))
    ));

    let valid = minimal_stationary_jsonl("s1", "hello");
    assert!(matches!(
        audit_jsonl_transcript_for_session(
            valid.as_bytes(),
            "other-s",
            &LiveSessionActivity::default()
        ),
        Err(MigrationError::CorruptedTranscript(_))
    ));

    let non_monotonic = concat!(
        r#"{"type":"turn_started","sessionId":"s1","turnId":"t1","sequence":2}"#,
        "\n",
        r#"{"type":"turn_result","sessionId":"s1","turnId":"t1","sequence":1}"#,
        "\n"
    );
    assert!(matches!(
        audit_jsonl_transcript(non_monotonic.as_bytes(), &LiveSessionActivity::default()),
        Err(MigrationError::CorruptedTranscript(_))
    ));
}

#[test]
fn import_preserves_order_tool_pairing_and_lone_utf16_surrogates_losslessly() {
    let dir = ScratchDir::new("lossless");
    let mut store = SessionOwnershipStore::open(
        dir.path().join("ownership.json"),
        BackendOwner::KernelScopedResident,
    )
    .unwrap();
    store
        .pin_session("src-utf16", BackendOwner::LegacyRuntime)
        .unwrap();

    // Includes lone surrogate `\ud800`, emoji `😀`, and paired tool_call + tool_result.
    let jsonl = concat!(
        r#"{"type":"turn_started","sessionId":"src-utf16","turnId":"t1","sequence":1}"#,
        "\n",
        r#"{"type":"accepted_input","sessionId":"src-utf16","turnId":"t1","sequence":2,"messages":[{"role":"user","content":[{"type":"text","text":"中文\ud800😀"}]}]}"#,
        "\n",
        r#"{"type":"step_started","sessionId":"src-utf16","turnId":"t1","step":1,"sequence":3}"#,
        "\n",
        r#"{"type":"tool_call","sessionId":"src-utf16","turnId":"t1","step":1,"call":{"id":"c-1","name":"read_file"},"sequence":4}"#,
        "\n",
        r#"{"type":"assistant_message","sessionId":"src-utf16","turnId":"t1","sequence":5,"message":{"role":"assistant","content":[{"type":"tool_call","id":"c-1","name":"read_file","input":{"path":"a"}}]}}"#,
        "\n",
        r#"{"type":"tool_result","sessionId":"src-utf16","turnId":"t1","step":1,"result":{"toolCallId":"c-1"},"sequence":6}"#,
        "\n",
        r#"{"type":"tool_result_message","sessionId":"src-utf16","turnId":"t1","sequence":7,"message":{"role":"user","content":[{"type":"tool_result","toolCallId":"c-1","content":[{"type":"text","text":"res-\ud800"}]}]}}"#,
        "\n",
        r#"{"type":"step_completed","sessionId":"src-utf16","turnId":"t1","step":1,"outcome":"completed","sequence":8}"#,
        "\n",
        r#"{"type":"step_started","sessionId":"src-utf16","turnId":"t1","step":2,"sequence":9}"#,
        "\n",
        r#"{"type":"assistant_message","sessionId":"src-utf16","turnId":"t1","sequence":10,"message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}"#,
        "\n",
        r#"{"type":"step_completed","sessionId":"src-utf16","turnId":"t1","step":2,"outcome":"completed","sequence":11}"#,
        "\n",
        r#"{"type":"turn_result","sessionId":"src-utf16","turnId":"t1","sequence":12,"result":{"type":"success"}}"#,
        "\n"
    );

    let audit = audit_jsonl_transcript(jsonl.as_bytes(), &LiveSessionActivity::default()).unwrap();
    let target_id = derive_import_target_session_id(
        &audit.source_session_id,
        &audit.source_sha256,
        BackendOwner::KernelScopedResident,
    )
    .unwrap();
    let ledger = ContextStateAuthority::new(target_id.clone());

    let mapping = import_stationary_session_with_options(
        jsonl.as_bytes(),
        &LiveSessionActivity::default(),
        &ledger,
        &mut store,
        BackendOwner::KernelScopedResident,
        &ImportOptions {
            control: ContextControl {
                route: Some("anthropic/claude".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();

    assert_eq!(mapping.source_session_id, "src-utf16");
    assert_eq!(mapping.target_session_id, target_id);
    assert_eq!(mapping.imported_revision, 1);
    assert_eq!(mapping.imported_entries, 12);
    assert_eq!(mapping.imported_messages, 4);
    assert!(!mapping.is_rollback);

    let snapshot = ledger.snapshot();
    assert_eq!(snapshot.payload.profile, boundary_codec::PROFILE);
    let decoded = boundary_codec::decode(&snapshot.payload.bytes).unwrap();
    let JsValue::Array(ref items) = decoded else {
        panic!("expected JsValue::Array");
    };
    assert_eq!(items.len(), 4);
    assert_eq!(items, &audit.messages);

    // Lone surrogate survived boundary_codec round-trip identically.
    let first_json = stringify_json(&items[0]).unwrap().unwrap();
    assert!(first_json.contains("\\ud800"));
    assert!(first_json.contains("😀"));
}

#[test]
fn import_respects_compact_boundary_replacement_and_post_boundary_turns() {
    let dir = ScratchDir::new("compact-boundary");
    let mut store = SessionOwnershipStore::open(
        dir.path().join("ownership.json"),
        BackendOwner::KernelFullLate,
    )
    .unwrap();

    let jsonl = concat!(
        r#"{"type":"turn_started","sessionId":"s-cb","turnId":"t1","sequence":1}"#,
        "\n",
        r#"{"type":"accepted_input","sessionId":"s-cb","turnId":"t1","sequence":2,"messages":[{"role":"user","content":[{"type":"text","text":"old-1"}]}]}"#,
        "\n",
        r#"{"type":"assistant_message","sessionId":"s-cb","turnId":"t1","sequence":3,"message":{"role":"assistant","content":[{"type":"text","text":"old-a1"}]}}"#,
        "\n",
        r#"{"type":"turn_result","sessionId":"s-cb","turnId":"t1","sequence":4,"result":{"type":"success"}}"#,
        "\n",
        r#"{"type":"control_boundary","sessionId":"s-cb","turnId":"t2","sequence":5,"boundary":{"kind":"compact","subtype":"compact_boundary","snapshot":{"version":1,"messages":[{"role":"user","content":[{"type":"text","text":"summary"}]}]}}}"#,
        "\n",
        r#"{"type":"turn_started","sessionId":"s-cb","turnId":"t2","sequence":6}"#,
        "\n",
        r#"{"type":"accepted_input","sessionId":"s-cb","turnId":"t2","sequence":7,"messages":[{"role":"user","content":[{"type":"text","text":"new-2"}]}]}"#,
        "\n",
        r#"{"type":"assistant_message","sessionId":"s-cb","turnId":"t2","sequence":8,"message":{"role":"assistant","content":[{"type":"text","text":"new-a2"}]}}"#,
        "\n",
        r#"{"type":"turn_result","sessionId":"s-cb","turnId":"t2","sequence":9,"result":{"type":"success"}}"#,
        "\n"
    );

    let ledger = ContextStateAuthority::new("target-cb");
    let mapping = import_stationary_session(
        jsonl.as_bytes(),
        &LiveSessionActivity::default(),
        &ledger,
        &mut store,
        BackendOwner::KernelFullLate,
    )
    .unwrap();
    assert_eq!(mapping.imported_messages, 3);

    let decoded = boundary_codec::decode(&ledger.snapshot().payload.bytes).unwrap();
    let JsValue::Array(ref items) = decoded else {
        panic!("expected array");
    };
    let rendered: Vec<String> = items
        .iter()
        .map(|m| stringify_json(m).unwrap().unwrap())
        .collect();
    assert!(rendered[0].contains("summary"));
    assert!(rendered[1].contains("new-2"));
    assert!(rendered[2].contains("new-a2"));
}

#[test]
fn import_rerun_is_idempotent_even_after_subsequent_ledger_transitions() {
    let dir = ScratchDir::new("idempotent-after-turn");
    let mut store = SessionOwnershipStore::open(
        dir.path().join("ownership.json"),
        BackendOwner::KernelScopedResident,
    )
    .unwrap();
    let jsonl = minimal_stationary_jsonl("s-orig", "hi");
    let ledger = ContextStateAuthority::new("target-idem");

    let first = import_stationary_session(
        jsonl.as_bytes(),
        &LiveSessionActivity::default(),
        &ledger,
        &mut store,
        BackendOwner::KernelScopedResident,
    )
    .unwrap();
    assert_eq!(first.imported_revision, 1);

    // Advance target ledger further to revision 2 with a subsequent Kernel turn.
    let rev2 = ledger
        .commit_transition(LedgerTransition::new(
            "post-import-turn-1",
            1,
            ContextOperationKind::CaptureTurn.into(),
        ))
        .unwrap();
    assert_eq!(rev2.revision, 2);

    // Re-running import must be completely idempotent and non-duplicating.
    let second = import_stationary_session(
        jsonl.as_bytes(),
        &LiveSessionActivity::default(),
        &ledger,
        &mut store,
        BackendOwner::KernelScopedResident,
    )
    .unwrap();
    assert_eq!(second, first);
    assert_eq!(ledger.snapshot().revision, 2);
}

#[test]
fn import_rejects_different_transcript_into_already_seeded_ledger() {
    let dir = ScratchDir::new("conflict-seeded");
    let mut store = SessionOwnershipStore::open(
        dir.path().join("ownership.json"),
        BackendOwner::KernelScopedResident,
    )
    .unwrap();

    let jsonl_a = minimal_stationary_jsonl("s-a", "alpha");
    let jsonl_b = minimal_stationary_jsonl("s-b", "beta");

    let ledger = ContextStateAuthority::new("target-shared");
    import_stationary_session(
        jsonl_a.as_bytes(),
        &LiveSessionActivity::default(),
        &ledger,
        &mut store,
        BackendOwner::KernelScopedResident,
    )
    .unwrap();

    let err = import_stationary_session(
        jsonl_b.as_bytes(),
        &LiveSessionActivity::default(),
        &ledger,
        &mut store,
        BackendOwner::KernelScopedResident,
    )
    .unwrap_err();
    assert!(matches!(err, MigrationError::OwnershipConflict { .. }));
    assert_eq!(ledger.snapshot().revision, 1);
}

#[test]
fn import_rejects_reusing_legacy_source_session_id_for_kernel_target() {
    let dir = ScratchDir::new("no-mix-id");
    let mut store = SessionOwnershipStore::open(
        dir.path().join("ownership.json"),
        BackendOwner::LegacyRuntime,
    )
    .unwrap();
    store.route_session("sess-mixed").unwrap();

    let jsonl = minimal_stationary_jsonl("sess-mixed", "hello");
    let ledger = ContextStateAuthority::new("sess-mixed");

    let err = import_stationary_session(
        jsonl.as_bytes(),
        &LiveSessionActivity::default(),
        &ledger,
        &mut store,
        BackendOwner::KernelScopedResident,
    )
    .unwrap_err();
    assert!(matches!(
        err,
        MigrationError::OwnershipConflict {
            recorded: BackendOwner::LegacyRuntime,
            requested: BackendOwner::KernelScopedResident,
            ..
        }
    ));
    assert_eq!(ledger.snapshot().revision, 0);
}

#[test]
fn import_for_rollback_creates_new_target_session_without_mutating_source_or_file() {
    let dir = ScratchDir::new("rollback");
    let transcript_path = dir.path().join("kernel-session.jsonl");
    let jsonl = minimal_stationary_jsonl("sess-kernel", "rollback prompt");
    fs::write(&transcript_path, &jsonl).unwrap();
    let before_bytes = fs::read(&transcript_path).unwrap();

    let mut store = SessionOwnershipStore::open(
        dir.path().join("ownership.json"),
        BackendOwner::KernelScopedResident,
    )
    .unwrap();
    let orig_record = store.route_session("sess-kernel").unwrap();
    assert_eq!(orig_record.owner, BackendOwner::KernelScopedResident);

    let audit = audit_jsonl_transcript(&before_bytes, &LiveSessionActivity::default()).unwrap();
    let rollback_target_id = derive_rollback_target_session_id(
        &audit.source_session_id,
        &audit.source_sha256,
        BackendOwner::LegacyRuntime,
        1,
    )
    .unwrap();
    assert_ne!(rollback_target_id, "sess-kernel");

    let rollback_ledger = ContextStateAuthority::new(rollback_target_id.clone());
    let mapping = import_for_rollback(
        &before_bytes,
        &LiveSessionActivity::default(),
        &rollback_ledger,
        &mut store,
        BackendOwner::LegacyRuntime,
    )
    .unwrap();

    assert!(mapping.is_rollback);
    assert_eq!(mapping.source_session_id, "sess-kernel");
    assert_eq!(mapping.target_session_id, rollback_target_id);
    assert_eq!(mapping.target_owner, BackendOwner::LegacyRuntime);
    assert_eq!(mapping.imported_revision, 1);
    assert_eq!(mapping.imported_messages, 2);

    // Source file bytes and original session ownership record remain strictly untouched.
    assert_eq!(fs::read(&transcript_path).unwrap(), before_bytes);
    assert_eq!(store.get("sess-kernel").unwrap().unwrap(), &orig_record);

    let rb_record = store.get(&rollback_target_id).unwrap().unwrap();
    assert_eq!(rb_record.owner, BackendOwner::LegacyRuntime);
    assert_eq!(rb_record.source_session_id.as_deref(), Some("sess-kernel"));
    assert_eq!(
        rb_record.rollback_from_session_id.as_deref(),
        Some("sess-kernel")
    );
    assert_eq!(
        rb_record.source_sha256.as_deref(),
        Some(audit.source_sha256.as_str())
    );
}

#[test]
fn import_for_rollback_rejects_same_session_id_or_existing_unrelated_session() {
    let dir = ScratchDir::new("rollback-guards");
    let mut store = SessionOwnershipStore::open(
        dir.path().join("ownership.json"),
        BackendOwner::KernelScopedResident,
    )
    .unwrap();
    store.route_session("sess-k").unwrap();
    store
        .pin_session("existing-legacy", BackendOwner::LegacyRuntime)
        .unwrap();

    let jsonl = minimal_stationary_jsonl("sess-k", "hi");

    // Reusing source_session_id for rollback must fail closed.
    let same_id_ledger = ContextStateAuthority::new("sess-k");
    assert!(matches!(
        import_for_rollback(
            jsonl.as_bytes(),
            &LiveSessionActivity::default(),
            &same_id_ledger,
            &mut store,
            BackendOwner::LegacyRuntime,
        ),
        Err(MigrationError::OwnershipConflict { .. })
    ));

    // Reusing an unrelated existing session ID for rollback must also fail closed.
    let existing_ledger = ContextStateAuthority::new("existing-legacy");
    assert!(matches!(
        import_for_rollback(
            jsonl.as_bytes(),
            &LiveSessionActivity::default(),
            &existing_ledger,
            &mut store,
            BackendOwner::LegacyRuntime,
        ),
        Err(MigrationError::OwnershipConflict { .. })
    ));

    // An explicit fresh rollback target ID succeeds and is idempotent on retry.
    let fresh_ledger = ContextStateAuthority::new("rb-fresh-1");
    let m1 = import_for_rollback(
        jsonl.as_bytes(),
        &LiveSessionActivity::default(),
        &fresh_ledger,
        &mut store,
        BackendOwner::LegacyRuntime,
    )
    .unwrap();
    let m2 = import_for_rollback(
        jsonl.as_bytes(),
        &LiveSessionActivity::default(),
        &fresh_ledger,
        &mut store,
        BackendOwner::LegacyRuntime,
    )
    .unwrap();
    assert_eq!(m1, m2);
}

#[test]
fn import_transcript_file_and_error_conversions_behave_consistently() {
    let dir = ScratchDir::new("file-import");
    let transcript_path = dir.path().join("sess.jsonl");
    let jsonl = minimal_stationary_jsonl("sess-file", "from disk");
    fs::write(&transcript_path, &jsonl).unwrap();

    let mut store = SessionOwnershipStore::open(
        dir.path().join("ownership.json"),
        BackendOwner::KernelScopedResident,
    )
    .unwrap();
    let ledger = ContextStateAuthority::new("target-file");

    let mapping = import_stationary_transcript_file(
        &transcript_path,
        &LiveSessionActivity::default(),
        &ledger,
        &mut store,
        BackendOwner::KernelScopedResident,
    )
    .unwrap();
    assert_eq!(mapping.source_session_id, "sess-file");
    assert_eq!(mapping.imported_messages, 2);

    let missing_err = import_stationary_transcript_file(
        dir.path().join("missing.jsonl"),
        &LiveSessionActivity::default(),
        &ContextStateAuthority::new("target-missing"),
        &mut store,
        BackendOwner::KernelScopedResident,
    )
    .unwrap_err();
    assert!(matches!(missing_err, MigrationError::Io(_)));
    let iface_err = missing_err.to_iface_error();
    assert_eq!(iface_err.code, ERR_APP);
    assert_eq!(iface_err.app_reason(), Some("MIGRATION_IO_ERROR"));

    // Ledger error mapping propagates reason token faithfully.
    let ledger_err = MigrationError::from(LedgerError::RevisionMismatch {
        expected: 0,
        actual: 1,
    });
    assert_eq!(
        ledger_err.to_iface_error().app_reason(),
        Some("CONTEXT_REVISION_MISMATCH")
    );
}

#[test]
fn context_snapshot_runtime_messages_and_accepted_input_default_role_preserve_order() {
    let dir = ScratchDir::new("ctx-splice");
    let mut store = SessionOwnershipStore::open(
        dir.path().join("ownership.json"),
        BackendOwner::KernelScopedResident,
    )
    .unwrap();

    // `accepted_input` without explicit `role` defaults to `user`, and `context_snapshot`
    // splices `runtimeContextMessages` immediately before the last real user message.
    let jsonl = concat!(
        r#"{"type":"turn_started","sessionId":"s-ctx","turnId":"t1","sequence":1}"#,
        "\n",
        r#"{"type":"accepted_input","sessionId":"s-ctx","turnId":"t1","sequence":2,"messages":[{"content":[{"type":"text","text":"user question"}]}]}"#,
        "\n",
        r#"{"type":"step_started","sessionId":"s-ctx","turnId":"t1","step":1,"sequence":3}"#,
        "\n",
        r#"{"type":"context_snapshot","sessionId":"s-ctx","turnId":"t1","step":1,"sequence":4,"runtimeContextMessages":[{"role":"user","content":[{"type":"text","text":"<memory-context>injected</memory-context>"}],"metadata":{"synthetic":true}}]}"#,
        "\n",
        r#"{"type":"assistant_message","sessionId":"s-ctx","turnId":"t1","sequence":5,"message":{"role":"assistant","content":[{"type":"text","text":"answer"}]}}"#,
        "\n",
        r#"{"type":"step_completed","sessionId":"s-ctx","turnId":"t1","step":1,"outcome":"completed","sequence":6}"#,
        "\n",
        r#"{"type":"turn_result","sessionId":"s-ctx","turnId":"t1","sequence":7,"result":{"type":"success"}}"#,
        "\n"
    );

    let ledger = ContextStateAuthority::new("target-ctx");
    let mapping = import_stationary_session(
        jsonl.as_bytes(),
        &LiveSessionActivity::default(),
        &ledger,
        &mut store,
        BackendOwner::KernelScopedResident,
    )
    .unwrap();
    assert_eq!(mapping.imported_messages, 3);

    let decoded = boundary_codec::decode(&ledger.snapshot().payload.bytes).unwrap();
    let JsValue::Array(ref items) = decoded else {
        panic!("expected array");
    };
    let rendered: Vec<String> = items
        .iter()
        .map(|m| stringify_json(m).unwrap().unwrap())
        .collect();
    assert!(rendered[0].contains("<memory-context>injected</memory-context>"));
    assert!(rendered[1].contains("user question"));
    assert!(rendered[2].contains("answer"));
}

#[test]
fn deterministic_idempotency_key_separates_import_and_rollback() {
    let sha = "a".repeat(64);
    let forward =
        compute_import_idempotency_key("s1", "t1", &sha, BackendOwner::KernelScopedResident, false);
    let rollback =
        compute_import_idempotency_key("s1", "t1", &sha, BackendOwner::LegacyRuntime, true);
    assert_ne!(forward, rollback);
    assert!(forward.starts_with("mig-import-v1:"));
    assert!(rollback.starts_with("mig-rollback-v1:"));
    assert!(parse_json(r#"{"ok":true}"#).is_ok());
}
