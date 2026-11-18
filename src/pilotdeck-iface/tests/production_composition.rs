#![forbid(unsafe_code)]

use agentcpm_iface::BoxFuture;
use agentcpm_scheduler_protocol::{
    self as p, Binding, CallContext, CallView, Client, ControlKind, ControlOutcome, Destination,
    Effect, Event, Failure, InputOrigin, Invocation, Kind, Message, Permissions, Result as PResult,
    State, VersionedState, Wait,
    context_history::{
        self as h, GatewayStartReceipt, GatewayTranscriptEntry, GatewayTranscriptPage,
        GatewayTranscriptReceipt, OpaquePayload,
    },
    context_operations::Operation,
    product_reads,
    step_scope::StepRef,
    tool_batch,
};
use pilotdeck_agent::ports::{
    ExecutionHints, ExecutionIdentity, HostCall, HostReply, PermissionMode, PlanTodoMutation,
    PlanTodoRequest, PortError, Route, ToolHints, ToolInvocation,
};
use pilotdeck_iface::{
    context_codec::InstalledContextOperation,
    initial_read_codec::InitialReadCodec,
    model_session_codec::ModelSessionCodec,
    native_loop_launch::{self, NativeLoopLaunch},
    production_bindings::{NoticeServices, ProductionAdapters, ProductionBindings},
    resource_mux::ResourceMux,
    resources::{ResourceCodec, ResourceStep},
    tool_batch_codec::ToolBatchCodec,
    tool_batch_owner::SourceToolOwners,
    transcript_snapshot::GatewayTranscriptSnapshot,
};
use pilotdeck_protocol::{JsObject, JsString, JsValue, boundary_codec, parse_json};
use serde_json::Value;
use std::sync::Arc;

struct StubClient {
    features: Vec<String>,
    scope: Option<StepRef>,
}

fn unavailable<T>() -> PResult<T> {
    Err(Failure::external("TEST_UNAVAILABLE", "unused test method"))
}

impl Client for StubClient {
    fn step_scope(&self) -> Option<StepRef> {
        self.scope.clone()
    }
    fn host_features(&self) -> Vec<String> {
        self.features.clone()
    }
    fn resource(&self, _method: &str, _payload: Value) -> BoxFuture<'_, PResult<Value>> {
        Box::pin(async { unavailable() })
    }
    fn publish(
        &self,
        _: &str,
        _: &str,
        _: Value,
        _: State,
        _: Option<Failure>,
        _: Option<Wait>,
    ) -> BoxFuture<'_, PResult<()>> {
        Box::pin(async { unavailable() })
    }
    fn deliver(&self, _: &str, _: Destination, _: Event) -> BoxFuture<'_, PResult<()>> {
        Box::pin(async { unavailable() })
    }
    fn invoke(&self, _: Invocation) -> BoxFuture<'_, PResult<String>> {
        Box::pin(async { unavailable() })
    }
    fn find(&self, _: &str, _: &str) -> BoxFuture<'_, PResult<Option<CallView>>> {
        Box::pin(async { unavailable() })
    }
    fn children(&self, _: &str) -> BoxFuture<'_, PResult<Vec<CallView>>> {
        Box::pin(async { unavailable() })
    }
    fn get(&self, _: &str) -> BoxFuture<'_, PResult<CallView>> {
        Box::pin(async { unavailable() })
    }
    fn wait(&self, _: &str) -> BoxFuture<'_, PResult<CallView>> {
        Box::pin(async { unavailable() })
    }
    fn control(
        &self,
        _: &str,
        _: ControlKind,
        _: String,
    ) -> BoxFuture<'_, PResult<ControlOutcome>> {
        Box::pin(async { unavailable() })
    }
    fn respond(&self, _: &str, _: &str, _: Vec<Message>, _: &str) -> BoxFuture<'_, PResult<()>> {
        Box::pin(async { unavailable() })
    }
    fn read_state(&self, _: &str) -> BoxFuture<'_, PResult<VersionedState>> {
        Box::pin(async { unavailable() })
    }
    fn write_state(&self, _: p::StateChange) -> BoxFuture<'_, PResult<u64>> {
        Box::pin(async { unavailable() })
    }
    fn events(&self, _: u64) -> BoxFuture<'_, PResult<Vec<Event>>> {
        Box::pin(async { unavailable() })
    }
    fn wait_events(&self, _: u64) -> BoxFuture<'_, PResult<Vec<Event>>> {
        Box::pin(async { unavailable() })
    }
}

