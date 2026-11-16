//! Integration tests for `pilotdeck-context-iface`.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use agentcpm_iface::BoxFuture;
use agentcpm_iface::context::{CompactionPersistenceContract, ContextContract, TokenCounter};
use agentcpm_iface::store::{EventRecord, StoreContract};
use agentcpm_iface::types::{CallMeta, Cursor, Error, ErrorCategory, NodeId, SessionId};
use pilotdeck_context_core::{MicroCompactionEngineOptions, SnipCallOptions, SnipEngineOptions};
use pilotdeck_context_iface::{
    AuthoritativeContextLedger, CONTEXT_LEDGER_PROTOCOL_VERSION, ContextControl,
    ContextOperationKind, KIND_COMPACTION, KIND_CONTEXT_INPUT, KIND_TOOL_REGISTERED, LedgerError,
    LedgerTransition, MESSAGES_PER_TURN_OVERHEAD, PilotDeckCompactionPersistence,
    PilotDeckContextContract, PilotDeckTokenCounter, SNAPSHOT_CONTENT_TYPE,
    compute_compaction_digest,
};
use pilotdeck_context_runtime::boundary_codec;
use pilotdeck_context_runtime::history::{Commit, History, HistoryError, Payload};
use pilotdeck_context_runtime::operations::Operation;
use pilotdeck_protocol::parse_json;
use serde_json::Value;

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut cx) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("test future must resolve synchronously"),
    }
}

#[derive(Default)]
struct TestStore {
    sessions: Mutex<BTreeMap<SessionId, Vec<EventRecord>>>,
    checkpoint_error: Mutex<Option<String>>,
    force_bad_page_session: Mutex<bool>,
}

impl TestStore {
    fn inject_event(&self, session: &SessionId, kind: &str, payload: Vec<u8>) {
        let mut sessions = self.sessions.lock().unwrap();
        let log = sessions.entry(session.clone()).or_default();
        let cursor = log.len() as Cursor + 1;
        let mut record = EventRecord::new(session.clone(), kind, payload);
        record.cursor = cursor;
        log.push(record);
    }

    fn replace_last_payload(&self, session: &SessionId, payload: Vec<u8>) {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(log) = sessions.get_mut(session)
            && let Some(last) = log.last_mut()
        {
            last.payload = payload;
        }
    }
}

impl StoreContract for TestStore {
    fn append(&self, mut record: EventRecord) -> BoxFuture<'_, Result<Cursor, Error>> {
        let session = record.session.clone();
        Box::pin(async move {
            let mut sessions = self.sessions.lock().unwrap();
            let log = sessions.entry(session).or_default();
            let cursor = log.len() as Cursor + 1;
            record.cursor = cursor;
            log.push(record);
            Ok(cursor)
        })
    }

    fn read(
        &self,
        session: &SessionId,
        from: Cursor,
        limit: usize,
    ) -> BoxFuture<'_, Result<Vec<EventRecord>, Error>> {
        let session = session.clone();
        let bad_session = *self.force_bad_page_session.lock().unwrap();
        Box::pin(async move {
            let sessions = self.sessions.lock().unwrap();
            let mut out: Vec<EventRecord> = sessions
                .get(&session)
                .map(|log| {
                    log.iter()
                        .filter(|event| event.cursor >= from)
                        .take(if limit == 0 { usize::MAX } else { limit })
                        .cloned()
                        .map(|mut event| {
                            if bad_session {
                                event.session = "foreign-session".to_string();
                            }
                            event
                        })
                        .collect()
                })
                .unwrap_or_default();
            out.sort_by_key(|event| event.cursor);
            Ok(out)
        })
    }

    fn last_cursor(&self, session: &SessionId) -> BoxFuture<'_, Result<Cursor, Error>> {
        let session = session.clone();
        Box::pin(async move {
            Ok(self
                .sessions
                .lock()
                .unwrap()
                .get(&session)
                .map_or(0, |log| log.len() as Cursor))
        })
    }

    fn checkpoint(&self, _session: &SessionId) -> BoxFuture<'_, Result<String, Error>> {
        Box::pin(async { Ok("cp-ok".to_string()) })
    }

    fn verify_checkpoint(
        &self,
        _session: &SessionId,
    ) -> BoxFuture<'_, Result<Option<(Cursor, String)>, Error>> {
        let err = self.checkpoint_error.lock().unwrap().clone();
        Box::pin(async move {
            if let Some(msg) = err {
                return Err(Error::new(ErrorCategory::Internal, -32000, msg));
            }
            Ok(None)
        })
    }

    fn create_session(&self, session: &SessionId) -> BoxFuture<'_, Result<(), Error>> {
        let session = session.clone();
        Box::pin(async move {
            self.sessions.lock().unwrap().entry(session).or_default();
            Ok(())
        })
    }

    fn contains_session(&self, session: &SessionId) -> BoxFuture<'_, Result<bool, Error>> {
        let session = session.clone();
        Box::pin(async move { Ok(self.sessions.lock().unwrap().contains_key(&session)) })
    }

    fn list_sessions(&self) -> BoxFuture<'_, Result<Vec<SessionId>, Error>> {
        Box::pin(async move { Ok(self.sessions.lock().unwrap().keys().cloned().collect()) })
    }

    fn node(&self) -> Option<&NodeId> {
        None
    }
}

