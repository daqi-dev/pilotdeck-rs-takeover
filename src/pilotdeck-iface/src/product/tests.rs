use super::*;
use agentcpm_iface::BoxFuture;
use p::Controller;
use std::{collections::BTreeMap, sync::Mutex};
#[derive(Default)]
struct Probe {
    views: Mutex<BTreeMap<String, p::CallView>>,
    invocations: Mutex<Vec<p::Invocation>>,
    publications: Mutex<Vec<Value>>,
}
fn unavailable<T>() -> BoxFuture<'static, p::Result<T>> {
    Box::pin(async {
        Err(p::Failure::external(
            "TEST_UNAVAILABLE",
            "unexpected test call",
        ))
    })
}
impl Client for Probe {
    fn resource(&self, _: &str, _: Value) -> BoxFuture<'_, p::Result<Value>> {
        unavailable()
    }
    fn publish(
        &self,
        _: &str,
        _: &str,
        payload: Value,
        _: p::State,
        _: Option<p::Failure>,
        _: Option<p::Wait>,
    ) -> BoxFuture<'_, p::Result<()>> {
        self.publications.lock().unwrap().push(payload);
        Box::pin(async { Ok(()) })
    }
    fn deliver(&self, _: &str, _: p::Destination, _: p::Event) -> BoxFuture<'_, p::Result<()>> {
        unavailable()
    }
    fn invoke(&self, request: p::Invocation) -> BoxFuture<'_, p::Result<String>> {
        self.invocations.lock().unwrap().push(request);
        Box::pin(async { Ok("child".into()) })
    }
    fn find(&self, _: &str, _: &str) -> BoxFuture<'_, p::Result<Option<p::CallView>>> {
        unavailable()
    }
    fn children(&self, _: &str) -> BoxFuture<'_, p::Result<Vec<p::CallView>>> {
        unavailable()
    }
    fn get(&self, call: &str) -> BoxFuture<'_, p::Result<p::CallView>> {
        let found = self.views.lock().unwrap().get(call).cloned();
        Box::pin(async move {
            found.ok_or_else(|| p::Failure::external("TEST_MISSING", "missing view"))
        })
    }
    fn wait(&self, call: &str) -> BoxFuture<'_, p::Result<p::CallView>> {
        self.get(call)
    }
    fn control(
        &self,
        _: &str,
        _: p::ControlKind,
        _: String,
    ) -> BoxFuture<'_, p::Result<p::ControlOutcome>> {
        unavailable()
    }
    fn respond(
        &self,
        _: &str,
        _: &str,
        _: Vec<p::Message>,
        _: &str,
    ) -> BoxFuture<'_, p::Result<()>> {
        unavailable()
    }
    fn read_state(&self, _: &str) -> BoxFuture<'_, p::Result<p::VersionedState>> {
        unavailable()
    }
    fn write_state(&self, _: p::StateChange) -> BoxFuture<'_, p::Result<u64>> {
        unavailable()
    }
    fn events(&self, _: u64) -> BoxFuture<'_, p::Result<Vec<p::Event>>> {
        unavailable()
    }
    fn wait_events(&self, _: u64) -> BoxFuture<'_, p::Result<Vec<p::Event>>> {
        unavailable()
    }
}
fn context(payload: Value) -> CallContext {
    CallContext {
        call_ref: "entry".into(),
        session_ref: "kernel-session".into(),
        binding: p::Binding {
            id: "controller".into(),
            revision: "candidate".into(),
            kind: p::Kind::Service,
            effect: p::Effect::State,
            permissions: p::Permissions::calls(["loop".into()]),
        },
        invocation: p::Invocation::new("controller", "input", "entry-key", "correlation", payload),
        state: Default::default(),
        attempt: 1,
        origin: Default::default(),
        interaction: Default::default(),
        security: Default::default(),
    }
}
fn view(context: &CallContext) -> p::CallView {
    p::CallView {
        id: context.call_ref.clone(),
        session_ref: context.session_ref.clone(),
        source_binding: None,
        parent_session_ref: None,
        cause_execution_ref: None,
        binding: context.binding.clone(),
        invocation: context.invocation.clone(),
        state: p::State::Running,
        result: None,
        presentation: None,
        settlement_data: None,
        settlement_error: None,
        error: None,
        waiting: None,
        cursor: 1,
        origin: context.origin.clone(),
        interaction: context.interaction.clone(),
        interactions: vec![],
        cancel_requested: false,
        stepped: false,
        security: Default::default(),
    }
}
fn controller() -> ProductController {
    ProductController {
        loop_binding: "loop".into(),
        controller_binding: "controller".into(),
    }
}
fn input_event(raw: &str) -> Value {
    json!({"id":"e1","session_ref":"kernel-session","call_ref":null,"correlation_id":"correlation","kind":"input","state":null,"payload":raw,"cursor":1})
}
#[tokio::test]
async fn input_preserves_full_payload_limits_and_authorized_component_return() {
    let raw = r#"{"payload":{"messages":[{"text":"\ud800"}]}}"#;
    let mut ctx = context(input_event(raw));
    ctx.invocation.limits.deadline_ms = Some(12345);
    ctx.invocation.limits.allowed_tools = Some(["read_file".into()].into());
    let probe = Arc::new(Probe::default());
    probe
        .views
        .lock()
        .unwrap()
        .insert(ctx.call_ref.clone(), view(&ctx));
    controller()
        .handle(ctx.clone(), probe.clone())
        .await
        .unwrap();
    let calls = probe.invocations.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].payload, Value::String(raw.into()));
    assert_eq!(calls[0].limits, ctx.invocation.limits);
    assert_eq!(
        calls[0].return_to,
        Some(p::Destination::Component {
            binding_ref: "controller".into(),
            context_ref: RESULT_CONTEXT.into()
        })
    );
}
#[tokio::test]
async fn changed_current_binding_cannot_dispatch_input() {
    let ctx = context(input_event("{}"));
    let mut wrong = view(&ctx);
    wrong.binding.id = "other".into();
    let probe = Arc::new(Probe::default());
    probe
        .views
        .lock()
        .unwrap()
        .insert(ctx.call_ref.clone(), wrong);
    assert!(controller().handle(ctx, probe.clone()).await.is_err());
    assert!(probe.invocations.lock().unwrap().is_empty());
}
#[tokio::test]
async fn event_without_authoritative_loop_parent_is_never_published() {
    let mut ctx = context(Value::String("{}".into()));
    ctx.invocation.context_ref = EVENT_CONTEXT.into();
    let probe = Arc::new(Probe::default());
    probe
        .views
        .lock()
        .unwrap()
        .insert(ctx.call_ref.clone(), view(&ctx));
    assert!(controller().handle(ctx, probe.clone()).await.is_err());
    assert!(probe.publications.lock().unwrap().is_empty());
}
#[tokio::test]
async fn confirmed_publication_failure_differs_from_observation_failure_and_unknown() {
    let ctx = context(Value::Null);
    let probe = Probe::default();
    let mut failed = view(&ctx);
    failed.state = p::State::Failed;
    failed.error = Some(p::Failure::external("PUBLISH_DENIED", "denied"));
    probe
        .views
        .lock()
        .unwrap()
        .insert(ctx.call_ref.clone(), failed.clone());
    let pending = PendingProductEvent { call: ctx.call_ref };
    assert!(
        matches!(pending.settlement(&probe).await.unwrap(),ProductEventSettlement::Failed(Some(error)) if error.code=="PUBLISH_DENIED")
    );
    failed.state = p::State::Unknown;
    probe
        .views
        .lock()
        .unwrap()
        .insert(pending.call_ref().into(), failed);
    assert!(matches!(
        pending.settlement(&probe).await.unwrap(),
        ProductEventSettlement::Unknown
    ));
    probe.views.lock().unwrap().clear();
    assert!(pending.settlement(&probe).await.is_err());
    assert_eq!(
        pending.call_ref(),
        "entry",
        "failed observation must retain publication identity"
    );
}

#[tokio::test]
async fn product_event_keys_are_scoped_per_actual_loop_and_stable_on_retry() {
    let first = context(Value::Null);
    let mut second = first.clone();
    second.call_ref = "next-loop".into();
    second.interaction.turn += 1;
    let probe = Probe::default();
    assert!(
        admit_event(&probe, &first, "controller", "event-1", "{}".into())
            .await
            .is_err()
    );
    assert!(probe.invocations.lock().unwrap().is_empty());
    for (ctx, key) in [
        (&first, "entry:event:1"),
        (&first, "entry:event:1"),
        (&second, "next-loop:event:1"),
    ] {
        admit_event(&probe, ctx, "controller", key, "{}".into())
            .await
            .unwrap();
    }
    let calls = probe.invocations.lock().unwrap();
    assert_eq!(calls[0], calls[1]);
    assert_ne!(calls[0].idempotency_key, calls[2].idempotency_key);
    assert_eq!(calls[0].parent_execution_ref.as_deref(), Some("entry"));
    assert_eq!(calls[2].parent_execution_ref.as_deref(), Some("next-loop"));
}