fn context() -> CallContext {
    CallContext {
        call_ref: "run-prod-1".into(),
        session_ref: "session-prod-1".into(),
        binding: Binding {
            id: "pilotdeck".into(),
            revision: "rev-prod-1".into(),
            kind: Kind::Loop,
            effect: Effect::ToolWrite,
            permissions: Permissions::calls([]),
        },
        invocation: Invocation::new(
            "pilotdeck",
            "pilotdeck.turn",
            "invoke-prod-1",
            "corr-prod-1",
            Value::Null,
        ),
        state: VersionedState::default(),
        attempt: 1,
        origin: InputOrigin {
            principal: "gateway".into(),
            turn: 1,
            generation: 1,
            input_ref: "input-1".into(),
        },
        interaction: InputOrigin {
            principal: "gateway".into(),
            turn: 1,
            generation: 1,
            input_ref: "input-1".into(),
        },
        security: p::SecurityRefs::default(),
    }
}

fn complete_context_roles() -> Vec<InstalledContextOperation> {
    [
        Operation::PrepareForModel,
        Operation::ApplyToolResults,
        Operation::RecoverFromModelError,
        Operation::CaptureTurn,
        Operation::TryAutoCompact,
    ]
    .into_iter()
    .enumerate()
    .map(|(i, operation)| InstalledContextOperation {
        operation,
        binding: format!("ctx-binding-{i}"),
        revision: format!("ctx-rev-{i}"),
    })
    .collect()
}

struct DummyAdapters;
impl ProductionAdapters for DummyAdapters {
    fn validate_startup(&self, _: &str, _: &str) -> pilotdeck_agent::ports::PortResult<()> {
        Ok(())
    }
    fn validate_call(
        &self,
        _: &CallContext,
        _: &agentcpm_iface::types::TurnInput,
        _: &dyn Client,
    ) -> pilotdeck_agent::ports::PortResult<()> {
        Ok(())
    }
    fn retain_observers(
        &self,
        _: &CallContext,
        _: Arc<pilotdeck_iface::context_codec::ContextProofs>,
        _: Arc<pilotdeck_iface::product_event_queue::EventSettlementLedger>,
        _: Arc<pilotdeck_iface::production_events::EventPublicationLedger>,
    ) -> pilotdeck_agent::ports::PortResult<()> {
        Ok(())
    }
    fn tool_executor(
        &self,
        _: &CallContext,
        _: &Arc<dyn Client>,
    ) -> pilotdeck_agent::ports::PortResult<tokio::runtime::Handle> {
        Err(PortError::InvalidResponse {
            message: "unused".into(),
        })
    }
    fn source_tool_owners(
        &self,
        _: &CallContext,
        _: &Arc<dyn Client>,
    ) -> pilotdeck_agent::ports::PortResult<Arc<SourceToolOwners>> {
        Err(PortError::InvalidResponse {
            message: "unused".into(),
        })
    }
    fn resource_codecs(
        &self,
        _: &CallContext,
        _: &agentcpm_iface::types::TurnInput,
        _: Arc<pilotdeck_iface::context_codec::ContextProofs>,
    ) -> pilotdeck_agent::ports::PortResult<Vec<Box<dyn ResourceCodec>>> {
        Ok(vec![])
    }
    fn notice_services(
        &self,
        _: &CallContext,
        _: &pilotdeck_agent::turn::TurnInput,
        _: Arc<dyn Client>,
        _: &pilotdeck_agent::ports::CallControl,
        _: Arc<pilotdeck_iface::context_codec::ContextProofs>,
    ) -> pilotdeck_agent::ports::PortResult<NoticeServices> {
        Err(PortError::InvalidResponse {
            message: "unused".into(),
        })
    }
}

