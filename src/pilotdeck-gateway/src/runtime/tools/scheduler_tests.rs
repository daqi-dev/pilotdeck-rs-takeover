use super::*;
use crate::host::modules::tool_context::{HostToolCheckpoint, ToolRuntimeContext};
use std::sync::Mutex;

fn context(sink: crate::host::modules::tool_context::EventSink) -> ToolRunContext {
    ToolRunContext::new(Arc::new(ToolRuntimeContext {
        fields: JsObject::new(),
        files: HostToolCheckpoint::new(None, &[])
            .unwrap()
            .tool_context_state(),
        abort_signal: None,
        model: None,
        elicitation: None,
        user_dialog: None,
        audit_recorder: None,
        file_history: None,
        file_update_notifier: None,
        now: None,
        progress: Some(sink),
        subagent: None,
        plan_todo: None,
        goal: None,
        plan_directory: None,
    }))
}

#[test]
fn progress_identity_uses_js_falsy_fallback_and_preserves_truthy_values() {
    let events = Arc::new(Mutex::new(Vec::<JsObject>::new()));
    let captured = events.clone();
    let mut call = context(Arc::new(move |event| captured.lock().unwrap().push(event)));
    scheduler::bind_call_progress(&mut call, &"call-1".into(), &"bash".into());
    let sink = call.host.progress.as_ref().unwrap();
    for value in [
        JsValue::Undefined,
        JsValue::Null,
        JsValue::Bool(false),
        JsValue::Number(0.0),
        JsValue::Number(f64::NAN),
        JsValue::from(""),
    ] {
        let mut event = JsObject::new();
        event.insert("toolCallId".into(), value.clone());
        event.insert("toolName".into(), value);
        sink(event);
    }
    let mut truthy = JsObject::new();
    truthy.insert("toolCallId".into(), JsValue::from("explicit"));
    truthy.insert("toolName".into(), JsValue::from("other"));
    truthy.insert("message".into(), JsValue::from("actual output"));
    sink(truthy);
    let events = events.lock().unwrap();
    for event in &events[..6] {
        assert_eq!(event.get("toolCallId"), Some(&JsValue::from("call-1")));
        assert_eq!(event.get("toolName"), Some(&JsValue::from("bash")));
    }
    assert_eq!(
        events[6].get("toolCallId"),
        Some(&JsValue::from("explicit"))
    );
    assert_eq!(events[6].get("toolName"), Some(&JsValue::from("other")));
    assert_eq!(
        events[6].get("message"),
        Some(&JsValue::from("actual output"))
    );
}

#[test]
fn progress_sinks_keep_concurrent_call_identity_and_shared_base() {
    let events = Arc::new(Mutex::new(Vec::<JsObject>::new()));
    let captured = events.clone();
    let base = context(Arc::new(move |event| captured.lock().unwrap().push(event)));
    let mut a = base.clone();
    let mut b = base.clone();
    scheduler::bind_call_progress(&mut a, &"call-a".into(), &"tool-a".into());
    scheduler::bind_call_progress(&mut b, &"call-b".into(), &"tool-b".into());
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let workers: Vec<_> = [a, b]
        .into_iter()
        .map(|call| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                (call.host.progress.as_ref().unwrap())(JsObject::new());
            })
        })
        .collect();
    barrier.wait();
    for worker in workers {
        worker.join().unwrap();
    }
    (base.host.progress.as_ref().unwrap())(JsObject::new());
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 3);
    for (id, name) in [("call-a", "tool-a"), ("call-b", "tool-b")] {
        assert_eq!(
            events
                .iter()
                .filter(|event| event.get("toolCallId") == Some(&JsValue::from(id))
                    && event.get("toolName") == Some(&JsValue::from(name)))
                .count(),
            1
        );
    }
    assert!(events.iter().any(JsObject::is_empty));
}

struct Allow;
impl crate::host::modules::permission::PermissionDecision for Allow {
    fn decide(
        &self,
        _tool: Arc<dyn crate::host::modules::capability::ToolDefinition>,
        _input: JsValue,
        _context: crate::host::modules::capability::RuntimeContext,
        _id: JsString,
    ) -> crate::host::HostFuture<JsValue> {
        Box::pin(async {
            Ok(JsValue::Object(object([(
                "decision",
                JsValue::from("allow"),
            )])))
        })
    }
}

struct ProgressTool {
    fields: JsObject,
    barrier: Arc<tokio::sync::Barrier>,
}
impl crate::host::modules::capability::ToolDefinition for ProgressTool {
    fn fields(&self) -> &JsObject {
        &self.fields
    }
    fn is_concurrency_safe(&self, _: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(true))
    }
}
impl ExecutableTool for ProgressTool {
    fn execute(&self, _: JsValue, context: ToolRunContext) -> ToolFuture<ToolOutput> {
        let barrier = self.barrier.clone();
        Box::pin(async move {
            barrier.wait().await;
            (context.host.progress.as_ref().unwrap())(object([
                ("toolCallId", JsValue::from("")),
                ("toolName", JsValue::from("")),
            ]));
            Ok(ToolOutput {
                content: vec![],
                supplemental_messages: None,
                data: None,
                metadata: None,
            })
        })
    }
}

#[tokio::test]
async fn runtime_concurrent_execution_binds_each_actual_call_progress() {
    use crate::host::modules::capability::ToolCall;
    let events = Arc::new(Mutex::new(Vec::<JsObject>::new()));
    let captured = events.clone();
    let base = context(Arc::new(move |event| captured.lock().unwrap().push(event)));
    let registry = ToolRegistry::default();
    let tool = ProgressTool {
        fields: object([
            ("name", JsValue::from("probe")),
            (
                "inputSchema",
                JsValue::Object(object([("type", JsValue::from("object"))])),
            ),
        ]),
        barrier: Arc::new(tokio::sync::Barrier::new(2)),
    };
    let _registration = registry.register(Arc::new(tool)).unwrap();
    let runtime = ToolRuntime::new(registry, Arc::new(Allow), SchedulerMode::Concurrent);
    let calls = ["call-a", "call-b"]
        .into_iter()
        .map(|id| ToolCall {
            id: id.into(),
            name: "probe".into(),
            input: JsValue::Object(JsObject::new()),
        })
        .collect();
    let results = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        runtime.execute_batch(calls, base),
    )
    .await
    .expect("concurrent executions must reach the barrier")
    .unwrap();
    assert_eq!(results.len(), 2);
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 2);
    for id in ["call-a", "call-b"] {
        assert_eq!(
            events
                .iter()
                .filter(|event| event.get("toolCallId") == Some(&JsValue::from(id))
                    && event.get("toolName") == Some(&JsValue::from("probe")))
                .count(),
            1
        );
    }
}