#[test]
fn trait_objects_are_object_safe_and_consistent() {
    let store = Arc::new(TestStore::default());
    let counter: Arc<dyn TokenCounter> = Arc::new(PilotDeckTokenCounter::new());
    let context: Arc<dyn ContextContract> = Arc::new(PilotDeckContextContract::new(store.clone()));
    let compaction: Arc<dyn CompactionPersistenceContract> =
        Arc::new(PilotDeckCompactionPersistence::new(store));
    let ledger = AuthoritativeContextLedger::new("s-obj");
    let history: Arc<dyn History> = Arc::new(ledger);

    assert_eq!(counter.count_text(""), 0);
    assert_eq!(MESSAGES_PER_TURN_OVERHEAD, 4);
    assert!(
        block_on(context.rebuild_capabilities(&"s-obj".into()))
            .unwrap()
            .is_empty()
    );
    assert!(
        block_on(compaction.latest(&"s-obj".into()))
            .unwrap()
            .is_none()
    );
    assert_eq!(block_on(history.read()).unwrap().revision, 0);
}

#[test]
fn ledger_concurrent_cas_allows_exactly_one_winner_and_preserves_idempotency() {
    let ledger = AuthoritativeContextLedger::new("s-race");
    let payload_a = Payload {
        profile: "pilotdeck.messages.json".into(),
        content_type: "application/json".into(),
        bytes: br#"[{"role":"user","content":[{"type":"text","text":"branch A"}]}]"#.to_vec(),
    };
    let payload_b = Payload {
        profile: "pilotdeck.messages.json".into(),
        content_type: "application/json".into(),
        bytes: br#"[{"role":"user","content":[{"type":"text","text":"branch B"}]}]"#.to_vec(),
    };

    let (res_a, res_b) = std::thread::scope(|scope| {
        let l1 = ledger.clone();
        let l2 = ledger.clone();
        let a = scope.spawn(move || {
            l1.commit_transition(
                LedgerTransition::new("tx-a", 0, Operation::PrepareForModel)
                    .with_replacement(payload_a)
                    .with_consumed_inputs(vec!["shared-input".into()]),
            )
        });
        let b = scope.spawn(move || {
            l2.commit_transition(
                LedgerTransition::new("tx-b", 0, Operation::PrepareForModel)
                    .with_replacement(payload_b)
                    .with_consumed_inputs(vec!["shared-input".into()]),
            )
        });
        (a.join().unwrap(), b.join().unwrap())
    });

    let (winner, loser) = match (res_a, res_b) {
        (Ok(win), Err(lose)) | (Err(lose), Ok(win)) => (win, lose),
        other => panic!("expected exactly one CAS winner and one loser, got {other:?}"),
    };
    assert_eq!(winner.revision, 1);
    assert_eq!(
        loser,
        LedgerError::RevisionMismatch {
            expected: 0,
            actual: 1
        }
    );
    assert!(ledger.is_input_consumed("shared-input"));

    // Preview-only transitions cannot mutate state.
    let mut preview = ContextControl::default();
    assert_eq!(preview.protocol_version, CONTEXT_LEDGER_PROTOCOL_VERSION);
    preview.preview_only = true;
    let err = ledger
        .commit_transition(
            LedgerTransition::new("tx-preview", 1, Operation::PrepareForModel)
                .with_control(preview)
                .with_consumed_inputs(vec!["in-2".into()]),
        )
        .unwrap_err();
    assert!(matches!(err, LedgerError::InvalidTransition(_)));
    assert!(!ledger.is_input_consumed("in-2"));
}

