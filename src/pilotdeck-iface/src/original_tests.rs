use super::*;
use crate::original::{OriginalBindings, OriginalPayloadAdmission};
use pilotdeck_agent::ports::*;
use pilotdeck_protocol::{JsString, JsValue, model::BoundarySnapshot};
use pilotdeck_turn_preparation::PreparationEffects;

type Order = Arc<Mutex<Vec<&'static str>>>;

struct InitialReads(Order);
impl HostPort for InitialReads {
    fn advertised(&self, module: &str, operation: &str) -> bool {
        matches!(
            (module, operation),
            ("capability", "plan_todo") | ("model", "get_metadata")
        )
    }
    fn call(&mut self, control: &CallControl, call: HostCall) -> PortResult<HostReply> {
        control.check()?;
        match call {
            HostCall::PlanTodo(request) => {
                assert!(matches!(request.mutation, PlanTodoMutation::Read));
                self.0.lock().unwrap().push("plan");
                Ok(HostReply::PlanTodo(Captured {
                    view: PlanTodoSnapshot {
                        approved_plan: None,
                        requires_initialization: false,
                        tool_calls_since_last_todo_write: 0.0,
                        last_markdown: None,
                        todos: vec![],
                        active_todos: vec![],
                        todo_history: vec![],
                        todo_diagnostics: Captured {
                            view: TodoDiagnostics {
                                write_count: 0.0,
                                todo_count: 0.0,
                                active_count: 0.0,
                                completed_count: 0.0,
                                cancelled_count: 0.0,
                                large_rewrite_count: 0.0,
                                deleted_open_item_count: 0.0,
                                completed_without_active_count: 0.0,
                                last_write: None,
                            },
                            boundary: BoundarySnapshot::capture(&JsValue::Null),
                        },
                    },
                    boundary: BoundarySnapshot::capture(&JsValue::Null),
                }))
            }
            HostCall::ModelMetadata { route } => {
                self.0.lock().unwrap().push("metadata");
                Ok(HostReply::ModelMetadata(Captured {
                    view: ModelMetadata {
                        route,
                        max_context_tokens: Some(8192.0),
                        max_output_tokens: Some(2048.0),
                        token_limits: None,
                        protocol: None,
                        supports_prompt_cache: None,
                    },
                    boundary: BoundarySnapshot::capture(&JsValue::Null),
                }))
            }
            _ => panic!("unexpected initialization operation"),
        }
    }
}

struct TestEffects;
impl PreparationEffects for TestEffects {
    fn notice(&mut self, _: &turn::TurnInput, _: bool) -> PortResult<()> {
        Ok(())
    }
    fn before_model_request(&mut self, _: &Route) -> PortResult<()> {
        Ok(())
    }
    fn finish(&mut self) -> PortResult<()> {
        Ok(())
    }
}
struct TestEvents;
impl TurnEventPort for TestEvents {
    fn emit(&mut self, _: BoundarySnapshot) -> PortResult<()> {
        Ok(())
    }
}
struct TestClock;
impl TurnClock for TestClock {
    fn now_iso(&self) -> JsString {
        "test-time".into()
    }
    fn uuid(&self) -> JsString {
        "test-id".into()
    }
}
struct Bindings {
    order: Order,
    expected_client: Arc<dyn Client>,
}
impl OriginalBindings for Bindings {
    fn resources(
        &self,
        _: &scheduler::CallContext,
        _: &TurnInput,
    ) -> PortResult<Box<dyn ResourceCodec>> {
        panic!("direct preparation uses the supplied test host")
    }
    fn effects(
        &self,
        _: &scheduler::CallContext,
        _: &turn::TurnInput,
        client: Arc<dyn Client>,
        _: &CallControl,
    ) -> PortResult<Arc<Mutex<dyn PreparationEffects>>> {
        assert!(Arc::ptr_eq(&client, &self.expected_client));
        self.order.lock().unwrap().push("effects");
        Ok(Arc::new(Mutex::new(TestEffects)))
    }
    fn events(
        &self,
        _: &scheduler::CallContext,
        _: &turn::TurnInput,
        client: Arc<dyn Client>,
        _: &CallControl,
    ) -> PortResult<Box<dyn TurnEventPort + Send>> {
        assert!(Arc::ptr_eq(&client, &self.expected_client));
        self.order.lock().unwrap().push("events");
        Ok(Box::new(TestEvents))
    }
    fn clock(&self, _: &scheduler::CallContext) -> PortResult<Box<dyn TurnClock + Send>> {
        self.order.lock().unwrap().push("clock");
        Ok(Box::new(TestClock))
    }
}

