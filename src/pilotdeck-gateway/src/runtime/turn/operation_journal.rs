//! Session operation journal over the session's existing shared transcript writer.
//! Blocking storage work is awaited; dropping an async waiter does not authorize
//! re-execution while its write is still owned by the blocking worker.
use crate::{
    host::{HostError, HostErrorKind, HostFuture, operation_journal::OperationJournal},
    storage::{
        KnownTerminal, LedgerError, OperationIdentity, OperationRecovery, OperationResolution,
        SessionOperationLedger, TranscriptSink, TranscriptWriter, UnknownTerminal,
    },
};
use pilotdeck_protocol::{JsObject, JsString, JsValue, module::ModuleOutcome};
use std::sync::{Arc, Mutex};

struct SharedWriterSink {
    writer: Arc<TranscriptWriter>,
    failed: bool,
}
impl TranscriptSink for SharedWriterSink {
    fn check_health(&self) -> crate::storage::Result<()> {
        if self.failed {
            Err(LedgerError::ReopenRequired(
                "Operation journal write failed; reopen the session before retrying.".into(),
            ))
        } else {
            Ok(())
        }
    }
    fn append(
        &mut self,
        session_id: &JsString,
        turn_id: &JsString,
        event: JsObject,
    ) -> crate::storage::Result<()> {
        self.check_health()?;
        let before = self.writer.snapshot_state().sequence;
        let expected = event.clone();
        let result = self
            .writer
            .record_session_event(session_id, turn_id, event)
            .and_then(|()| {
                // The shared TS writer silently ignores appends after close.
                // A transport journal must never publish that no-op as durable.
                let entries = self.writer.read_operation_entries()?;
                if entries.iter().any(|entry| {
                    entry
                        .get("sequence")
                        .and_then(JsValue::as_number)
                        .is_some_and(|sequence| sequence > before as f64)
                        && expected.entries().iter().all(|(key, value)| {
                            entry.as_object().and_then(|object| object.get_key(key)) == Some(value)
                        })
                }) {
                    Ok(())
                } else {
                    Err(crate::storage::AdapterError::Closed(
                        "Shared session writer did not commit the operation event.".into(),
                    ))
                }
            });
        result.map_err(|error| {
            self.failed = true;
            match error {
                crate::storage::AdapterError::Store(error) => error,
                other => LedgerError::Invalid(other.to_string()),
            }
        })
    }
}
struct State {
    ledger: SessionOperationLedger<SharedWriterSink>,
    identity: OperationIdentity,
    stream_id: Option<JsString>,
    unknown_recorded: bool,
}
/// One external execute owns this adapter. Subsequent executes restore through
/// the same writer/store, rather than opening a second file writer or lock.
#[derive(Default)]
struct WorkState {
    closing: bool,
    failed: bool,
    in_flight: usize,
    next_ticket: u64,
    serving: u64,
    finished: std::collections::BTreeSet<u64>,
}
#[derive(Default)]
struct WorkOwner {
    state: Mutex<WorkState>,
    settled: tokio::sync::Notify,
    #[cfg(test)]
    pause: Mutex<Option<(u64, Arc<WorkPause>)>>,
}
#[cfg(test)]
#[derive(Default)]
pub(crate) struct WorkPause {
    pub(crate) entered: tokio::sync::Notify,
    released: Mutex<bool>,
    release: std::sync::Condvar,
}
#[cfg(test)]
impl WorkPause {
    pub(crate) fn open(&self) {
        *self.released.lock().unwrap() = true;
        self.release.notify_all();
    }
}
#[cfg(test)]
impl WorkOwner {
    fn pause_before_state(&self, ticket: u64) {
        let pause = {
            let mut slot = self.pause.lock().unwrap();
            if slot
                .as_ref()
                .is_some_and(|(expected, _)| *expected == ticket)
            {
                slot.take().map(|(_, pause)| pause)
            } else {
                None
            }
        };
        if let Some(pause) = pause {
            pause.entered.notify_one();
            let mut released = pause.released.lock().unwrap();
            while !*released {
                released = pause.release.wait(released).unwrap();
            }
        }
    }
}
struct WorkGuard {
    owner: Arc<WorkOwner>,
    ticket: u64,
    started: bool,
}
impl Drop for WorkGuard {
    fn drop(&mut self) {
        let mut state = self.owner.state.lock().unwrap_or_else(|e| e.into_inner());
        state.in_flight -= 1;
        state.failed |= std::thread::panicking() || !self.started;
        state.finished.insert(self.ticket);
        loop {
            let serving = state.serving;
            if !state.finished.remove(&serving) {
                break;
            }
            state.serving += 1;
        }
        drop(state);
        self.owner.settled.notify_waiters();
    }
}
pub(crate) struct SessionJournal {
    writer: Arc<TranscriptWriter>,
    state: Arc<Mutex<Option<State>>>,
    work_owner: Arc<WorkOwner>,
}
impl SessionJournal {
    pub(crate) fn new(writer: Arc<TranscriptWriter>) -> Arc<Self> {
        Arc::new(Self {
            writer,
            state: Arc::new(Mutex::new(None)),
            work_owner: Arc::default(),
        })
    }
    #[cfg(test)]
    pub(crate) fn pause_work_before_state_for_test(&self, ticket: u64) -> Arc<WorkPause> {
        let pause = Arc::new(WorkPause::default());
        *self.work_owner.pause.lock().unwrap() = Some((ticket, pause.clone()));
        pause
    }
    #[cfg(test)]
    pub(crate) async fn wait_for_registered_work_for_test(&self, count: u64) {
        loop {
            let notified = self.work_owner.settled.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.work_owner.state.lock().unwrap().next_ticket >= count {
                return;
            }
            notified.await;
        }
    }
    fn work<T: Send + 'static>(
        &self,
        action: impl FnOnce(&mut Option<State>, Arc<TranscriptWriter>) -> Result<T, HostError>
        + Send
        + 'static,
    ) -> HostFuture<T> {
        let runtime = match tokio::runtime::Handle::try_current() {
            Ok(runtime) => runtime,
            Err(error) => return Box::pin(async move { Err(journal_error(error)) }),
        };
        let state = self.state.clone();
        let writer = self.writer.clone();
        let owner = self.work_owner.clone();
        // Register synchronously, before returning even an unpolled waiter.
        // Submitted acceptance can never be overtaken by abandonment/close.
        let ticket = {
            let mut gate = owner.state.lock().unwrap_or_else(|e| e.into_inner());
            if gate.closing || gate.failed {
                return Box::pin(async {
                    Err(journal_error(
                        "Operation journal is closing or requires reopen.",
                    ))
                });
            }
            let ticket = gate.next_ticket;
            gate.next_ticket += 1;
            gate.in_flight += 1;
            ticket
        };
        owner.settled.notify_waiters();
        let guard = WorkGuard {
            owner: owner.clone(),
            ticket,
            started: false,
        };
        let task = runtime.spawn(async move {
            // Waiting for the predecessor does not occupy a blocking-pool
            // worker; only the actual store action enters spawn_blocking.
            loop {
                let notified = owner.settled.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                {
                    let gate = owner.state.lock().unwrap_or_else(|e| e.into_inner());
                    if gate.failed {
                        return Err(journal_error(
                            "Operation journal worker failed; reopen required.",
                        ));
                    }
                    if gate.serving == ticket {
                        break;
                    }
                }
                notified.await;
            }
            tokio::task::spawn_blocking(move || {
                let mut guard = guard;
                guard.started = true;
                #[cfg(test)]
                guard.owner.pause_before_state(ticket);
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                action(&mut state, writer)
            })
            .await
            .map_err(|error| HostError::new(HostErrorKind::Protocol, error.to_string()))?
        });
        Box::pin(async move { task.await.map_err(journal_error)? })
    }
}
fn journal_error(error: impl std::fmt::Display) -> HostError {
    HostError::new(HostErrorKind::Protocol, error.to_string())
}
fn active(state: &mut Option<State>) -> Result<&mut State, HostError> {
    state
        .as_mut()
        .ok_or_else(|| journal_error("Operation journal has not started."))
}
impl OperationJournal for SessionJournal {
    fn abandoned(&self, last_applied_sequence: f64, error: JsValue) -> HostFuture<()> {
        self.work(move |slot, _| {
            let Some(state) = slot.as_mut() else {
                return Ok(());
            };
            state.ledger.sink().check_health().map_err(journal_error)?;
            let Some(stream_id) = state.stream_id.clone() else {
                return Ok(());
            };
            if state.unknown_recorded {
                return state.ledger.sink().check_health().map_err(journal_error);
            }
            state
                .ledger
                .result_unknown(&UnknownTerminal {
                    identity: state.identity.clone(),
                    stream_id,
                    last_applied_sequence: Some(last_applied_sequence),
                    code: Some("TRANSPORT_INTERRUPTED".into()),
                    error: Some(error),
                })
                .map_err(journal_error)?;
            state.unknown_recorded = true;
            Ok(())
        })
    }
    fn drain(&self) -> HostFuture<()> {
        let owner = self.work_owner.clone();
        Box::pin(async move {
            {
                let mut state = owner.state.lock().unwrap_or_else(|e| e.into_inner());
                state.closing = true;
            }
            loop {
                let notified = owner.settled.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                {
                    let state = owner.state.lock().unwrap_or_else(|e| e.into_inner());
                    if state.in_flight == 0 {
                        return if state.failed {
                            Err(journal_error(
                                "Operation journal worker failed; reopen required.",
                            ))
                        } else {
                            Ok(())
                        };
                    }
                }
                notified.await;
            }
        })
    }
    fn begin(&self, identity: OperationIdentity) -> HostFuture<Option<OperationResolution>> {
        self.work(move |slot, writer| {
            if slot.is_some() {
                return Err(journal_error("Operation journal is already bound."));
            }
            let entries = writer.read_operation_entries().map_err(journal_error)?;
            let mut ledger = SessionOperationLedger::new(
                identity.session_id.clone(),
                SharedWriterSink {
                    writer,
                    failed: false,
                },
                &entries,
            )
            .map_err(journal_error)?;
            match ledger.recover(&identity).map_err(journal_error)? {
                Some(OperationRecovery::Terminal(resolution)) => return Ok(Some(resolution)),
                Some(OperationRecovery::Incomplete) => {
                    return Err(journal_error(
                        "A recovered sidecar operation is incomplete and cannot be safely re-executed.",
                    ));
                }
                Some(OperationRecovery::ResultUnknown(_)) => {
                    return Err(HostError::new(
                        HostErrorKind::ResultUnknown,
                        "A recovered sidecar operation has an unknown terminal and cannot be safely re-executed.",
                    ));
                }
                None => {}
            }
            ledger.start(&identity).map_err(journal_error)?;
            *slot = Some(State {
                ledger,
                identity,
                stream_id: None,
                unknown_recorded: false,
            });
            Ok(None)
        })
    }
    fn accepted(&self, stream_id: JsString) -> HostFuture<()> {
        self.work(move |slot, _| {
            let state = active(slot)?;
            state
                .ledger
                .accept(&state.identity, &stream_id)
                .map_err(journal_error)?;
            state.stream_id = Some(stream_id);
            Ok(())
        })
    }
    fn terminal(
        &self,
        outcome: ModuleOutcome,
        last_applied_sequence: f64,
        projected: JsValue,
    ) -> HostFuture<()> {
        self.work(move |slot, _| {
            let state = active(slot)?;
            let result = projected
                .get("result")
                .cloned()
                .ok_or_else(|| journal_error("Projected operation terminal lacks result."))?;
            let messages = projected
                .get("messages")
                .and_then(|value| match value {
                    JsValue::Array(values) => Some(values),
                    _ => None,
                })
                .ok_or_else(|| journal_error("Projected operation terminal lacks messages."))?
                .to_vec();
            state
                .ledger
                .terminal(&KnownTerminal {
                    identity: state.identity.clone(),
                    stream_id: state.stream_id.clone(),
                    last_applied_sequence,
                    resolution: OperationResolution {
                        outcome,
                        result,
                        messages,
                        seed_state: projected.get("seedState").cloned(),
                    },
                    code: projected.get("code").and_then(JsValue::as_string).cloned(),
                    error: projected.get("error").cloned(),
                })
                .map_err(journal_error)
        })
    }
    fn result_unknown(
        &self,
        stream_id: JsString,
        last_applied_sequence: f64,
        code: Option<JsString>,
        error: Option<JsValue>,
    ) -> HostFuture<()> {
        self.work(move |slot, _| {
            let state = active(slot)?;
            state
                .ledger
                .result_unknown(&UnknownTerminal {
                    identity: state.identity.clone(),
                    stream_id,
                    last_applied_sequence: Some(last_applied_sequence),
                    code,
                    error,
                })
                .map_err(journal_error)?;
            state.unknown_recorded = true;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{TranscriptKey, TranscriptRead, TranscriptStore, recorder::Recorder};
    use pilotdeck_protocol::module::ModuleBinding;
    use std::sync::{
        Condvar,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    struct Gate {
        kind: &'static str,
        entered: tokio::sync::Notify,
        released: Mutex<bool>,
        release: Condvar,
    }
    impl Gate {
        fn new(kind: &'static str) -> Arc<Self> {
            Arc::new(Self {
                kind,
                entered: Default::default(),
                released: Mutex::new(false),
                release: Condvar::new(),
            })
        }
        fn open(&self) {
            *self.released.lock().unwrap() = true;
            self.release.notify_all();
        }
    }
    struct ReleaseGate(Arc<Gate>);
    impl Drop for ReleaseGate {
        fn drop(&mut self) {
            self.0.open();
        }
    }
    #[derive(Default)]
    struct Store {
        entries: Mutex<Vec<JsValue>>,
        fail: AtomicBool,
        appends: AtomicUsize,
        gate: Mutex<Option<Arc<Gate>>>,
    }
    impl TranscriptStore for Store {
        fn append(&self, _key: &TranscriptKey, entry: &JsValue) -> crate::storage::StoreResult<()> {
            self.appends.fetch_add(1, Ordering::SeqCst);
            let gate = {
                let mut slot = self.gate.lock().unwrap();
                if slot.as_ref().is_some_and(|g| {
                    entry
                        .get("type")
                        .and_then(JsValue::as_string)
                        .is_some_and(|s| s.equals_str(g.kind))
                }) {
                    slot.take()
                } else {
                    None
                }
            };
            if let Some(gate) = gate {
                gate.entered.notify_one();
                let mut released = gate.released.lock().unwrap();
                while !*released {
                    released = gate.release.wait(released).unwrap();
                }
            }
            if self.fail.load(Ordering::SeqCst) {
                return Err(LedgerError::Io(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "journal fixture write denied",
                )));
            }
            self.entries.lock().unwrap().push(entry.clone());
            Ok(())
        }
        fn read(
            &self,
            _key: &TranscriptKey,
            _max: Option<u64>,
        ) -> crate::storage::StoreResult<TranscriptRead> {
            Ok(TranscriptRead {
                entries: self.entries.lock().unwrap().clone(),
                diagnostics: vec![],
            })
        }
    }
    fn setup() -> (
        Arc<Store>,
        Arc<Recorder>,
        Arc<SessionJournal>,
        OperationIdentity,
    ) {
        let store = Arc::new(Store::default());
        let recorder = Arc::new(Recorder::new(
            store.clone(),
            TranscriptKey {
                project_root: "/journal-project".into(),
                pilot_home: "/journal-home".into(),
                session_id: "journal-session".into(),
                transcript_path: "/journal-home/session.jsonl".into(),
            },
        ));
        let id = OperationIdentity {
            run_id: "run-1".into(),
            operation_id: "operation-1".into(),
            request_id: "request-1".into(),
            session_id: "journal-session".into(),
            turn_id: "turn-1".into(),
            binding: ModuleBinding {
                module_instance_id: "actual-peer".into(),
                connection_generation: "actual-generation".into(),
            },
            idempotency_key: Some("key-1".into()),
        };
        recorder.start_turn(&id.session_id, &id.turn_id).unwrap();
        let journal = SessionJournal::new(recorder.writer().clone());
        (store, recorder, journal, id)
    }
    fn terminal(id: &OperationIdentity) -> JsValue {
        let mut result = JsObject::new();
        result.insert("type".into(), "success".into());
        result.insert("sessionId".into(), JsValue::String(id.session_id.clone()));
        result.insert("turnId".into(), JsValue::String(id.turn_id.clone()));
        let mut payload = JsObject::new();
        payload.insert("result".into(), JsValue::Object(result));
        payload.insert("messages".into(), JsValue::Array(vec![]));
        JsValue::Object(payload)
    }
    fn kinds(store: &Store) -> Vec<String> {
        store
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| e.get("type").and_then(JsValue::as_string))
            .map(|s| String::from_utf16_lossy(s.units()))
            .collect()
    }
    #[tokio::test]
    async fn journal_shared_writer_commits_three_boundaries_and_recovers_without_append() {
        let (store, recorder, journal, id) = setup();
        assert!(journal.begin(id.clone()).await.unwrap().is_none());
        journal.accepted("actual-stream".into()).await.unwrap();
        journal
            .terminal(ModuleOutcome::Completed, 7.0, terminal(&id))
            .await
            .unwrap();
        let expected = vec![
            "turn_started",
            "agent_loop_operation_started",
            "agent_loop_operation_accepted",
            "agent_loop_operation_terminal",
        ];
        assert_eq!(kinds(&store), expected);
        let before = store.appends.load(Ordering::SeqCst);
        let reopened = SessionJournal::new(recorder.writer().clone());
        let recovered = reopened.begin(id).await.unwrap().unwrap();
        assert_eq!(recovered.outcome, ModuleOutcome::Completed);
        assert_eq!(
            store.appends.load(Ordering::SeqCst),
            before,
            "recovery must not resend or append a fresh operation"
        );
    }
    #[tokio::test]
    async fn journal_write_failure_is_visible_and_retry_is_not_false_success() {
        let (store, recorder, journal, id) = setup();
        journal.begin(id.clone()).await.unwrap();
        store.fail.store(true, Ordering::SeqCst);
        assert!(
            journal
                .accepted("actual-stream".into())
                .await
                .unwrap_err()
                .message
                .contains("fixture write denied")
        );
        let attempted = store.appends.load(Ordering::SeqCst);
        store.fail.store(false, Ordering::SeqCst);
        assert!(journal.accepted("actual-stream".into()).await.is_err());
        assert_eq!(store.appends.load(Ordering::SeqCst), attempted);
        let reopened = SessionJournal::new(recorder.writer().clone());
        assert!(
            reopened
                .begin(id)
                .await
                .unwrap_err()
                .message
                .contains("incomplete")
        );
        assert!(
            !kinds(&store)
                .iter()
                .any(|kind| kind == "agent_loop_operation_accepted")
        );
    }
    #[tokio::test]
    async fn journal_unknown_is_durable_and_never_authorizes_reexecute() {
        let (store, recorder, journal, id) = setup();
        journal.begin(id.clone()).await.unwrap();
        journal.accepted("actual-stream".into()).await.unwrap();
        journal
            .result_unknown(
                "actual-stream".into(),
                2.0,
                Some("TRANSPORT_INTERRUPTED".into()),
                None,
            )
            .await
            .unwrap();
        let before = store.appends.load(Ordering::SeqCst);
        journal
            .abandoned(2.0, "another observation".into())
            .await
            .unwrap();
        assert_eq!(store.appends.load(Ordering::SeqCst), before);
        let reopened = SessionJournal::new(recorder.writer().clone());
        assert_eq!(
            reopened.begin(id).await.unwrap_err().kind,
            HostErrorKind::ResultUnknown
        );
        assert_eq!(store.appends.load(Ordering::SeqCst), before);
    }
    #[tokio::test]
    async fn closed_shared_writer_is_not_a_successful_journal_noop() {
        let (store, recorder, journal, id) = setup();
        recorder.writer().close();
        assert!(
            journal
                .begin(id)
                .await
                .unwrap_err()
                .message
                .contains("closed")
        );
        assert_eq!(kinds(&store), ["turn_started"]);
    }
    async fn dropped_write_is_owned_until_drain(kind: &'static str) {
        use std::{future::poll_fn, task::Poll};
        let (store, _recorder, journal, id) = setup();
        if kind != "agent_loop_operation_started" {
            journal.begin(id.clone()).await.unwrap();
        }
        if kind == "agent_loop_operation_terminal" {
            journal.accepted("actual-stream".into()).await.unwrap();
        }
        let gate = Gate::new(kind);
        let _release_on_drop = ReleaseGate(gate.clone());
        *store.gate.lock().unwrap() = Some(gate.clone());
        let task = match kind {
            "agent_loop_operation_started" => tokio::spawn(journal.begin(id.clone())),
            _ => {
                let work = if kind == "agent_loop_operation_accepted" {
                    journal.accepted("actual-stream".into())
                } else {
                    journal.terminal(ModuleOutcome::Completed, 3.0, terminal(&id))
                };
                tokio::spawn(async move { work.await.map(|()| None) })
            }
        };
        gate.entered.notified().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let mut drain = journal.drain();
        poll_fn(|cx| {
            assert!(
                drain.as_mut().poll(cx).is_pending(),
                "drain cannot report settled while the store still owns the write"
            );
            Poll::Ready(())
        })
        .await;
        assert!(
            journal.accepted("later-stream".into()).await.is_err(),
            "closing gate rejects another lifecycle"
        );
        gate.open();
        drain.await.unwrap();
        assert_eq!(
            kinds(&store)
                .iter()
                .filter(|item| item.as_str() == kind)
                .count(),
            1
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropped_begin_waiter_drain_waits_for_real_writer() {
        dropped_write_is_owned_until_drain("agent_loop_operation_started").await;
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropped_accept_waiter_drain_waits_for_real_writer() {
        dropped_write_is_owned_until_drain("agent_loop_operation_accepted").await;
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropped_terminal_waiter_drain_waits_for_real_writer() {
        dropped_write_is_owned_until_drain("agent_loop_operation_terminal").await;
    }
}