#[test]
fn context_contract_applies_micro_and_snip_without_mutating_authority() {
    let store = Arc::new(TestStore::default());
    let session: SessionId = "s-full".into();
    block_on(store.create_session(&session)).unwrap();
    store.inject_event(&session, KIND_TOOL_REGISTERED, b"read_file|{}|9".to_vec());
    store.inject_event(&session, KIND_TOOL_REGISTERED, b"read_file|{}|9".to_vec());

    let big_text = "alpha beta gamma delta ".repeat(60);
    let raw_json = format!(
        r#"[
            {{"role":"user","content":[{{"type":"text","text":"t1"}}]}},
            {{"role":"assistant","content":[{{"type":"tool_call","id":"c1","name":"read_file","input":{{"path":"a"}}}}]}},
            {{"role":"user","content":[{{"type":"tool_result","toolCallId":"c1","content":[{{"type":"text","text":"{big_text}"}}]}}]}},
            {{"role":"user","content":[{{"type":"text","text":"t2"}}]}},
            {{"role":"assistant","content":[{{"type":"text","text":"a2"}}]}},
            {{"role":"user","content":[{{"type":"text","text":"t3"}}]}},
            {{"role":"assistant","content":[{{"type":"text","text":"a3"}}]}},
            {{"role":"user","content":[{{"type":"text","text":"t4"}}]}},
            {{"role":"assistant","content":[{{"type":"text","text":"a4"}}]}},
            {{"role":"user","content":[{{"type":"text","text":"t5"}}]}},
            {{"role":"assistant","content":[{{"type":"text","text":"a5"}}]}},
            {{"role":"user","content":[{{"type":"text","text":"t6"}}]}},
            {{"role":"assistant","content":[{{"type":"text","text":"a6"}}]}},
            {{"role":"user","content":[{{"type":"text","text":"t7"}}]}},
            {{"role":"assistant","content":[{{"type":"text","text":"a7"}}]}}
        ]"#
    );
    let messages_val = parse_json(&raw_json).unwrap();
    let payload = Payload {
        profile: boundary_codec::PROFILE.into(),
        content_type: "application/vnd.pilotdeck.context.boundary.v1".into(),
        bytes: boundary_codec::encode(&messages_val).unwrap(),
    };

    let ledger =
        AuthoritativeContextLedger::with_initial_payload(session.clone(), payload.clone()).unwrap();
    assert_eq!(ledger.snapshot().revision, 0);

    let contract = PilotDeckContextContract::new(store.clone())
        .with_ledger(ledger.clone())
        .with_capabilities(vec![("context.core".into(), "0.1.0".into())])
        .with_micro_options(MicroCompactionEngineOptions {
            keep_latest: Some(0.0),
            trim_to_tokens: Some(64.0),
            ..Default::default()
        })
        .with_snip_options(SnipEngineOptions {
            keep_head_turns: Some(1.0),
            keep_tail_turns: Some(2.0),
            enabled: Some(true),
            ..Default::default()
        })
        .with_snip_call_options(SnipCallOptions {
            target_tokens: Some(32.0),
            target_total_tokens: None,
        });

    let snap = block_on(contract.assemble(&session, 7)).unwrap();
    assert_eq!(snap.content_type.as_deref(), Some(SNAPSHOT_CONTENT_TYPE));
    let doc: Value = serde_json::from_slice(&snap.bytes).unwrap();
    assert_eq!(doc["ledger"]["projections"]["micro_applied"], true);
    assert_eq!(doc["ledger"]["projections"]["snip_applied"], true);

    // Authority remains untouched at revision 0 with original uncompacted payload.
    let view = ledger.snapshot();
    assert_eq!(view.revision, 0);
    assert_eq!(view.payload, payload);

    // Trace write-ahead appends `context.input`.
    let trace_id = block_on(contract.trace_input(
        &session,
        7,
        &CallMeta {
            session_id: session.clone(),
            call_id: "call-7".into(),
            turn: 7,
        },
        b"model prompt",
    ))
    .unwrap();
    assert!(trace_id.starts_with("trace-"));
    let events = block_on(store.read(&session, 1, 10)).unwrap();
    assert_eq!(events.last().unwrap().kind, KIND_CONTEXT_INPUT);

    // Corrupt store session mismatch fails closed.
    *store.force_bad_page_session.lock().unwrap() = true;
    let err = block_on(contract.assemble(&session, 8)).unwrap_err();
    assert_eq!(err.category, ErrorCategory::Internal);
    assert_eq!(err.code, -32000);
}

