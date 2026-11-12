use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Done;
impl LiveTurn for Done {
    fn next(&mut self) -> SessionNext<'_> {
        Box::pin(async { Ok(None) })
    }
}
struct Session {
    aborted: AtomicUsize,
    during_abort: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}
impl LiveSession for Session {
    fn submit(&self, _: JsValue, _: JsValue) -> Result<Box<dyn LiveTurn>, RuntimeError> {
        Ok(Box::new(Done))
    }
    fn snapshot(&self) -> JsValue {
        JsValue::Null
    }
    fn abort(&self, _: &JsString) -> RuntimeResult {
        self.aborted.fetch_add(1, Ordering::SeqCst);
        if let Some(callback) = self.during_abort.lock().unwrap().as_ref() {
            callback();
        }
        Ok(())
    }
    fn dispose(&self) -> DisposeFuture<'_> {
        Box::pin(async { Ok(()) })
    }
}
fn session() -> Arc<Session> {
    Arc::new(Session {
        aborted: AtomicUsize::new(0),
        during_abort: Mutex::new(None),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h1_owned_abort_keeps_activity_until_callback_returns() {
    let session = session();
    let handle = LiveSessionHandle::new(session.clone(), None);
    let mut own = handle.submit(JsValue::Null, JsValue::Null).unwrap();
    handle.admit_queued();
    let probe = handle.clone();
    *session.during_abort.lock().unwrap() = Some(Box::new(move || {
        // Reentrant and another-thread queued admission both observe the
        // exact activity still owned, without an owner mutex around callback.
        assert!(probe.submit_queued(JsValue::Null, JsValue::Null).is_err());
        let other = probe.clone();
        std::thread::spawn(move || {
            assert!(other.submit_queued(JsValue::Null, JsValue::Null).is_err())
        })
        .join()
        .unwrap();
    }));
    own.abort_owned(&"stop".into()).unwrap();
    *session.during_abort.lock().unwrap() = None;
    assert_eq!(session.aborted.load(Ordering::SeqCst), 1);
    own.next().await.unwrap();
    let mut successor = handle.submit_queued(JsValue::Null, JsValue::Null).unwrap();
    own.abort_owned(&"late stop".into()).unwrap();
    assert_eq!(session.aborted.load(Ordering::SeqCst), 1);
    assert!(handle.0.progress.lock().unwrap().cancelled.is_none());
    successor.next().await.unwrap();
    handle.dispose("done".into()).await.unwrap();
}

#[tokio::test]
async fn h1_owned_old_handle_abort_never_looks_up_replacement() {
    let old_session = session();
    let replacement_session = session();
    let old_handle = LiveSessionHandle::new(old_session.clone(), None);
    let replacement = LiveSessionHandle::new(replacement_session.clone(), None);
    let mut old = old_handle.submit(JsValue::Null, JsValue::Null).unwrap();
    old_handle.stop_new();
    let mut newer = replacement.submit(JsValue::Null, JsValue::Null).unwrap();
    old.abort_owned(&"old closed".into()).unwrap();
    assert_eq!(replacement_session.aborted.load(Ordering::SeqCst), 0);
    old.next().await.unwrap();
    newer.next().await.unwrap();
    old_handle.dispose("done".into()).await.unwrap();
    replacement.dispose("done".into()).await.unwrap();
}