fn control() -> CallControl {
    CallControl {
        cancellation: Cancellation::default(),
        deadline: None,
    }
}

fn payload(seed: &str) -> String {
    format!(
        r#"{{"operationId":"product-op","sessionId":"product-session","turnId":"product-turn","payload":{{"provider":"provider","model":"model","messages":[{{"role":"user","content":[{{"type":"text","text":"A\ud800B"}},{{"type":"image","source":"base64","mimeType":"image/png","data":"AA=="}}],"metadata":{{"compactSnapshotId":"snapshot-7","transient":true}}}}],"seedState":{seed},"modelState":{{"tokenCaps":[{{"provider":"provider","model":"model","sessionMaxOutputTokens":512}}]}}}}}}"#
    )
}

#[test]
fn full_original_admission_keeps_rich_boundaries_and_initialization_order() {
    let order = Arc::new(Mutex::new(vec![]));
    let client: Arc<dyn Client> = tests::client(None);
    let admission = OriginalPayloadAdmission::new(
        "/isolated/workspace".into(),
        Arc::new(Bindings {
            order: order.clone(),
            expected_client: client.clone(),
        }),
    );
    let mut context = tests::context();
    context.invocation.payload =
        Value::String(payload(r#"{"allowedReadFiles":["/fixture","/fixture"]}"#));
    let admitted = admission
        .prepare(
            &context,
            &tests::input(),
            &mut InitialReads(order.clone()),
            client,
            &control(),
        )
        .unwrap();
    assert_eq!(
        *order.lock().unwrap(),
        ["plan", "metadata", "effects", "events", "clock"]
    );
    assert_eq!(admitted.input.session_id, JsString::from("product-session"));
    assert_eq!(admitted.config.cwd, JsString::from("/isolated/workspace"));
    assert_eq!(admitted.initial.len(), 1);
    assert_eq!(
        admitted.file_state.allowed_read_files,
        [JsString::from("/fixture")]
    );
    assert_eq!(
        admitted.preparation.model_state().token_caps[0].session_max_output_tokens,
        Some(512.0)
    );
    let boundary = admitted.initial[0].boundary.value();
    assert_eq!(
        boundary.get("metadata").unwrap().get("compactSnapshotId"),
        Some(&JsValue::String("snapshot-7".into()))
    );
    let JsValue::Array(content) = boundary.get("content").unwrap() else {
        panic!("rich array lost")
    };
    assert_eq!(content.len(), 2);
    assert_eq!(
        content[0].get("text").unwrap().as_string().unwrap().units(),
        [0x41, 0xd800, 0x42]
    );
    assert_eq!(
        content[1].get("type").unwrap(),
        &JsValue::String("image".into())
    );
    assert_eq!(
        admitted.tool_hints.execution.run_id,
        JsString::from("run-1")
    );
    assert_eq!(
        admitted.tool_hints.execution.operation_id,
        JsString::from("step-1")
    );
}

#[test]
fn invalid_restored_state_fails_after_reads_before_observers() {
    let order = Arc::new(Mutex::new(vec![]));
    let client: Arc<dyn Client> = tests::client(None);
    let admission = OriginalPayloadAdmission::new(
        "/isolated".into(),
        Arc::new(Bindings {
            order: order.clone(),
            expected_client: client.clone(),
        }),
    );
    let mut context = tests::context();
    context.invocation.payload = Value::String(payload(r#"{"allowedReadFiles":42}"#));
    let result = admission.prepare(
        &context,
        &tests::input(),
        &mut InitialReads(order.clone()),
        client,
        &control(),
    );
    assert!(
        matches!(result, Err(PortError::InvalidResponse { message }) if message.equals_str("Invalid sidecar seedState.allowedReadFiles."))
    );
    assert_eq!(*order.lock().unwrap(), ["plan", "metadata"]);
}

#[test]
fn cancelled_original_admission_performs_no_reads_or_observer_creation() {
    let order = Arc::new(Mutex::new(vec![]));
    let client: Arc<dyn Client> = tests::client(None);
    let admission = OriginalPayloadAdmission::new(
        "/isolated".into(),
        Arc::new(Bindings {
            order: order.clone(),
            expected_client: client.clone(),
        }),
    );
    let control = control();
    control.cancellation.cancel(CancelCause::OwnerRetired);
    // Even the deliberately invalid boundary must not be consumed after cancel.
    let result = admission.prepare(
        &tests::context(),
        &tests::input(),
        &mut InitialReads(order.clone()),
        client,
        &control,
    );
    assert!(matches!(
        result,
        Err(PortError::Cancelled {
            cause: CancelCause::OwnerRetired
        })
    ));
    assert!(order.lock().unwrap().is_empty());
}
