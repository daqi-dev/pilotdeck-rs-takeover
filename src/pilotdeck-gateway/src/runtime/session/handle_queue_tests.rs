//! Follow-up turns admitted to the session inbox (`AgentHandle.followup` /
//! `runQueuedPump`): while one is pending an ordinary submit is refused
//! (AgentHandle.ts:74-77), and the pump's own submit takes the slot.
use super::*;
use std::time::Duration;

struct Idle;
struct Done;
impl LiveTurn for Done {
    fn next(&mut self) -> SessionNext<'_> {
        Box::pin(async { Ok(None) })
    }
}
impl LiveSession for Idle {
    fn submit(&self, _: JsValue, _: JsValue) -> Result<Box<dyn LiveTurn>, RuntimeError> {
        Ok(Box::new(Done))
    }
    fn snapshot(&self) -> JsValue {
        JsValue::Null
    }
    fn abort(&self, _: &JsString) -> RuntimeResult {
        Ok(())
    }
    fn dispose(&self) -> DisposeFuture<'_> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn a_pending_follow_up_refuses_ordinary_submits_until_it_starts() {
    let handle = LiveSessionHandle::new(Arc::new(Idle), None);
    handle.admit_queued();
    let refused = handle
        .submit(JsValue::Null, JsValue::Null)
        .err()
        .expect("a pending follow-up refuses an ordinary submit");
    assert_eq!(refused.message, "Agent handle already has an active turn.");
    let refused = handle.begin_maintenance().err().expect("and maintenance");
    assert_eq!(refused.message, "Agent handle is not idle for maintenance.");

    let mut queued = handle
        .submit_queued(JsValue::Null, JsValue::Null)
        .expect("the pump's submit takes the slot");
    // Busy while the follow-up turn runs.
    assert!(handle.submit(JsValue::Null, JsValue::Null).is_err());
    let waiter = tokio::spawn({
        let handle = handle.clone();
        async move { handle.wait_idle().await }
    });
    assert!(queued.next().await.expect("turn").is_none());
    assert!(
        tokio::time::timeout(Duration::from_secs(3), waiter)
            .await
            .expect("idle once the turn ended")
            .expect("waiter")
    );
    assert!(handle.submit(JsValue::Null, JsValue::Null).is_ok());
}

#[tokio::test]
async fn wait_idle_reports_a_handle_that_stopped_admitting() {
    let handle = LiveSessionHandle::new(Arc::new(Idle), None);
    assert!(handle.wait_idle().await);
    handle.stop_new();
    assert!(!handle.wait_idle().await);
}
