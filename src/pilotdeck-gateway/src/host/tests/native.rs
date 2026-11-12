//! Native host lifetime checks: real ExecutionServer with a source-level Rust
//! factory, bounded memory transport, and no child executable.
use super::*;
use pilotdeck_protocol::{admission::CallEffect, module::ModuleOutcome};
use pilotdeck_transport::server::{ExecutionContext, ExecutionFactory, ExecutionTerminal};
use std::path::Path;

struct Factory {
    calls: Arc<AtomicUsize>,
    entered: Arc<Notify>,
    wait_for_cancel: bool,
}
impl ExecutionFactory for Factory {
    fn execute(
        &self,
        _request: &pilotdeck_transport::server::ExecuteRequest,
        context: &mut ExecutionContext,
    ) -> pilotdeck_agent::ports::PortResult<ExecutionTerminal> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let identity = context.identity("session".into(), "turn".into());
        context.call(
            identity,
            "model",
            "get_metadata",
            CallEffect::ReadOnly,
            JsObject::new(),
        )?;
        self.entered.notify_one();
        let (outcome, kind, reason) = if self.wait_for_cancel {
            let cause = context
                .control
                .cancellation
                .wait_until(std::time::Instant::now() + Duration::from_secs(10));
            assert!(matches!(
                cause,
                Some(pilotdeck_agent::ports::CancelCause::Requested(_))
            ));
            (ModuleOutcome::Cancelled, "aborted", "aborted_streaming")
        } else {
            (ModuleOutcome::Completed, "success", "completed")
        };
        let result = parse_json(&format!(r#"{{"type":"{kind}","sessionId":"session","turnId":"turn","stopReason":"{reason}","turns":1}}"#)).unwrap();
        let mut payload = JsObject::new();
        payload.insert("result".into(), result);
        payload.insert("messages".into(), JsValue::Array(vec![]));
        Ok(ExecutionTerminal {
            outcome,
            payload,
            error: None,
        })
    }
}

#[tokio::test]
async fn native_handshake_needs_no_executable_and_releases_workers() {
    // An intentionally nonexistent path is only loop cwd metadata, never an
    // executable lookup or process-wide current_dir mutation.
    let client = HostClient::native(
        Path::new("/native-loop/no-executable"),
        Some(&"pilotdeck-agent-loop".into()),
        Limits::default(),
    )
    .await
    .unwrap();
    assert_eq!(client.child_id(), None);
    client.close().await.unwrap();
}

#[tokio::test]
async fn native_factory_executes_host_callback_once_and_releases() {
    let calls = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(Factory {
        calls: calls.clone(),
        entered: Arc::new(Notify::new()),
        wait_for_cancel: false,
    });
    let released = Arc::downgrade(&factory);
    let client =
        HostClient::native_with_factory(Path::new("/project"), None, Limits::default(), factory)
            .await
            .unwrap();
    assert_eq!(client.child_id(), None);
    let handler = Handler::new("native");
    let result = client.execute(request(), handler.clone()).await.unwrap();
    assert!(matches!(result, ExecuteResult::Terminal(_)));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
    assert_eq!(handler.disposed.load(Ordering::SeqCst), 1);
    assert!(
        released.upgrade().is_none(),
        "native server retained its factory after close"
    );
}

#[tokio::test]
async fn native_user_cancel_reaches_same_execution_and_drains_workers() {
    let calls = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(Notify::new());
    let factory = Arc::new(Factory {
        calls: calls.clone(),
        entered: entered.clone(),
        wait_for_cancel: true,
    });
    let released = Arc::downgrade(&factory);
    let client =
        HostClient::native_with_factory(Path::new("/project"), None, Limits::default(), factory)
            .await
            .unwrap();
    let cancel = Cancellation::default();
    let execution =
        tokio::spawn(client.execute_cancellable(request(), Handler::new("native"), cancel.clone()));
    timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    cancel.cancel(Some("user_stop".into()));
    let result = timeout(Duration::from_secs(5), execution)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let ExecuteResult::Terminal(terminal) = result else {
        panic!("authoritative cancellation terminal")
    };
    assert_eq!(terminal.get("outcome"), Some(&JsValue::from("cancelled")));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(released.upgrade().is_none());
}
