//! Real stdio ownership with the real shared-writer journal, including dropped waiters.
use super::*;
use crate::host::operation_journal::OperationJournal;
use crate::{
    runtime::turn::operation_journal::SessionJournal,
    storage::{
        JsonlTranscriptStore, LedgerError, StoreResult, TranscriptKey, TranscriptRead,
        TranscriptStore, recorder::Recorder,
    },
};
use std::sync::{Condvar, Mutex};

struct StoreGate {
    kind: &'static str,
    entered: Notify,
    released: Mutex<bool>,
    release: Condvar,
}
impl StoreGate {
    fn new(kind: &'static str) -> Arc<Self> {
        Arc::new(Self {
            kind,
            entered: Notify::new(),
            released: Mutex::new(false),
            release: Condvar::new(),
        })
    }
    fn open(&self) {
        *self.released.lock().unwrap() = true;
        self.release.notify_all();
    }
}
struct ReleaseGate(Arc<StoreGate>);
impl Drop for ReleaseGate {
    fn drop(&mut self) {
        self.0.open();
    }
}
struct GatedFileStore {
    actual: JsonlTranscriptStore,
    gate: Mutex<Option<Arc<StoreGate>>>,
    fail_terminal: AtomicBool,
}
impl TranscriptStore for GatedFileStore {
    fn append(&self, key: &TranscriptKey, entry: &JsValue) -> StoreResult<()> {
        let gate = {
            let mut slot = self.gate.lock().unwrap();
            if slot.as_ref().is_some_and(|g| {
                entry
                    .get("type")
                    .and_then(JsValue::as_string)
                    .is_some_and(|v| v.equals_str(g.kind))
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
        if self.fail_terminal.load(Ordering::SeqCst)
            && entry.get("type") == Some(&"agent_loop_operation_terminal".into())
        {
            return Err(LedgerError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "actual journal store write denied",
            )));
        }
        self.actual.append(key, entry)
    }
    fn read(&self, key: &TranscriptKey, max: Option<u64>) -> StoreResult<TranscriptRead> {
        self.actual.read(key, max)
    }
}
fn journal(
    dir: &std::path::Path,
) -> (
    Arc<GatedFileStore>,
    Arc<Recorder>,
    Arc<SessionJournal>,
    std::path::PathBuf,
) {
    let path = dir.join("actual-session.jsonl");
    let store = Arc::new(GatedFileStore {
        actual: JsonlTranscriptStore::new(),
        gate: Mutex::new(None),
        fail_terminal: AtomicBool::new(false),
    });
    let recorder = Arc::new(Recorder::new(
        store.clone(),
        TranscriptKey {
            project_root: dir.to_owned(),
            pilot_home: dir.to_owned(),
            session_id: "session".into(),
            transcript_path: path.clone(),
        },
    ));
    recorder
        .start_turn(&"session".into(), &"turn".into())
        .unwrap();
    let ledger = SessionJournal::new(recorder.writer().clone());
    (store, recorder, ledger, path)
}
fn payload(result: ExecuteResult) -> (JsValue, crate::host::operation_journal::OperationFinalizer) {
    let ExecuteResult::Journal { result, finalizer } = result else {
        panic!("external journal finalizer missing")
    };
    let ExecuteResult::Terminal(terminal) = *result else {
        panic!("real peer did not return normal terminal")
    };
    (terminal.get("payload").cloned().unwrap(), finalizer)
}
#[cfg(unix)]
fn assert_live(pid: u32) {
    rustix::process::test_kill_process(rustix::process::Pid::from_raw(pid as i32).unwrap())
        .expect("peer closed before journal settled");
}
fn operation_entries(path: &std::path::Path) -> Vec<JsValue> {
    crate::storage::read_transcript(path)
        .unwrap()
        .entries
        .into_iter()
        .filter(|e| {
            e.get("type").and_then(JsValue::as_string).is_some_and(|t| {
                String::from_utf16_lossy(t.units()).starts_with("agent_loop_operation_")
            })
        })
        .collect()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn external_finalizer_persists_projected_result_before_real_close() {
    let (mut command, receipt) = peer("positive");
    let dir = receipt.0.parent().unwrap();
    let (_store, _recorder, journal, path) = journal(dir);
    let client = HostClient::spawn(&mut command, None, limits())
        .await
        .unwrap()
        .with_operation_journal(journal);
    let pid = client.child_id().unwrap();
    let handler = Handler::new("positive");
    let (mut projected, finalizer) =
        payload(client.execute(request(), handler.clone()).await.unwrap());
    assert_live(pid);
    assert_eq!(handler.disposed.load(Ordering::SeqCst), 0);
    assert_eq!(
        operation_entries(&path).len(),
        2,
        "known result cannot be persisted before actual host projection"
    );
    if let JsValue::Object(fields) = &mut projected {
        let mut result = fields.get("result").unwrap().as_object().unwrap().clone();
        result.insert("fixtureProjectionWitness".into(), true.into());
        fields.insert("result".into(), JsValue::Object(result));
    }
    finalizer.settle(Ok(projected)).await.unwrap();
    assert_reaped(pid).await;
    assert_eq!(handler.disposed.load(Ordering::SeqCst), 1);
    receipt.assert_pass("positive");
    let entries = operation_entries(&path);
    assert_eq!(entries.len(), 3);
    assert_eq!(
        entries[2]
            .get("result")
            .unwrap()
            .get("fixtureProjectionWitness"),
        Some(&true.into())
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn dropped_terminal_waiter_keeps_real_child_until_writer_and_dispose_settle() {
    let (mut command, receipt) = peer("positive");
    let (store, _recorder, journal, path) = journal(receipt.0.parent().unwrap());
    let client = HostClient::spawn(&mut command, None, limits())
        .await
        .unwrap()
        .with_operation_journal(journal);
    let pid = client.child_id().unwrap();
    let handler = Handler::new("positive");
    let (projected, finalizer) = payload(client.execute(request(), handler.clone()).await.unwrap());
    let gate = StoreGate::new("agent_loop_operation_terminal");
    let _release_on_drop = ReleaseGate(gate.clone());
    *store.gate.lock().unwrap() = Some(gate.clone());
    let waiter = tokio::spawn(finalizer.settle(Ok(projected)));
    gate.entered.notified().await;
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    assert_live(pid);
    assert_eq!(handler.disposed.load(Ordering::SeqCst), 0);
    assert_eq!(operation_entries(&path).len(), 2);
    gate.open();
    assert_reaped(pid).await;
    wait_for_event("journal-owned disposer", || {
        handler.disposed.load(Ordering::SeqCst) == 1
    })
    .await;
    receipt.assert_pass("positive");
    assert_eq!(
        operation_entries(&path).len(),
        3,
        "exactly one terminal commit after waiter cancellation"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn dropped_accept_waiter_owns_write_then_unknown_then_real_child_close() {
    let (mut command, receipt) = peer("positive");
    let (store, _recorder, journal, path) = journal(receipt.0.parent().unwrap());
    let gate = StoreGate::new("agent_loop_operation_accepted");
    let _release_on_drop = ReleaseGate(gate.clone());
    *store.gate.lock().unwrap() = Some(gate.clone());
    let client = HostClient::spawn(&mut command, None, limits())
        .await
        .unwrap()
        .with_operation_journal(journal);
    let pid = client.child_id().unwrap();
    let handler = Handler::new("positive");
    let waiter = tokio::spawn(client.execute(request(), handler.clone()));
    gate.entered.notified().await;
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    assert_live(pid);
    assert_eq!(handler.disposed.load(Ordering::SeqCst), 0);
    gate.open();
    assert_reaped(pid).await;
    wait_for_event("abandoned journal disposer", || {
        handler.disposed.load(Ordering::SeqCst) == 1
    })
    .await;
    let entries = operation_entries(&path);
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[2].get("outcome"), Some(&"result_unknown".into()));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn journal_terminal_write_failure_never_returns_success_and_closes_peer() {
    let (mut command, receipt) = peer("positive");
    let (store, _recorder, journal, path) = journal(receipt.0.parent().unwrap());
    let client = HostClient::spawn(&mut command, None, limits())
        .await
        .unwrap()
        .with_operation_journal(journal);
    let pid = client.child_id().unwrap();
    let handler = Handler::new("positive");
    let (projected, finalizer) = payload(client.execute(request(), handler.clone()).await.unwrap());
    store.fail_terminal.store(true, Ordering::SeqCst);
    let error = finalizer.settle(Ok(projected)).await.unwrap_err();
    assert!(error.message.contains("actual journal store write denied"));
    assert!(error.accepted);
    assert_reaped(pid).await;
    assert_eq!(handler.disposed.load(Ordering::SeqCst), 1);
    assert_eq!(operation_entries(&path).len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn finalizer_success_waits_for_actual_handler_dispose_once() {
    let (mut command, receipt) = peer("positive");
    let (_store, _recorder, journal, path) = journal(receipt.0.parent().unwrap());
    let mut client = HostClient::spawn(&mut command, None, limits())
        .await
        .unwrap()
        .with_operation_journal(journal);
    let pid = client.child_id().unwrap();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    client.pause_dispose_for_test(entered.clone(), release.clone());
    let handler = Handler::new("positive");
    let (projected, finalizer) = payload(client.execute(request(), handler.clone()).await.unwrap());
    let waiter = tokio::spawn(finalizer.settle(Ok(projected)));
    entered.notified().await;
    assert!(
        !waiter.is_finished(),
        "close cannot claim settled while dispose still owns its handler"
    );
    assert_reaped(pid).await;
    assert_eq!(handler.disposed.load(Ordering::SeqCst), 0);
    release.notify_one();
    waiter.await.unwrap().unwrap();
    assert_eq!(handler.disposed.load(Ordering::SeqCst), 1);
    assert_eq!(operation_entries(&path).len(), 3);
    receipt.assert_pass("positive");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn dropping_unsettled_finalizer_records_unknown_and_reaps_real_peer() {
    let (mut command, receipt) = peer("positive");
    let (_store, _recorder, journal, path) = journal(receipt.0.parent().unwrap());
    let client = HostClient::spawn(&mut command, None, limits())
        .await
        .unwrap()
        .with_operation_journal(journal);
    let pid = client.child_id().unwrap();
    let handler = Handler::new("positive");
    let (_unprojected, finalizer) =
        payload(client.execute(request(), handler.clone()).await.unwrap());
    drop(finalizer); // no terminal projection or success/settled observation
    assert_reaped(pid).await;
    wait_for_event("dropped finalizer disposer", || {
        handler.disposed.load(Ordering::SeqCst) == 1
    })
    .await;
    let entries = operation_entries(&path);
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[2].get("outcome"), Some(&"result_unknown".into()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn dropped_begin_waiter_cannot_send_execute_or_close_before_owned_write_settles() {
    let (mut command, receipt) = peer("positive");
    let (store, recorder, journal, path) = journal(receipt.0.parent().unwrap());
    let gate = StoreGate::new("agent_loop_operation_started");
    let _release_on_drop = ReleaseGate(gate.clone());
    *store.gate.lock().unwrap() = Some(gate.clone());
    let client = HostClient::spawn(&mut command, None, limits())
        .await
        .unwrap()
        .with_operation_journal(journal);
    let pid = client.child_id().unwrap();
    let handler = Handler::new("positive");
    let waiter = tokio::spawn(client.execute(request(), handler.clone()));
    gate.entered.notified().await;
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    assert_live(pid);
    assert_eq!(handler.disposed.load(Ordering::SeqCst), 0);
    gate.open();
    assert_reaped(pid).await;
    wait_for_event("dropped begin journal disposer", || {
        handler.disposed.load(Ordering::SeqCst) == 1
    })
    .await;
    let entries = operation_entries(&path);
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].get("type"),
        Some(&"agent_loop_operation_started".into())
    );
    let reopened = SessionJournal::new(recorder.writer().clone());
    let id = crate::storage::OperationIdentity {
        run_id: "run".into(),
        operation_id: "operation".into(),
        request_id: "request".into(),
        session_id: "session".into(),
        turn_id: "turn".into(),
        binding: pilotdeck_protocol::module::ModuleBinding {
            module_instance_id: "reopened".into(),
            connection_generation: "new".into(),
        },
        idempotency_key: None,
    };
    assert!(
        reopened
            .begin(id)
            .await
            .unwrap_err()
            .message
            .contains("incomplete")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn accepted_registered_before_state_cannot_be_overtaken_by_abandonment() {
    let (mut command, receipt) = peer("positive");
    let (_store, _recorder, journal, path) = journal(receipt.0.parent().unwrap());
    let pause = journal.pause_work_before_state_for_test(1);
    struct Release(Arc<crate::runtime::turn::operation_journal::WorkPause>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.open();
        }
    }
    let _release = Release(pause.clone());
    let client = HostClient::spawn(&mut command, None, limits())
        .await
        .unwrap()
        .with_operation_journal(journal.clone());
    let pid = client.child_id().unwrap();
    let handler = Handler::new("positive");
    let waiter = tokio::spawn(client.execute(request(), handler.clone()));
    pause.entered.notified().await;
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    journal.wait_for_registered_work_for_test(3).await;
    assert_live(pid);
    assert_eq!(handler.disposed.load(Ordering::SeqCst), 0);
    assert_eq!(operation_entries(&path).len(), 1);
    pause.open();
    assert_reaped(pid).await;
    wait_for_event("ordered abandoned journal disposer", || {
        handler.disposed.load(Ordering::SeqCst) == 1
    })
    .await;
    let entries = operation_entries(&path);
    assert_eq!(entries.len(), 3);
    assert_eq!(
        entries[1].get("type"),
        Some(&"agent_loop_operation_accepted".into())
    );
    assert_eq!(entries[2].get("outcome"), Some(&"result_unknown".into()));
}