#[test]
fn native_loop_launch_roundtrips_and_rejects_incomplete_or_relative_configs() {
    let roles = complete_context_roles();
    let launch = NativeLoopLaunch::new(
        "controller".into(),
        "controller-rev-1".into(),
        "/abs/project".into(),
        &roles,
    )
    .expect("complete absolute config should encode");
    let parsed = native_loop_launch::parse(launch.args()).expect("encoded launch should parse");
    assert_eq!(parsed.controller, "controller");
    assert_eq!(parsed.controller_revision, "controller-rev-1");
    assert_eq!(parsed.process_cwd, "/abs/project");
    assert_eq!(parsed.contexts.len(), 5);

    // Relative cwd must be rejected
    assert!(
        NativeLoopLaunch::new(
            "controller".into(),
            "controller-rev-1".into(),
            "relative/project".into(),
            &roles,
        )
        .is_err()
    );

    // Incomplete roles must fail closed in both launch and ProductionBindings
    let mut incomplete = roles.clone();
    incomplete.pop();
    assert!(
        NativeLoopLaunch::new(
            "controller".into(),
            "controller-rev-1".into(),
            "/abs/project".into(),
            &incomplete,
        )
        .is_err()
    );
    assert!(
        ProductionBindings::new(
            "controller".into(),
            "controller-rev-1".into(),
            incomplete,
            Arc::new(DummyAdapters),
        )
        .is_err()
    );

    // Duplicate bindings must fail closed
    let mut dup = roles;
    dup[2].binding = dup[0].binding.clone();
    assert!(
        NativeLoopLaunch::new(
            "controller".into(),
            "controller-rev-1".into(),
            "/abs/project".into(),
            &dup,
        )
        .is_err()
    );
    assert!(
        ProductionBindings::new(
            "controller".into(),
            "controller-rev-1".into(),
            dup,
            Arc::new(DummyAdapters),
        )
        .is_err()
    );
}

#[test]
fn gateway_transcript_snapshot_verifies_receipts_and_rejects_tampering() {
    let values = vec![
        JsValue::String(JsString::from_utf16(vec![0x41, 0xd800, 0x42])),
        JsValue::String("turn_completed".into()),
    ];
    let first = OpaquePayload {
        profile: boundary_codec::PROFILE.into(),
        bytes: boundary_codec::encode(&values[0]).unwrap(),
    };
    let second = OpaquePayload {
        profile: boundary_codec::PROFILE.into(),
        bytes: boundary_codec::encode(&values[1]).unwrap(),
    };
    let mut start = GatewayStartReceipt {
        session: "session-1".into(),
        reserved_input_ref: "input-1".into(),
        origin: InputOrigin {
            principal: "gateway".into(),
            turn: 1,
            generation: 1,
            input_ref: "input-1".into(),
        },
        loop_binding: Binding {
            id: "pilotdeck".into(),
            revision: "rev-1".into(),
            kind: Kind::Loop,
            effect: Effect::ToolWrite,
            permissions: Permissions::calls([]),
        },
        loop_context: "pilotdeck.turn".into(),
        producer_contract: "trusted-gateway".into(),
        producer_instance: "gw-1".into(),
        event_digest: h::gateway_event_digest(&first).unwrap(),
        receipt_digest: String::new(),
        revision: 1,
        event_cursor: 2,
    };
    start.receipt_digest = h::gateway_start_digest(&start).unwrap();
    let mut second_receipt = GatewayTranscriptReceipt {
        start_digest: start.receipt_digest.clone(),
        attempt_id: "attempt-1".into(),
        ordinal: 1,
        terminal_cursor: Some(5),
        event_digest: h::gateway_event_digest(&second).unwrap(),
        revision: 2,
        event_cursor: 5,
        receipt_digest: String::new(),
    };
    second_receipt.receipt_digest = h::gateway_transcript_digest(&second_receipt).unwrap();
    let page0 = GatewayTranscriptPage {
        start: start.clone(),
        upper_cursor: 5,
        total: 2,
        offset: 0,
        entries: vec![GatewayTranscriptEntry {
            event_cursor: 2,
            ordinal: 0,
            receipt: None,
            event: first,
        }],
    };
    let page1 = GatewayTranscriptPage {
        start: start.clone(),
        upper_cursor: 5,
        total: 2,
        offset: 1,
        entries: vec![GatewayTranscriptEntry {
            event_cursor: 5,
            ordinal: 1,
            receipt: Some(second_receipt),
            event: second,
        }],
    };

    let mut snapshot = GatewayTranscriptSnapshot::new(start.clone()).unwrap();
    snapshot.push(page0.clone()).unwrap();

    // Tampered profile must fail
    let mut bad_profile = page1.clone();
    bad_profile.entries[0].event.profile = "wrong-profile".into();
    assert!(snapshot.push(bad_profile).is_err());

    // Tampered cursor must fail
    let mut bad_cursor = page1.clone();
    bad_cursor.entries[0].event_cursor = 1;
    assert!(snapshot.push(bad_cursor).is_err());

    // Tampered receipt digest must fail
    let mut bad_digest = page1.clone();
    bad_digest.entries[0]
        .receipt
        .as_mut()
        .unwrap()
        .receipt_digest = "0".repeat(64);
    assert!(snapshot.push(bad_digest).is_err());

    // Original page completes and preserves lone surrogate UTF-16 code units
    snapshot.push(page1).unwrap();
    let decoded = snapshot.finish().unwrap();
    assert_eq!(decoded, values);

    // Tampered start digest fails upfront
    let mut tampered_start = start;
    tampered_start.loop_binding.id = "other-loop".into();
    assert!(GatewayTranscriptSnapshot::new(tampered_start).is_err());
}

