use super::*;
use crate::host::modules::tool_context::{ToolFileState, ToolRuntimeContext};

fn context() -> ToolRunContext {
    ToolRunContext::new(Arc::new(ToolRuntimeContext {
        fields: JsObject::new(),
        files: ToolFileState {
            read_file_state: Arc::default(),
            write_snapshots: Arc::default(),
            allowed_read_files: vec![],
        },
        abort_signal: None,
        model: None,
        elicitation: None,
        user_dialog: None,
        audit_recorder: None,
        file_history: None,
        file_update_notifier: None,
        now: None,
        progress: None,
        subagent: None,
        plan_todo: None,
        goal: None,
        plan_directory: None,
    }))
}
fn channel(coordinator: Arc<GatewayInteractionCoordinator>) -> GatewayElicitation {
    let root = std::env::temp_dir().join(format!("h1-elicitation-{}", uuid::Uuid::new_v4()));
    GatewayElicitation {
        session: "s".into(),
        coordinator,
        emit: Arc::default(),
        timeout: Duration::from_secs(30),
        recorder: Arc::new(Recorder::new(
            Arc::new(crate::storage::JsonlTranscriptStore::new()),
            crate::storage::TranscriptKey {
                project_root: root.clone(),
                pilot_home: root.join("home"),
                transcript_path: root.join("unused.jsonl"),
                session_id: "s".into(),
            },
        )),
        deterministic: None,
        hooks: None,
    }
}
fn request() -> ElicitationRequest {
    ElicitationRequest {
        tool_call_id: "tc".into(),
        tool_name: "ask".into(),
        preview_format: None,
        questions: JsValue::Array(vec![]),
        metadata: None,
    }
}

#[tokio::test]
async fn h1_dropped_native_question_future_disposes_exact_registration() {
    let coordinator = Arc::new(GatewayInteractionCoordinator::default());
    let channel = channel(coordinator.clone());
    let context = context();
    let request = request();
    let mut pending = Box::pin(channel.round_trip(&request, &context));
    std::future::poll_fn(|cx| {
        assert!(pending.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    assert_eq!(coordinator.elicitation_bus().pending_count(&"s".into()), 1);
    drop(pending);
    assert_eq!(coordinator.elicitation_bus().pending_count(&"s".into()), 0);
}

#[tokio::test]
async fn h1_question_future_drop_cannot_remove_same_key_successor() {
    let coordinator = Arc::new(GatewayInteractionCoordinator::default());
    let seen = Arc::new(Mutex::new(None));
    let channel = channel(coordinator.clone());
    let capture = seen.clone();
    assert!(
        channel
            .emit
            .set(Arc::new(move |_, event| {
                *capture.lock().unwrap() =
                    event.get("requestId").and_then(JsValue::as_string).cloned();
                true
            }))
            .is_ok()
    );
    let context = context();
    let request = request();
    let mut pending = Box::pin(channel.round_trip(&request, &context));
    std::future::poll_fn(|cx| {
        assert!(pending.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    let id = seen.lock().unwrap().clone().unwrap();
    assert!(coordinator.respond_elicitation(&"s".into(), &id, JsValue::Null, None));
    let newer = coordinator
        .elicitation_bus()
        .register(
            &"s".into(),
            PendingEntry {
                request_id: id,
                tool_call_id: "new".into(),
                tool_name: "new".into(),
                resolve: Arc::new(|_| {}),
                reject: Arc::new(|_| {}),
            },
            None,
        )
        .unwrap();
    drop(pending);
    assert!(newer.active());
    assert_eq!(coordinator.elicitation_bus().pending_count(&"s".into()), 1);
    newer.dispose();
}