#[test]
fn compaction_persistence_verifies_digest_and_rejects_corruption() {
    let store = Arc::new(TestStore::default());
    let persistence = PilotDeckCompactionPersistence::new(store.clone());
    let session: SessionId = "s-comp".into();
    block_on(store.create_session(&session)).unwrap();

    let boundary = b"boundary-bytes-\x80\xff";
    let messages = b"messages-bytes-\x00\xfe";
    let expected_digest = compute_compaction_digest(9, boundary, messages);

    let commit = block_on(persistence.commit(&session, 9, boundary, messages)).unwrap();
    assert_eq!(commit.digest, expected_digest);
    assert_eq!(commit.boundary_len, boundary.len());
    assert_eq!(commit.messages_len, messages.len());

    let loaded = block_on(persistence.load(&session)).unwrap().unwrap();
    assert_eq!(loaded.commit, commit);
    assert_eq!(loaded.boundary, boundary);
    assert_eq!(loaded.messages, messages);

    // Unsupported schema version fails closed.
    store.inject_event(
        &session,
        KIND_COMPACTION,
        br#"{"version":99,"turn":1,"digest":"0000000000000000000000000000000000000000000000000000000000000000","boundary_len":0,"messages_len":0}
"#.to_vec(),
    );
    let err = block_on(persistence.latest(&session)).unwrap_err();
    assert_eq!(err.category, ErrorCategory::Internal);
    assert_eq!(err.code, -32000);
    assert!(err.message.contains("COMPACTION_CORRUPT"));

    // Trailing garbage past declared lengths fails closed.
    let valid_payload = format!(
        "{}\n",
        serde_json::json!({
            "version": 1,
            "turn": 9,
            "digest": expected_digest,
            "boundary_len": boundary.len(),
            "messages_len": messages.len(),
        })
    );
    let mut extended = valid_payload.into_bytes();
    extended.extend_from_slice(boundary);
    extended.extend_from_slice(messages);
    extended.push(b'!');
    store.replace_last_payload(&session, extended);

    let err = block_on(persistence.load(&session)).unwrap_err();
    assert_eq!(err.category, ErrorCategory::Internal);
    assert_eq!(err.code, -32000);
    assert!(err.message.contains("COMPACTION_CORRUPT"));

    // All five operations are represented distinctly in ContextOperationKind.
    assert_eq!(ContextOperationKind::all().len(), 5);
    assert_eq!(
        block_on(AuthoritativeContextLedger::new("x").commit(Commit {
            transaction_id: "".into(),
            expected_revision: 0,
            replacement: None,
            consumed_inputs: vec![],
            facts: vec![],
            compaction: None,
        })),
        Err(HistoryError::Invalid(
            "transaction_id must be 1..=256 bytes".into()
        ))
    );
}