fn encode_opaque_json(raw: &str) -> Value {
    let parsed = parse_json(raw).unwrap();
    serde_json::to_value(OpaquePayload {
        profile: boundary_codec::PROFILE.into(),
        bytes: boundary_codec::encode(&parsed).unwrap(),
    })
    .unwrap()
}

#[tokio::test]
async fn resource_mux_routes_initial_reads_and_tool_codec_enforces_scope_and_features() {
    let ctx = context();
    // 1. Initial reads advertised and decoded through ResourceMux
    let read_client = Arc::new(StubClient {
        features: vec![
            product_reads::PLAN_TODO_FEATURE.into(),
            product_reads::MODEL_METADATA_FEATURE.into(),
        ],
        scope: None,
    });
    let mut mux = ResourceMux::new(vec![
        Box::new(InitialReadCodec::new(read_client.as_ref())),
        Box::new(ModelSessionCodec::new_with_local_resource_facts(
            ctx.clone(),
        )),
    ])
    .unwrap();
    assert!(mux.advertised("capability", "plan_todo"));
    assert!(mux.advertised("model", "get_metadata"));

    let plan_call = HostCall::PlanTodo(PlanTodoRequest {
        session_id: "session-prod-1".into(),
        turn_id: "1".into(),
        mutation: PlanTodoMutation::Read,
    });
    let ResourceStep::Request(req) = mux.begin(&plan_call).unwrap() else {
        panic!("expected PLAN_TODO request");
    };
    assert_eq!(req.method, product_reads::PLAN_TODO);
    let plan_reply = encode_opaque_json(
        r#"{"ok":true,"payload":{"snapshot":{"requiresInitialization":false,"toolCallsSinceLastTodoWrite":0,"todos":[],"activeTodos":[],"todoHistory":[],"todoDiagnostics":{"writeCount":0,"todoCount":0,"activeCount":0,"completedCount":0,"cancelledCount":0,"largeRewriteCount":0,"deletedOpenItemCount":0,"completedWithoutActiveCount":0}}}}"#,
    );
    let ResourceStep::Reply(reply) = mux.resume(&plan_call, plan_reply).unwrap() else {
        panic!("expected PLAN_TODO reply");
    };
    assert!(matches!(*reply, HostReply::PlanTodo(_)));

    let meta_call = HostCall::ModelMetadata {
        route: Route {
            provider: JsString::from_utf16(vec![0x70, 0xd800]),
            model: "m".into(),
        },
    };
    let ResourceStep::Request(req) = mux.begin(&meta_call).unwrap() else {
        panic!("expected MODEL_METADATA request");
    };
    assert_eq!(req.method, product_reads::MODEL_METADATA);
    let meta_reply = encode_opaque_json(
        r#"{"ok":true,"payload":{"metadata":{"provider":"p\ud800","model":"m","maxContextTokens":8192}}}"#,
    );
    let ResourceStep::Reply(reply) = mux.resume(&meta_call, meta_reply).unwrap() else {
        panic!("expected MODEL_METADATA reply");
    };
    let HostReply::ModelMetadata(meta) = *reply else {
        panic!("expected ModelMetadata reply");
    };
    assert_eq!(meta.view.route.provider.units(), [0x70, 0xd800]);
    assert_eq!(meta.view.max_context_tokens, Some(8192.0));

    // Non-read PlanTodo must not be routed to InitialReadCodec
    let write_call = HostCall::PlanTodo(PlanTodoRequest {
        session_id: "session-prod-1".into(),
        turn_id: "1".into(),
        mutation: PlanTodoMutation::MarkToolProgress {
            tool_name: "read_file".into(),
        },
    });
    assert!(matches!(
        mux.begin(&write_call),
        Err(PortError::NotAdvertised { .. })
    ));

    // Unadvertised InitialReadCodec fails closed
    let unadvertised = StubClient {
        features: vec![],
        scope: None,
    };
    let mut disabled = InitialReadCodec::new(&unadvertised);
    assert!(!disabled.advertised("capability", "plan_todo"));
    assert!(!disabled.advertised("model", "get_metadata"));
    assert!(matches!(
        disabled.begin(&plan_call),
        Err(PortError::NotAdvertised { .. })
    ));

    // 2. ToolBatchCodec enforces step_scope, tool_batch::FEATURE, and native_correlation_issuer
    let handle = tokio::runtime::Handle::current();
    let unscoped: Arc<dyn Client> = Arc::new(StubClient {
        features: vec![tool_batch::FEATURE.into()],
        scope: None,
    });
    assert!(ToolBatchCodec::new(ctx.clone(), unscoped, handle.clone()).is_err());

    let wrong_scope: Arc<dyn Client> = Arc::new(StubClient {
        features: vec![tool_batch::FEATURE.into()],
        scope: Some(StepRef {
            grant: "g1".into(),
            session_ref: "other-session".into(),
            call_ref: ctx.call_ref.clone(),
            driver_generation: 1,
            operation_id: "op-1".into(),
            interaction: ctx.interaction.clone(),
        }),
    });
    assert!(ToolBatchCodec::new(ctx.clone(), wrong_scope, handle.clone()).is_err());

    let matching_scope = StepRef {
        grant: "g1".into(),
        session_ref: ctx.session_ref.clone(),
        call_ref: ctx.call_ref.clone(),
        driver_generation: 1,
        operation_id: "op-1".into(),
        interaction: ctx.interaction.clone(),
    };
    let no_feature: Arc<dyn Client> = Arc::new(StubClient {
        features: vec![],
        scope: Some(matching_scope.clone()),
    });
    assert!(matches!(
        ToolBatchCodec::new(ctx.clone(), no_feature, handle.clone()),
        Err(PortError::NotAdvertised { .. })
    ));

    // Even with matching scope + feature, a stub client without real SDK
    // NativeCorrelationIssuer fails closed (cannot forge module/host correlation).
    let no_correlation: Arc<dyn Client> = Arc::new(StubClient {
        features: vec![tool_batch::FEATURE.into()],
        scope: Some(matching_scope),
    });
    assert!(ToolBatchCodec::new(ctx, no_correlation, handle).is_err());

    // Sanity check ExecuteSingle hints construct without panicking
    let _tool_call = HostCall::ExecuteSingle {
        call: ToolInvocation {
            tool_call_id: "tc-1".into(),
            name: "read_file".into(),
            arguments: JsValue::Undefined,
        },
        hints: ToolHints {
            session_id: "session-prod-1".into(),
            turn_id: "1".into(),
            cwd: "/abs/project".into(),
            permission_mode: PermissionMode::Default,
            run_mode: None,
            can_prompt: false,
            can_elicit: None,
            additional_working_directories: vec![],
            context_extensions: JsObject::new(),
            execution: ExecutionHints {
                run_id: "run-prod-1".into(),
                operation_id: "op-1".into(),
                idempotency_key: None,
                operation_deadline: None,
            },
        },
    };
    let _ = ExecutionIdentity {
        session_id: "s".into(),
        turn_id: "1".into(),
        run_id: "r".into(),
        operation_id: "o".into(),
        request_id: "q".into(),
        idempotency_key: None,
        module_instance_id: "m".into(),
        connection_generation: "1".into(),
    };
}
