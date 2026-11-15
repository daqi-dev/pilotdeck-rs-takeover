use super::*;
use crate::model_session_codec::ModelSessionCodec;
use crate::resources::{ResourceCodec, ResourceRequest, ResourceStep};
use agentcpm_iface::model_session::{
    MODEL_EVENT_PROFILE_V1, MODEL_REQUEST_PROFILE_V1, REQUIRED_MODEL_EVENT_PROFILE,
    REQUIRED_MODEL_REQUEST_PROFILE,
};
use pilotdeck_agent::ports::*;
use pilotdeck_protocol::{JsValue, model::BoundarySnapshot};
use serde_json::json;

fn request() -> CapturedRequest {
    pilotdeck_turn_preparation::admit_model_request(&pilotdeck_protocol::parse_json(r#"{"provider":"p","model":"m","messages":[{"role":"user","content":[{"type":"text","text":"A\ud800B"}]}],"thinking":{"enabled":true,"mode":"high"},"metadata":{"custom":"keep"}}"#).unwrap()).unwrap()
}
fn hints() -> ModelHints {
    ModelHints {
        session_id: "product-session".into(),
        turn_id: "product-turn".into(),
        execution: ExecutionHints {
            run_id: "run-1".into(),
            operation_id: "model-1".into(),
            idempotency_key: Some("model-1".into()),
            operation_deadline: None,
        },
        workspace_id: None,
        storage_config_version: None,
        metadata: None,
        model_override: None,
    }
}
fn prepare_call() -> HostCall {
    HostCall::ModelPrepare {
        handle: PreparedRequestHandle("handle".into()),
        request: Box::new(request()),
        hints: hints(),
    }
}
fn pull_call() -> HostCall {
    HostCall::ModelPull {
        handle: PreparedRequestHandle("handle".into()),
        request: Box::new(request()),
        hints: hints(),
    }
}
fn close_call() -> HostCall {
    HostCall::ModelClose {
        handle: PreparedRequestHandle("handle".into()),
    }
}
fn resource(step: ResourceStep, method: &str) -> ResourceRequest {
    let ResourceStep::Request(request) = step else {
        panic!("expected {method} resource request")
    };
    assert_eq!(request.method, method);
    request
}
fn opened() -> Value {
    json!({"model_session_id":"ms-1","state":"ready","effective_revision":0,"negotiated_profiles":[MODEL_EVENT_PROFILE_V1,MODEL_REQUEST_PROFILE_V1],"capabilities":{"completion":true,"streaming_output":true},"limits":{"inline_block_bytes":17,"reliable_input_bytes":4096,"reliable_input_items":64}})
}
fn prepare(codec: &mut ModelSessionCodec) {
    let open = resource(codec.begin(&prepare_call()).unwrap(), "model.session_open");
    assert_eq!(
        open.payload["extensions"][REQUIRED_MODEL_EVENT_PROFILE],
        MODEL_EVENT_PROFILE_V1
    );
    assert_eq!(
        open.payload["extensions"][REQUIRED_MODEL_REQUEST_PROFILE],
        MODEL_REQUEST_PROFILE_V1
    );
    assert!(open.payload.get("epoch").is_none());
    assert!(open.payload.get("session_id").is_none());
    assert!(
        matches!(codec.resume(&prepare_call(), opened()).unwrap(), ResourceStep::Reply(reply) if matches!(*reply, HostReply::ModelPrepared(_)))
    );
}
fn commit(codec: &mut ModelSessionCodec) -> ResourceStep {
    let call = pull_call();
    let mut step = codec.begin(&call).unwrap();
    let mut bytes = Vec::new();
    let mut sequence = 0;
    loop {
        let ResourceStep::Request(next) = step else {
            panic!("expected input dispatch")
        };
        if next.method == "model.input_commit" {
            break;
        }
        assert_eq!(next.method, "model.input_append");
        assert_eq!(next.payload["chunk_seq"], sequence);
        assert_eq!(next.payload["last_chunk"], false);
        assert_eq!(next.payload["content_type"], MODEL_REQUEST_PROFILE_V1);
        let hex = next.payload["content_hex"].as_str().unwrap();
        bytes.extend(
            (0..hex.len())
                .step_by(2)
                .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap()),
        );
        sequence += 1;
        step = codec.resume(&call, json!({"model_session_id":"ms-1","input_id":"pd-input-1","status":if sequence % 2 == 0 {"consumed"} else {"accepted"}})).unwrap();
    }
    assert!(sequence > 1, "fixture must test actual segmentation");
    assert_eq!(
        String::from_utf8(bytes).unwrap(),
        pilotdeck_protocol::stringify_json(request().wire.request())
            .unwrap()
            .unwrap()
    );
    codec
        .resume(
            &call,
            json!({"model_session_id":"ms-1","input_id":"pd-input-1","admitted":true}),
        )
        .unwrap()
}
fn event(sequence: u64, kind: &str) -> Value {
    let mut event = json!({"type":"model_session.event","session_id":"session-1","model_session_id":"ms-1","event_seq":sequence.to_string(),"kind":kind});
    if kind.starts_with("response_") {
        event["input_id"] = json!("pd-input-1");
        event["response_id"] = json!("response-1");
    }
    event
}
fn structured(sequence: u64, kind: &str, raw: &str) -> Value {
    let mut event = event(sequence, "structured");
    event["profile"] = json!(MODEL_EVENT_PROFILE_V1);
    event["event_type"] = json!(kind);
    event["event_json"] = json!(raw);
    event["input_id"] = json!("pd-input-1");
    event["response_id"] = json!("response-1");
    let boundary = pilotdeck_protocol::parse_json(raw).unwrap();
    event["projection"] = match kind {
        "text_delta" => boundary.get("text").and_then(JsValue::as_string).and_then(|text| text.to_utf8().ok()).map_or(Value::Null, |text| json!({"kind":"content_delta","content_type":"text/plain","content_encoding":"utf8","content_bytes":text.len(),"content":text})),
        "message_end" => json!({"kind":"response_metadata","finish_reason":boundary.get("finishReason").and_then(JsValue::as_string).map(|reason| reason.to_utf8().unwrap())}),
        "usage" => {
            let input = boundary.get("usage").and_then(|usage| usage.get("inputTokens")).and_then(JsValue::as_number).unwrap();
            json!({"kind":"usage","estimated":false,"usage_json":format!("{{\"input_tokens\":{input}}}")})
        },
        _ => Value::Null,
    };
    event
}
fn page(events: Vec<Value>, cursor: &str, terminal: bool, subscription_closed: bool) -> Value {
    json!({"session_id":"session-1","model_session_id":"ms-1","events":events,"cursor":cursor,"has_more":false,"terminal":terminal,"subscription_closed":subscription_closed})
}

#[test]
fn model_codec_keeps_full_request_chunks_usage_after_message_end_and_confirmed_close() {
    let mut codec = ModelSessionCodec::new(tests::context());
    prepare(&mut codec);
    resource(commit(&mut codec), "model.events_read");
    let first = codec
        .resume(
            &pull_call(),
            page(
                vec![
                    event(1, "session_ready"),
                    structured(
                        2,
                        "text_delta",
                        r#"{"type":"text_delta","text":"A\ud800B","raw":{"custom":7}}"#,
                    ),
                ],
                "c2",
                false,
                false,
            ),
        )
        .unwrap();
    let ResourceStep::Reply(first) = first else {
        panic!("expected live prefix")
    };
    let HostReply::ModelChunk(first) = *first else {
        panic!("expected pull result")
    };
    assert!(!first.done);
    assert_eq!(first.events.len(), 1);
    let StreamEvent::BoundaryOnly(boundary) = &first.events[0] else {
        panic!("expected exact source boundary")
    };
    assert_eq!(
        boundary
            .value()
            .get("text")
            .unwrap()
            .as_string()
            .unwrap()
            .units(),
        [0x41, 0xd800, 0x42]
    );
    assert_eq!(
        boundary.value().get("raw").unwrap().get("custom"),
        Some(&JsValue::Number(7.0))
    );
    resource(codec.begin(&pull_call()).unwrap(), "model.events_read");
    let idle = codec
        .resume(&pull_call(), page(vec![], "c2", false, false))
        .unwrap();
    assert!(
        matches!(idle, ResourceStep::Delay { .. }),
        "empty page is bounded idle, never completion"
    );
    let terminal = page(
        vec![
            structured(
                3,
                "message_end",
                r#"{"type":"message_end","finishReason":"length"}"#,
            ),
            structured(
                4,
                "usage",
                r#"{"type":"usage","usage":{"inputTokens":0,"cacheReadTokens":12,"cacheWriteTokens":3,"nativeCost":0.125}}"#,
            ),
            event(5, "response_completed"),
        ],
        "c5",
        false,
        false,
    );
    let ResourceStep::Reply(tail) = codec.resume(&pull_call(), terminal).unwrap() else {
        panic!("accepted source tail must be delivered before close")
    };
    assert!(
        matches!(*tail,HostReply::ModelChunk(StreamChunk{done:false,ref events}) if events.len()==2)
    );
    resource(codec.begin(&pull_call()).unwrap(), "model.events_read");
    resource(
        codec
            .resume(&pull_call(), page(vec![], "c5", false, false))
            .unwrap(),
        "model.session_close",
    );
    resource(
        codec
            .resume(
                &pull_call(),
                json!({"model_session_id":"ms-1","state":"closed","closed_by_backend":true}),
            )
            .unwrap(),
        "model.events_read",
    );
    let mut closed = event(6, "session_closed");
    closed["terminal"] = json!("closed");
    let ResourceStep::Reply(result) = codec
        .resume(&pull_call(), page(vec![closed], "c6", true, false))
        .unwrap()
    else {
        panic!("expected confirmed result")
    };
    let HostReply::ModelChunk(result) = *result else {
        panic!("expected model chunk")
    };
    assert!(result.done);
    assert!(
        result.events.is_empty(),
        "the source tail is not replayed after confirmed close"
    );
    assert!(codec.cleanup_calls().is_empty());
}

#[test]
fn model_codec_requires_profile_ack_before_sending_input() {
    let mut codec = ModelSessionCodec::new(tests::context());
    resource(codec.begin(&prepare_call()).unwrap(), "model.session_open");
    let mut old = opened();
    old.as_object_mut().unwrap().remove("negotiated_profiles");
    assert!(matches!(
        codec.resume(&prepare_call(), old),
        Err(PortError::InvalidResponse { .. })
    ));
    assert_eq!(
        codec.cleanup_calls().len(),
        1,
        "accepted open remains owned on invalid acknowledgement"
    );
}

#[test]
fn model_codec_close_ack_and_subscription_end_do_not_claim_closed() {
    let mut codec = ModelSessionCodec::new(tests::context());
    prepare(&mut codec);
    resource(codec.begin(&close_call()).unwrap(), "model.session_abort");
    resource(
        codec
            .resume(
                &close_call(),
                json!({"model_session_id":"ms-1","state":"closed"}),
            )
            .unwrap(),
        "model.events_read",
    );
    assert!(matches!(
        codec.resume(&close_call(), page(vec![], "c0", false, true)),
        Err(PortError::AcceptedUnknown { .. })
    ));
    assert_eq!(codec.cleanup_calls().len(), 1);
}

#[test]
fn model_codec_rejects_repeated_event_sequence_and_cross_input_identity() {
    for wrong_input in [false, true] {
        let mut codec = ModelSessionCodec::new(tests::context());
        prepare(&mut codec);
        commit(&mut codec);
        let mut invalid = structured(
            if wrong_input { 1 } else { 2 },
            "text_delta",
            r#"{"type":"text_delta","text":"hello"}"#,
        );
        if wrong_input {
            invalid["input_id"] = json!("some-other-input");
        }
        assert!(matches!(
            codec.resume(&pull_call(), page(vec![invalid], "c2", false, false)),
            Err(PortError::InvalidResponse { .. })
        ));
    }
}

#[test]
fn model_codec_retains_unknown_open_but_retires_affirmative_rejection() {
    for rejected in [false, true] {
        let mut codec = ModelSessionCodec::new(tests::context());
        codec.begin(&prepare_call()).unwrap();
        let error = PortError::HostFailure {
            code: if rejected {
                "CAPABILITY_DENIED"
            } else {
                "OUTCOME_UNKNOWN"
            }
            .into(),
            message: "controlled".into(),
            boundary: BoundarySnapshot::capture(&JsValue::Null),
        };
        codec.resource_failed(&prepare_call(), &error);
        assert_eq!(codec.cleanup_calls().len(), usize::from(!rejected));
    }
}

#[test]
fn model_codec_remembers_source_error_across_separate_pull_pages() {
    let mut codec = ModelSessionCodec::new(tests::context());
    prepare(&mut codec);
    commit(&mut codec);
    let error = structured(
        1,
        "error",
        r#"{"type":"error","error":{"provider":"p","protocol":"openai","code":"rate_limit","message":"try later","retryable":true,"raw":{"v":7}}}"#,
    );
    assert!(
        matches!(codec.resume(&pull_call(),page(vec![error],"c1",false,false)).unwrap(),ResourceStep::Reply(reply) if matches!(*reply,HostReply::ModelChunk(StreamChunk{done:false,..})))
    );
    resource(codec.begin(&pull_call()).unwrap(), "model.events_read");
    let mut failed = event(2, "response_failed");
    failed["code"] = json!(-32000);
    failed["message"] = json!("try later");
    resource(
        codec
            .resume(&pull_call(), page(vec![failed], "c2", false, false))
            .unwrap(),
        "model.session_close",
    );
    resource(
        codec
            .resume(
                &pull_call(),
                json!({"model_session_id":"ms-1","state":"closing"}),
            )
            .unwrap(),
        "model.events_read",
    );
    let mut closed = event(3, "session_closed");
    closed["terminal"] = json!("closed");
    assert!(
        matches!(codec.resume(&pull_call(),page(vec![closed],"c3",true,false)).unwrap(),ResourceStep::Reply(reply) if matches!(*reply,HostReply::ModelChunk(StreamChunk{done:true,..})))
    );
}

#[test]
fn model_codec_rejects_raw_tool_authority_divergence_and_missing_scalar_text_projection() {
    for tool in [false, true] {
        let mut codec = ModelSessionCodec::new(tests::context());
        prepare(&mut codec);
        commit(&mut codec);
        let wrong = if tool {
            let mut value = structured(
                1,
                "tool_call_end",
                r#"{"type":"tool_call_end","toolCall":{"id":"real","name":"read","input":{"path":"safe"}}}"#,
            );
            value["projection"] = json!({"kind":"tool_call_completed","tool_call_id":"other","name":"write","arguments_json":"{\"path\":\"unsafe\"}"});
            value
        } else {
            let mut value = structured(
                1,
                "text_delta",
                r#"{"type":"text_delta","text":"valid scalar text"}"#,
            );
            value["projection"] = Value::Null;
            value
        };
        assert!(matches!(
            codec.resume(&pull_call(), page(vec![wrong], "c1", false, false)),
            Err(PortError::InvalidResponse { .. })
        ));
    }
}

#[test]
fn model_codec_rejects_known_type_disguised_as_other_and_usage_argument_divergence() {
    let mut wrongs = Vec::new();
    let mut hidden = structured(
        1,
        "other",
        r#"{"type":"tool_call_end","toolCall":{"id":"real","name":"read","input":{}}}"#,
    );
    hidden["projection"] = Value::Null;
    wrongs.push(hidden);
    let mut arguments = structured(
        1,
        "tool_call_end",
        r#"{"type":"tool_call_end","toolCall":{"id":"real","name":"read","input":{"path":"safe"}}}"#,
    );
    arguments["projection"] = json!({"kind":"tool_call_completed","tool_call_id":"real","name":"read","arguments_json":"{\"path\":\"unsafe\"}"});
    wrongs.push(arguments);
    let mut usage = structured(1, "usage", r#"{"type":"usage","usage":{"inputTokens":0}}"#);
    usage["projection"]["usage_json"] = json!("{\"input_tokens\":10}");
    wrongs.push(usage);
    let missing_finish = structured(1, "message_end", r#"{"type":"message_end"}"#);
    wrongs.push(missing_finish);
    for event in wrongs {
        let mut codec = ModelSessionCodec::new(tests::context());
        prepare(&mut codec);
        commit(&mut codec);
        assert!(matches!(
            codec.resume(&pull_call(), page(vec![event], "c1", false, false)),
            Err(PortError::InvalidResponse { .. })
        ));
    }
}

#[test]
fn model_codec_drains_remaining_pages_after_closed_before_returning_done() {
    let mut codec = ModelSessionCodec::new(tests::context());
    prepare(&mut codec);
    commit(&mut codec);
    resource(
        codec
            .resume(
                &pull_call(),
                page(vec![event(1, "response_completed")], "c1", false, false),
            )
            .unwrap(),
        "model.session_close",
    );
    resource(
        codec
            .resume(
                &pull_call(),
                json!({"model_session_id":"ms-1","state":"closing"}),
            )
            .unwrap(),
        "model.events_read",
    );
    let mut closed = event(2, "session_closed");
    closed["terminal"] = json!("closed");
    let mut first = page(vec![closed], "c2", true, false);
    first["has_more"] = json!(true);
    resource(
        codec.resume(&pull_call(), first).unwrap(),
        "model.events_read",
    );
    let last = page(
        vec![structured(
            3,
            "usage",
            r#"{"type":"usage","usage":{"inputTokens":4}}"#,
        )],
        "c3",
        true,
        false,
    );
    let ResourceStep::Reply(reply) = codec.resume(&pull_call(), last).unwrap() else {
        panic!("expected drained final page")
    };
    assert!(
        matches!(*reply,HostReply::ModelChunk(StreamChunk{done:true,ref events}) if events.len()==1)
    );
}

#[test]
fn model_codec_rejects_success_terminal_after_source_failure() {
    let mut codec = ModelSessionCodec::new(tests::context());
    prepare(&mut codec);
    commit(&mut codec);
    let events = vec![
        structured(
            1,
            "message_end",
            r#"{"type":"message_end","finishReason":"error"}"#,
        ),
        event(2, "response_completed"),
    ];
    assert!(matches!(
        codec.resume(&pull_call(), page(events, "c2", false, false)),
        Err(PortError::InvalidResponse { .. })
    ));
}

#[test]
fn local_release_requires_explicit_negotiation_and_never_becomes_model_content() {
    use agentcpm_iface::model_session::{
        MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1, REQUIRED_MODEL_LOCAL_RESOURCE_FACTS_PROFILE,
    };
    use agentcpm_scheduler_protocol::model_session_resources::EventsPage;
    let mut legacy = ModelSessionCodec::new(tests::context());
    let open = resource(legacy.begin(&prepare_call()).unwrap(), "model.session_open");
    assert!(
        open.payload["extensions"]
            .get(REQUIRED_MODEL_LOCAL_RESOURCE_FACTS_PROFILE)
            .is_none()
    );
    let mut enabled = ModelSessionCodec::new_with_local_resource_facts(tests::context());
    let open = resource(
        enabled.begin(&prepare_call()).unwrap(),
        "model.session_open",
    );
    assert_eq!(
        open.payload["extensions"][REQUIRED_MODEL_LOCAL_RESOURCE_FACTS_PROFILE],
        MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1
    );
    assert!(
        enabled.resume(&prepare_call(), opened()).is_err(),
        "legacy backend cannot silently satisfy explicit facts profile"
    );
    let mut observer = crate::model_session_events::ModelEvents::new(
        "session-1".into(),
        "ms-1".into(),
        "pd-input-1".into(),
    );
    observer.expects_local_release = true;
    observer.job_id = Some("job-1".into());
    let mut fact = event(2, "local_resources_released");
    fact["input_id"] = json!("pd-input-1");
    fact["response_id"] = json!("response-1");
    fact["job_id"] = json!("job-1");
    let data: EventsPage = serde_json::from_value(page(
        vec![event(1, "response_started"), fact],
        "c2",
        false,
        false,
    ))
    .unwrap();
    assert!(observer.apply(data).unwrap().is_empty());
    assert!(observer.local_release_seen);
    assert!(observer.response_end.is_none());
    assert!(observer.session_end.is_none());
}
#[test]
fn local_release_rejects_wrong_or_unnegotiated_job() {
    use agentcpm_scheduler_protocol::model_session_resources::EventsPage;
    let mut fact = event(1, "local_resources_released");
    fact["input_id"] = json!("pd-input-1");
    fact["job_id"] = json!("wrong");
    let data: EventsPage =
        serde_json::from_value(page(vec![fact.clone()], "c1", false, false)).unwrap();
    let mut observer = crate::model_session_events::ModelEvents::new(
        "session-1".into(),
        "ms-1".into(),
        "pd-input-1".into(),
    );
    observer.expects_local_release = true;
    observer.job_id = Some("right".into());
    assert!(observer.apply(data).is_err());
    let data: EventsPage = serde_json::from_value(page(vec![fact], "c1", false, false)).unwrap();
    let mut legacy = crate::model_session_events::ModelEvents::new(
        "session-1".into(),
        "ms-1".into(),
        "pd-input-1".into(),
    );
    assert!(legacy.apply(data).is_err());
}
