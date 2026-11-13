use super::*;
use crate::host::modules::tool_context::{ToolFileState, ToolRuntimeContext};
use crate::runtime::interaction::PermissionDecision;

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

fn prompt(
    coordinator: Arc<GatewayInteractionCoordinator>,
    seen: Arc<Mutex<Option<JsString>>>,
) -> PermissionPrompt {
    let prompt = PermissionPrompt {
        session: "s".into(),
        coordinator,
        emit: Arc::default(),
        rules: RuleSetHandle::default(),
        timeout: Duration::from_secs(30),
        interaction: SessionInteraction::default(),
    };
    assert!(
        prompt
            .emit
            .set(Arc::new(move |_, event| {
                if event.get("type") == Some(&JsValue::from("permission_request")) {
                    *seen.lock().unwrap() =
                        event.get("requestId").and_then(JsValue::as_string).cloned();
                }
                true
            }))
            .is_ok()
    );
    prompt
}

#[tokio::test]
async fn h1_dropped_native_permission_request_disposes_exact_registration() {
    let coordinator = Arc::new(GatewayInteractionCoordinator::default());
    let prompt = prompt(coordinator.clone(), Arc::default());
    let context = context();
    let (tool, call, input) = ("write_file".into(), "tc".into(), JsValue::Null);
    // Exercise the production request -> ask path, not the RAII helper alone.
    let mut pending = Box::pin(prompt.request(&tool, &call, &input, None, &context));
    std::future::poll_fn(|cx| {
        assert!(pending.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    assert_eq!(coordinator.permission_bus().pending_count(&"s".into()), 1);
    drop(pending);
    assert_eq!(coordinator.permission_bus().pending_count(&"s".into()), 0);
}

#[tokio::test]
async fn h1_permission_request_drop_cannot_remove_same_key_successor() {
    let coordinator = Arc::new(GatewayInteractionCoordinator::default());
    let seen = Arc::new(Mutex::new(None));
    let prompt = prompt(coordinator.clone(), seen.clone());
    let context = context();
    let (tool, call, input) = ("write_file".into(), "tc".into(), JsValue::Null);
    let mut pending = Box::pin(prompt.request(&tool, &call, &input, None, &context));
    std::future::poll_fn(|cx| {
        assert!(pending.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    let id = seen.lock().unwrap().clone().unwrap();
    assert!(coordinator.decide_permission(
        &"s".into(),
        PermissionDecision {
            request_id: id.clone(),
            decision: "allow".into(),
            remember: Some(JsValue::Bool(false)),
            reason: None,
        },
        None,
    ));
    // The old future has not resumed. Reuse its public keys with a distinct
    // bus registration before dropping it to expose any keyed cleanup.
    let newer = coordinator
        .permission_bus()
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
    assert_eq!(coordinator.permission_bus().pending_count(&"s".into()), 1);
    newer.dispose();
}

#[tokio::test]
async fn callback_status_is_session_scoped_while_child_permission_uses_parent_answerer() {
    for actual_session in [Some("s"), Some("s::sub::child"), None] {
        let coordinator = Arc::new(GatewayInteractionCoordinator::default());
        let emitted = Arc::new(Mutex::new(Vec::<(JsString, JsValue)>::new()));
        let prompt = PermissionPrompt {
            session: "s".into(),
            coordinator: coordinator.clone(),
            emit: Arc::default(),
            rules: RuleSetHandle::default(),
            timeout: Duration::from_secs(30),
            interaction: SessionInteraction::default(),
        };
        let seen = emitted.clone();
        assert!(
            prompt
                .emit
                .set(Arc::new(move |session, event| {
                    seen.lock().unwrap().push((session.clone(), event));
                    true
                }))
                .is_ok()
        );
        let mut run = context();
        if let Some(session) = actual_session {
            run.fields.insert("sessionId".into(), session.into());
        }
        let (tool, call, input) = ("write_file".into(), "child-tool".into(), JsValue::Null);
        let mut pending = Box::pin(prompt.request(&tool, &call, &input, None, &run));
        std::future::poll_fn(|cx| {
            assert!(pending.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert_eq!(coordinator.permission_bus().pending_count(&"s".into()), 1);
        let request_id = {
            let rows = emitted.lock().unwrap();
            assert!(rows.iter().all(|(session, _)| session.equals_str("s")));
            assert_eq!(rows.len(), if actual_session == Some("s") { 2 } else { 1 });
            rows.iter()
                .find(|(_, event)| event.get("type") == Some(&"permission_request".into()))
                .unwrap()
                .1
                .get("requestId")
                .unwrap()
                .as_string()
                .unwrap()
                .clone()
        };
        let decision = || PermissionDecision {
            request_id: request_id.clone(),
            decision: "allow".into(),
            remember: Some(JsValue::Bool(false)),
            reason: None,
        };
        assert!(!coordinator.decide_permission(&"s::sub::child".into(), decision(), None));
        assert!(coordinator.decide_permission(&"s".into(), decision(), None));
        assert!(matches!(pending.await, PromptOutcome::Allow));
        assert_eq!(coordinator.permission_bus().pending_count(&"s".into()), 0);
        let rows = emitted.lock().unwrap();
        let states: Vec<_> = rows
            .iter()
            .filter_map(|(_, event)| event.get("event").and_then(JsValue::as_string))
            .map(|s| String::from_utf16_lossy(s.units()))
            .collect();
        assert_eq!(
            states,
            if actual_session == Some("s") {
                vec!["hook_execution_started", "hook_execution_completed"]
            } else {
                Vec::new()
            }
        );
    }
}
