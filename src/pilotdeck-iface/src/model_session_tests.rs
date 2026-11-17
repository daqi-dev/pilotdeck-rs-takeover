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
pub(super) fn pull_call() -> HostCall {
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
pub(super) fn event(sequence: u64, kind: &str) -> Value {
    let mut event = json!({"type":"model_session.event","session_id":"session-1","model_session_id":"ms-1","event_seq":sequence.to_string(),"kind":kind});
    if kind.starts_with("response_") {
        event["input_id"] = json!("pd-input-1");
        event["response_id"] = json!("response-1");
        event["config_revision"] = json!(0);
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
pub(super) fn page(
    events: Vec<Value>,
    cursor: &str,
    terminal: bool,
    subscription_closed: bool,
) -> Value {
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

#[test]
fn demand_advances_only_on_consumer_pull_and_checks_ack_identity() {
    use agentcpm_iface::model_session::{
        MODEL_CONSUMER_DEMAND_PROFILE_V1, REQUIRED_MODEL_CONSUMER_DEMAND_PROFILE,
    };
    let mut legacy = ModelSessionCodec::new(tests::context());
    let legacy_open = resource(legacy.begin(&prepare_call()).unwrap(), "model.session_open");
    assert!(
        legacy_open.payload["extensions"]
            .get(REQUIRED_MODEL_CONSUMER_DEMAND_PROFILE)
            .is_none(),
        "demand must remain explicit opt-in"
    );
    let mut unnegotiated = ModelSessionCodec::new(tests::context()).with_consumer_demand();
    resource(
        unnegotiated.begin(&prepare_call()).unwrap(),
        "model.session_open",
    );
    assert!(
        unnegotiated.resume(&prepare_call(), opened()).is_err(),
        "an opted-in consumer must reject an unnegotiated demand profile"
    );
    let mut codec = ModelSessionCodec::new(tests::context()).with_consumer_demand();
    let open = resource(codec.begin(&prepare_call()).unwrap(), "model.session_open");
    assert_eq!(
        open.payload["extensions"][REQUIRED_MODEL_CONSUMER_DEMAND_PROFILE],
        MODEL_CONSUMER_DEMAND_PROFILE_V1
    );
    let mut response = opened();
    response["negotiated_profiles"]
        .as_array_mut()
        .unwrap()
        .push(json!(MODEL_CONSUMER_DEMAND_PROFILE_V1));
    assert!(matches!(
        codec.resume(&prepare_call(), response).unwrap(),
        ResourceStep::Reply(_)
    ));
    let call = pull_call();
    let mut step = codec.begin(&call).unwrap();
    loop {
        let ResourceStep::Request(req) = step else {
            panic!("expected input request")
        };
        if req.method == "model.input_commit" {
            break;
        }
        assert_eq!(req.method, "model.input_append");
        step = codec
            .resume(
                &call,
                json!({"model_session_id":"ms-1","input_id":"pd-input-1","status":"accepted"}),
            )
            .unwrap();
    }
    let first = resource(
        codec
            .resume(
                &call,
                json!({"model_session_id":"ms-1","input_id":"pd-input-1","job_id":"job-1","admitted":true}),
            )
            .unwrap(),
        "model.delivery_advance",
    );
    assert_eq!(first.payload["previous_event_seq"], Value::Null);
    resource(codec.resume(&call, json!({"model_session_id":"ms-1","job_id":"job-1","input_id":"pd-input-1","previous_event_seq":null})).unwrap(), "model.events_read");
    let mut e = structured(1, "text_delta", r#"{"type":"text_delta","text":"x"}"#);
    e["config_revision"] = json!(0);
    assert!(matches!(
        codec
            .resume(&call, page(vec![e], "c1", false, false))
            .unwrap(),
        ResourceStep::Reply(_)
    ));
    let next = resource(codec.begin(&call).unwrap(), "model.delivery_advance");
    assert_eq!(next.payload["previous_event_seq"], "1");
    assert!(codec.resume(&call, json!({"model_session_id":"ms-1","job_id":"other","input_id":"pd-input-1","previous_event_seq":"1"})).is_err());
}

#[test]
fn demand_observed_failure_after_source_is_reconciled_without_new_credit() {
    use agentcpm_iface::model_session::MODEL_CONSUMER_DEMAND_PROFILE_V1;
    let mut codec = ModelSessionCodec::new(tests::context()).with_consumer_demand();
    resource(codec.begin(&prepare_call()).unwrap(), "model.session_open");
    let mut open = opened();
    open["negotiated_profiles"]
        .as_array_mut()
        .unwrap()
        .push(json!(MODEL_CONSUMER_DEMAND_PROFILE_V1));
    assert!(matches!(
        codec.resume(&prepare_call(), open).unwrap(),
        ResourceStep::Reply(_)
    ));
    let call = pull_call();
    let mut step = codec.begin(&call).unwrap();
    loop {
        let ResourceStep::Request(req) = step else {
            panic!("input dispatch")
        };
        if req.method == "model.input_commit" {
            break;
        }
        assert_eq!(req.method, "model.input_append");
        step = codec
            .resume(
                &call,
                json!({"model_session_id":"ms-1","input_id":"pd-input-1","status":"accepted"}),
            )
            .unwrap();
    }
    resource(
        codec
            .resume(
                &call,
                json!({"model_session_id":"ms-1","input_id":"pd-input-1","job_id":"job-1","admitted":true}),
            )
            .unwrap(),
        "model.delivery_advance",
    );
    resource(codec.resume(&call, json!({"model_session_id":"ms-1","job_id":"job-1","input_id":"pd-input-1","previous_event_seq":null})).unwrap(), "model.events_read");
    let mut source = structured(1, "text_delta", r#"{"type":"text_delta","text":"x"}"#);
    source["config_revision"] = json!(0);
    let mut failed = event(2, "response_failed");
    failed["code"] = json!(500);
    failed["message"] = json!("failed");
    assert!(matches!(
        codec
            .resume(&call, page(vec![source, failed], "c2", false, false))
            .unwrap(),
        ResourceStep::Reply(_)
    ));
    resource(codec.begin(&call).unwrap(), "model.events_read");
}

#[test]
fn demand_failed_advance_observes_once_and_retains_owner_and_original_failure() {
    use agentcpm_iface::model_session::MODEL_CONSUMER_DEMAND_PROFILE_V1;
    use agentcpm_scheduler_protocol::Failure;
    for case in [
        "terminal",
        "read_denied",
        "gap",
        "foreign",
        "no_terminal",
        "session_only",
        "source",
    ] {
        let mut codec = ModelSessionCodec::new(tests::context()).with_consumer_demand();
        resource(codec.begin(&prepare_call()).unwrap(), "model.session_open");
        let mut open = opened();
        open["negotiated_profiles"]
            .as_array_mut()
            .unwrap()
            .push(json!(MODEL_CONSUMER_DEMAND_PROFILE_V1));
        assert!(matches!(
            codec.resume(&prepare_call(), open).unwrap(),
            ResourceStep::Reply(_)
        ));
        let call = pull_call();
        let mut step = codec.begin(&call).unwrap();
        loop {
            let ResourceStep::Request(req) = step else {
                panic!("input dispatch")
            };
            if req.method == "model.input_commit" {
                break;
            }
            assert_eq!(req.method, "model.input_append");
            step = codec
                .resume(
                    &call,
                    json!({"model_session_id":"ms-1","input_id":"pd-input-1","status":"accepted"}),
                )
                .unwrap();
        }
        resource(codec.resume(&call, json!({"model_session_id":"ms-1","input_id":"pd-input-1","job_id":"job-1","admitted":true})).unwrap(), "model.delivery_advance");
        resource(codec.resume(&call, json!({"model_session_id":"ms-1","job_id":"job-1","input_id":"pd-input-1","previous_event_seq":null})).unwrap(), "model.events_read");
        let mut source = structured(1, "text_delta", r#"{"type":"text_delta","text":"x"}"#);
        source["config_revision"] = json!(0);
        assert!(matches!(
            codec
                .resume(&call, page(vec![source], "c1", false, false))
                .unwrap(),
            ResourceStep::Reply(_)
        ));
        resource(codec.begin(&call).unwrap(), "model.delivery_advance");
        let original = Failure::new("KERNEL_REJECTED", "scheduler.resource.rejected")
            .with("rpc_error", json!({"reason":"BAD_STATE"}));
        let error = PortError::InvalidResponse {
            message: "placeholder".into(),
        };
        let (read, result) = codec
            .reconcile_failure(&call, "model.delivery_advance", &original, &error, false)
            .unwrap();
        assert_eq!(read.method, "model.events_read");
        assert_eq!(read.payload["cursor"], "c1");
        assert!(matches!(result, PortError::AcceptedUnknown { .. }));
        let result = match case {
            "read_denied" => Err(Failure::new("AUTHORITY_REVOKED", "test.denied")),
            "no_terminal" => Ok(page(vec![], "c1", false, false)),
            "session_only" => {
                let mut end = event(2, "session_closed");
                end["terminal"] = json!("closed");
                Ok(page(vec![end], "c2", false, false))
            }
            "source" => {
                let mut e = structured(
                    2,
                    "text_delta",
                    r#"{"type":"text_delta","text":"unconsumed"}"#,
                );
                e["config_revision"] = json!(0);
                Ok(page(vec![e], "c2", false, false))
            }
            _ => {
                let mut failed = event(if case == "gap" { 3 } else { 2 }, "response_failed");
                if case == "foreign" {
                    failed["response_id"] = json!("other");
                }
                failed["code"] = json!(500);
                failed["message"] = json!("failed");
                Ok(page(vec![failed], "c2", false, false))
            }
        };
        codec.reconciliation_observed(&call, result);
        assert_eq!(codec.reconciliation_cause(), Some(&original));
        assert_eq!(
            codec.cleanup_calls().len(),
            1,
            "{case} must retain cleanup owner"
        );
        // Cleanup uses close only after a valid observed terminal; all other
        // cases abort the still-unsettled generation without granting credit.
        let close = resource(
            codec.begin(&close_call()).unwrap(),
            if case == "terminal" {
                "model.session_close"
            } else {
                "model.session_abort"
            },
        );
        assert_eq!(close.payload["model_session_id"], "ms-1");
    }
}

pub(super) fn demand_after_first_yield(local: bool) -> ModelSessionCodec {
    use agentcpm_iface::model_session::{
        MODEL_CONSUMER_DEMAND_PROFILE_V1, MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1,
    };
    let mut codec = if local {
        ModelSessionCodec::new_with_local_resource_facts(tests::context())
    } else {
        ModelSessionCodec::new(tests::context())
    }
    .with_consumer_demand();
    resource(codec.begin(&prepare_call()).unwrap(), "model.session_open");
    let mut open = opened();
    open["negotiated_profiles"]
        .as_array_mut()
        .unwrap()
        .push(json!(MODEL_CONSUMER_DEMAND_PROFILE_V1));
    if local {
        open["negotiated_profiles"]
            .as_array_mut()
            .unwrap()
            .push(json!(MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1));
    }
    codec.resume(&prepare_call(), open).unwrap();
    let call = pull_call();
    let mut step = codec.begin(&call).unwrap();
    loop {
        let ResourceStep::Request(req) = step else {
            panic!("input")
        };
        if req.method == "model.input_commit" {
            break;
        }
        step = codec
            .resume(
                &call,
                json!({"model_session_id":"ms-1","input_id":"pd-input-1","status":"accepted"}),
            )
            .unwrap();
    }
    resource(
        codec
            .resume(
                &call,
                json!({"model_session_id":"ms-1","input_id":"pd-input-1","job_id":"job-1","admitted":true}),
            )
            .unwrap(),
        "model.delivery_advance",
    );
    resource(codec.resume(&call, json!({"model_session_id":"ms-1","job_id":"job-1","input_id":"pd-input-1","previous_event_seq":null})).unwrap(), "model.events_read");
    let mut source = structured(
        if local { 2 } else { 1 },
        "text_delta",
        r#"{"type":"text_delta","text":"x"}"#,
    );
    source["config_revision"] = json!(0);
    let mut events = Vec::new();
    if local {
        events.push(event(1, "response_started"));
    }
    events.push(source);
    assert!(matches!(
        codec
            .resume(
                &call,
                page(events, if local { "c2" } else { "c1" }, false, false)
            )
            .unwrap(),
        ResourceStep::Reply(_)
    ));
    codec
}

/// The FullLate close path reroutes `ModelClose` into a delivery advance once
/// the driver already consumed a source error event. This builds exactly that
/// state: committed input, consumer demand, an already-delivered source error,
/// and no response terminal observed yet.
fn source_error_after_first_yield(local: bool) -> ModelSessionCodec {
    let mut codec = demand_after_first_yield(local);
    let pull = pull_call();
    resource(codec.begin(&pull).unwrap(), "model.delivery_advance");
    resource(
        codec
            .resume(
                &pull,
                json!({"model_session_id":"ms-1","job_id":"job-1","input_id":"pd-input-1","previous_event_seq": if local { "2" } else { "1" }}),
            )
            .unwrap(),
        "model.events_read",
    );
    let mut error_event = structured(
        if local { 3 } else { 2 },
        "error",
        r#"{"type":"error","error":{"type":"context_length_exceeded","message":"too long"}}"#,
    );
    error_event["config_revision"] = json!(0);
    let ResourceStep::Reply(reply) = codec
        .resume(
            &pull,
            page(
                vec![error_event],
                if local { "c3" } else { "c2" },
                false,
                false,
            ),
        )
        .unwrap()
    else {
        panic!("expected the source error to be delivered to the driver");
    };
    assert!(
        matches!(*reply, HostReply::ModelChunk(StreamChunk { done: false, ref events }) if events.len() == 1),
        "the source error must reach the driver before the close"
    );
    codec
}

/// Replays the exact FullLate Turn 4 (TRIGGER_OVERFLOW) model resource
/// sequence captured from the real kernel wire: the close is rerouted into a
/// delivery advance, the kernel accepts it, the held terminal pages settle the
/// session (local release, response_failed, session_closed), and the close
/// must converge to ModelClosed instead of stranding the turn as unknown.
#[test]
fn wire_replay_turn4_close_settles_after_the_source_error() {
    fn wire_drive(
        codec: &mut ModelSessionCodec,
        call: &HostCall,
        queue: &mut Vec<(&'static str, Value)>,
    ) -> ResourceStep {
        let mut step = codec.begin(call).unwrap();
        loop {
            match step {
                ResourceStep::Reply(_) => return step,
                ResourceStep::Delay { next, .. } => step = *next,
                ResourceStep::Request(request) => {
                    let (expected, response) = queue.remove(0);
                    assert_eq!(
                        request.method, expected,
                        "wire replay request order diverged"
                    );
                    step = codec.resume(call, response).unwrap_or_else(|error| {
                        panic!("wire replay failed after {expected}: {error:?}")
                    });
                }
            }
        }
    }
    fn wire_page(events: Vec<Value>, cursor: &str, terminal: bool) -> Value {
        json!({"session_id":"wire-session-1","model_session_id":"ms-wire-1","events":events,"cursor":cursor,"has_more":false,"terminal":terminal,"subscription_closed":false})
    }
    fn wire_event(sequence: u64, kind: &str) -> Value {
        json!({"type":"model_session.event","session_id":"wire-session-1","model_session_id":"ms-wire-1","event_seq":sequence.to_string(),"kind":kind})
    }
    let mut context = tests::context();
    context.session_ref = "wire-session-1".into();
    let mut codec =
        ModelSessionCodec::new_with_local_resource_facts(context).with_consumer_demand();
    let mut queue = vec![
        (
            "model.session_open",
            json!({
                "model_session_id":"ms-wire-1","state":"opening","effective_revision":0,
                "negotiated_profiles":[
                    "agentcpm.model-event/1",
                    "application/vnd.agentcpm.model-request+json;version=1",
                    "agentcpm.model-consumer-demand/1",
                    "agentcpm.model-local-resource-facts/1"
                ],
                "capabilities":{"completion":true,"streaming_output":true},
                "limits":{"inline_block_bytes":65536,"reliable_input_bytes":4194304,"reliable_input_items":64}
            }),
        ),
        (
            "model.events_read",
            wire_page(vec![wire_event(1, "session_ready")], "w1.1", false),
        ),
        (
            "model.input_append",
            json!({"model_session_id":"ms-wire-1","input_id":"pd-input-1","status":"accepted"}),
        ),
        (
            "model.input_commit",
            json!({"model_session_id":"ms-wire-1","input_id":"pd-input-1","job_id":"wire-job-1","admitted":true,"state":"ready"}),
        ),
        (
            "model.delivery_advance",
            json!({"model_session_id":"ms-wire-1","job_id":"wire-job-1","input_id":"pd-input-1","previous_event_seq":null}),
        ),
        (
            "model.events_read",
            wire_page(
                vec![
                    {
                        let mut accepted = wire_event(2, "input_accepted");
                        accepted["input_id"] = json!("pd-input-1");
                        accepted
                    },
                    {
                        let mut started = wire_event(3, "response_started");
                        started["input_id"] = json!("pd-input-1");
                        started["response_id"] = json!("wire-response-1");
                        started["config_revision"] = json!(0);
                        started
                    },
                    {
                        let mut structured = wire_event(4, "structured");
                        structured["profile"] = json!("agentcpm.model-event/1");
                        structured["event_type"] = json!("request_started");
                        structured["event_json"] = json!(
                            r#"{"type":"request_started","provider":"fixture","model":"mock","providerBaseUrl":"http://127.0.0.1:37145/v1","requestFingerprint":"a4b5daebd24427c3a62c9ae9f58edafd2f635a1ba067fed83fa49cebff68e940"}"#
                        );
                        structured["projection"] = Value::Null;
                        structured["input_id"] = json!("pd-input-1");
                        structured["response_id"] = json!("wire-response-1");
                        structured["config_revision"] = json!(0);
                        structured
                    },
                ],
                "w1.4",
                false,
            ),
        ),
        (
            "model.delivery_advance",
            json!({"model_session_id":"ms-wire-1","job_id":"wire-job-1","input_id":"pd-input-1","previous_event_seq":"4"}),
        ),
        (
            "model.events_read",
            wire_page(
                vec![{
                    let mut error = wire_event(5, "structured");
                    error["profile"] = json!("agentcpm.model-event/1");
                    error["event_type"] = json!("error");
                    error["event_json"] = json!(
                        r#"{"type":"error","error":{"provider":"fixture","protocol":"openai","code":"context_overflow","status":400,"message":"This model's maximum context length is 4096 tokens. However, your messages resulted in 4900 tokens.","retryable":false,"raw":{"error":{"code":"context_length_exceeded","message":"This model's maximum context length is 4096 tokens. However, your messages resulted in 4900 tokens.","type":"invalid_request_error"}},"userHint":"Input exceeds the model context window. Run /compact, start a new session with /new, remove large attachments, or switch to a larger-context model in Settings.","maxContextTokens":4096,"recoverableViaCompact":true}}"#
                    );
                    error["projection"] = Value::Null;
                    error["input_id"] = json!("pd-input-1");
                    error["response_id"] = json!("wire-response-1");
                    error["config_revision"] = json!(0);
                    error
                }],
                "w1.5",
                false,
            ),
        ),
        // The driver breaks on the delivered source error; the explicit close
        // is rerouted into the outstanding-yield advance and accepted.
        (
            "model.delivery_advance",
            json!({"model_session_id":"ms-wire-1","job_id":"wire-job-1","input_id":"pd-input-1","previous_event_seq":"5"}),
        ),
        (
            "model.events_read",
            wire_page(
                vec![
                    {
                        let mut released = wire_event(6, "local_resources_released");
                        released["input_id"] = json!("pd-input-1");
                        released["response_id"] = json!("wire-response-1");
                        released["job_id"] = json!("wire-job-1");
                        released["config_revision"] = json!(0);
                        released
                    },
                    {
                        let mut failed = wire_event(7, "response_failed");
                        failed["input_id"] = json!("pd-input-1");
                        failed["response_id"] = json!("wire-response-1");
                        failed["config_revision"] = json!(0);
                        failed["code"] = json!(-32000);
                        failed["end_kind"] = json!("response_failed");
                        failed["message"] = json!(
                            "This model's maximum context length is 4096 tokens. However, your messages resulted in 4900 tokens."
                        );
                        failed["retryable"] = json!(false);
                        failed
                    },
                ],
                "w1.7",
                false,
            ),
        ),
        (
            "model.session_close",
            json!({"already_closed":false,"closed_by_backend":true,"model_session_id":"ms-wire-1","state":"closed"}),
        ),
        (
            "model.events_read",
            wire_page(
                vec![{
                    let mut closed = wire_event(8, "session_closed");
                    closed["config_revision"] = json!(0);
                    closed["end_kind"] = json!("model_session_closed");
                    closed["terminal"] = json!("closed");
                    closed
                }],
                "w1.8",
                true,
            ),
        ),
    ];
    let prepared = wire_drive(&mut codec, &prepare_call(), &mut queue);
    assert!(
        matches!(prepared, ResourceStep::Reply(ref reply) if matches!(**reply, HostReply::ModelPrepared(_)))
    );
    let first = wire_drive(&mut codec, &pull_call(), &mut queue);
    assert!(
        matches!(first, ResourceStep::Reply(ref reply) if matches!(**reply, HostReply::ModelChunk(ref chunk) if !chunk.done && chunk.events.len() == 1))
    );
    let second = wire_drive(&mut codec, &pull_call(), &mut queue);
    assert!(
        matches!(second, ResourceStep::Reply(ref reply) if matches!(**reply, HostReply::ModelChunk(ref chunk) if !chunk.done))
    );
    let closed = wire_drive(&mut codec, &close_call(), &mut queue);
    assert!(
        matches!(closed, ResourceStep::Reply(ref reply) if matches!(**reply, HostReply::ModelClosed)),
        "the wire-replayed close must settle after the accepted rerouted advance"
    );
    assert!(
        queue.is_empty(),
        "every scripted wire response must have been consumed"
    );
    assert!(codec.cleanup_calls().is_empty());
}

#[test]
fn close_rerouted_delivery_advance_is_read_only_like_a_pull_advance() {
    let mut codec = source_error_after_first_yield(true);
    let close = close_call();
    resource(codec.begin(&close).unwrap(), "model.delivery_advance");
    assert_eq!(
        codec.resource_effect(&close, "model.delivery_advance"),
        pilotdeck_protocol::admission::CallEffect::ReadOnly,
        "a close-rerouted advance only acknowledges an already delivered yield"
    );
    assert_eq!(
        codec.resource_effect(&close, "model.session_close"),
        pilotdeck_protocol::admission::CallEffect::Advance
    );
    assert_eq!(
        codec.resource_effect(&pull_call(), "model.delivery_advance"),
        pilotdeck_protocol::admission::CallEffect::ReadOnly,
        "pull advances keep their read-only classification"
    );
}

#[test]
fn rejected_close_advance_falls_back_to_the_explicit_close_and_settles() {
    use agentcpm_scheduler_protocol::Failure;
    for message in [
        "generation is no longer active",
        "consumer advance does not identify the outstanding yield",
        "consumer demand is not active",
    ] {
        // Without local resource facts the settled unknown terminal must
        // converge through the delivered source error, not a release fact.
        let mut codec = source_error_after_first_yield(false);
        let close = close_call();
        resource(codec.begin(&close).unwrap(), "model.delivery_advance");
        let rejected = Failure::new("MODEL_SESSION_BAD_STATE", message);
        let Some(ResourceStep::Request(fallback)) =
            codec.retry_rejected("model.delivery_advance", &rejected)
        else {
            panic!("close-rerouted advance must fall back after: {message}");
        };
        assert_eq!(
            fallback.method, "model.session_abort",
            "an unseen response terminal aborts the unsettled generation"
        );
        assert_eq!(fallback.payload["model_session_id"], "ms-1");
        resource(
            codec
                .resume(&close, json!({"model_session_id":"ms-1"}))
                .unwrap(),
            "model.events_read",
        );
        let mut failed = event(3, "response_failed");
        failed["code"] = json!(400);
        failed["message"] = json!("context length exceeded");
        let mut closed = event(4, "session_closed");
        closed["terminal"] = json!("unknown");
        let ResourceStep::Reply(reply) = codec
            .resume(&close, page(vec![failed, closed], "c4", true, true))
            .unwrap()
        else {
            panic!("the fallback close must settle, got error for: {message}");
        };
        assert!(
            matches!(*reply, HostReply::ModelClosed),
            "close must converge after the fallback for: {message}"
        );
        // Cleanup must never re-issue the rejected advance: the explicit
        // close path owns every later retry.
        resource(codec.begin(&close).unwrap(), "model.events_read");
        let ResourceStep::Reply(settled) = codec
            .resume(&close, page(vec![], "c4", true, true))
            .unwrap()
        else {
            panic!("a repeated close must settle without another advance");
        };
        assert!(matches!(*settled, HostReply::ModelClosed));
    }
}

#[test]
fn rejected_close_advance_settles_with_local_release_and_retires_cleanup() {
    use agentcpm_scheduler_protocol::Failure;
    let mut codec = source_error_after_first_yield(true);
    let close = close_call();
    resource(codec.begin(&close).unwrap(), "model.delivery_advance");
    let rejected = Failure::new("MODEL_SESSION_BAD_STATE", "generation is no longer active");
    let Some(ResourceStep::Request(fallback)) =
        codec.retry_rejected("model.delivery_advance", &rejected)
    else {
        panic!("close-rerouted advance must fall back to the explicit close");
    };
    assert_eq!(fallback.method, "model.session_abort");
    resource(
        codec
            .resume(&close, json!({"model_session_id":"ms-1"}))
            .unwrap(),
        "model.events_read",
    );
    let mut failed = event(4, "response_failed");
    failed["code"] = json!(400);
    failed["message"] = json!("context length exceeded");
    let mut released = event(5, "local_resources_released");
    released["input_id"] = json!("pd-input-1");
    released["response_id"] = json!("response-1");
    released["job_id"] = json!("job-1");
    released["config_revision"] = json!(0);
    let mut closed = event(6, "session_closed");
    closed["terminal"] = json!("unknown");
    let ResourceStep::Reply(reply) = codec
        .resume(
            &close,
            page(vec![failed, released, closed], "c6", true, true),
        )
        .unwrap()
    else {
        panic!("the fallback close must settle with the local release");
    };
    assert!(matches!(*reply, HostReply::ModelClosed));
    assert!(
        codec.cleanup_calls().is_empty(),
        "a released abort retires the cleanup owner"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kernel_host_port_settles_a_rejected_close_advance_without_unknown_outcome() {
    use agentcpm_iface::BoxFuture;
    use agentcpm_iface::model_session::{
        MODEL_CONSUMER_DEMAND_PROFILE_V1, MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1,
    };
    use agentcpm_scheduler_protocol::{
        self as p, Client, ControlKind, ControlOutcome, Destination, Event, Failure, Invocation,
        Result as PResult, State, StateChange, VersionedState, Wait,
    };
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    struct CloseAdvanceClient {
        advance_count: AtomicUsize,
        events_count: AtomicUsize,
        calls: Mutex<Vec<String>>,
    }

    fn unused<T>() -> BoxFuture<'static, PResult<T>> {
        Box::pin(async { Err(Failure::external("UNUSED", "unused")) })
    }

    impl Client for CloseAdvanceClient {
        fn get(&self, _: &str) -> BoxFuture<'_, PResult<p::CallView>> {
            unused()
        }
        fn find(&self, _: &str, _: &str) -> BoxFuture<'_, PResult<Option<p::CallView>>> {
            unused()
        }
        fn wait(&self, _: &str) -> BoxFuture<'_, PResult<p::CallView>> {
            unused()
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
            unused()
        }
        fn deliver(&self, _: &str, _: Destination, _: Event) -> BoxFuture<'_, PResult<()>> {
            unused()
        }
        fn invoke(&self, _: Invocation) -> BoxFuture<'_, PResult<String>> {
            unused()
        }
        fn children(&self, _: &str) -> BoxFuture<'_, PResult<Vec<p::CallView>>> {
            unused()
        }
        fn control(
            &self,
            _: &str,
            _: ControlKind,
            _: String,
        ) -> BoxFuture<'_, PResult<ControlOutcome>> {
            unused()
        }
        fn respond(
            &self,
            _: &str,
            _: &str,
            _: Vec<p::Message>,
            _: &str,
        ) -> BoxFuture<'_, PResult<()>> {
            unused()
        }
        fn read_state(&self, _: &str) -> BoxFuture<'_, PResult<VersionedState>> {
            unused()
        }
        fn write_state(&self, _: StateChange) -> BoxFuture<'_, PResult<u64>> {
            unused()
        }
        fn resource(&self, method: &str, _payload: Value) -> BoxFuture<'_, PResult<Value>> {
            let method = method.to_owned();
            Box::pin(async move {
                self.calls.lock().unwrap().push(method.clone());
                match method.as_str() {
                    "model.session_open" => {
                        let mut open = opened();
                        open["negotiated_profiles"]
                            .as_array_mut()
                            .unwrap()
                            .push(json!(MODEL_CONSUMER_DEMAND_PROFILE_V1));
                        open["negotiated_profiles"]
                            .as_array_mut()
                            .unwrap()
                            .push(json!(MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1));
                        Ok(open)
                    }
                    "model.input_append" => Ok(
                        json!({"model_session_id":"ms-1","input_id":"pd-input-1","status":"accepted"}),
                    ),
                    "model.input_commit" => Ok(
                        json!({"model_session_id":"ms-1","input_id":"pd-input-1","job_id":"job-1","admitted":true}),
                    ),
                    "model.delivery_advance" => {
                        let n = self.advance_count.fetch_add(1, Ordering::SeqCst);
                        if n == 0 {
                            Ok(
                                json!({"model_session_id":"ms-1","job_id":"job-1","input_id":"pd-input-1","previous_event_seq":null}),
                            )
                        } else {
                            Err(Failure::new(
                                "MODEL_SESSION_BAD_STATE",
                                "generation is no longer active",
                            ))
                        }
                    }
                    "model.events_read" => {
                        let n = self.events_count.fetch_add(1, Ordering::SeqCst);
                        if n == 0 {
                            let mut error_event = structured(
                                2,
                                "error",
                                r#"{"type":"error","error":{"type":"context_length_exceeded","message":"too long"}}"#,
                            );
                            error_event["config_revision"] = json!(0);
                            Ok(page(
                                vec![event(1, "response_started"), error_event],
                                "c2",
                                false,
                                false,
                            ))
                        } else {
                            let mut failed = event(3, "response_failed");
                            failed["code"] = json!(400);
                            failed["message"] = json!("context length exceeded");
                            let mut released = event(4, "local_resources_released");
                            released["input_id"] = json!("pd-input-1");
                            released["response_id"] = json!("response-1");
                            released["job_id"] = json!("job-1");
                            released["config_revision"] = json!(0);
                            let mut closed = event(5, "session_closed");
                            closed["terminal"] = json!("unknown");
                            Ok(page(vec![failed, released, closed], "c5", true, true))
                        }
                    }
                    "model.session_abort" => Ok(json!({"model_session_id":"ms-1"})),
                    _ => Err(Failure::external("UNEXPECTED_METHOD", method)),
                }
            })
        }
        fn events(&self, _: u64) -> BoxFuture<'_, PResult<Vec<Event>>> {
            unused()
        }
        fn wait_events(&self, _: u64) -> BoxFuture<'_, PResult<Vec<Event>>> {
            unused()
        }
    }

    let handle = tokio::runtime::Handle::current();
    let control = CallControl {
        cancellation: Cancellation::default(),
        deadline: None,
    };
    let client = Arc::new(CloseAdvanceClient {
        advance_count: AtomicUsize::new(0),
        events_count: AtomicUsize::new(0),
        calls: Mutex::new(Vec::new()),
    });
    let observed = client.clone();
    let (closed, cleanup_after) = tokio::task::spawn_blocking({
        let client = client.clone();
        let control = control.clone();
        let handle = handle.clone();
        move || {
            let ctx = tests::context();
            let codec = ModelSessionCodec::new_with_local_resource_facts(ctx.clone())
                .with_consumer_demand();
            let mut port =
                crate::resources::KernelHostPort::new(client, ctx, handle, Box::new(codec));
            assert!(matches!(
                port.call(&control, prepare_call()).unwrap(),
                HostReply::ModelPrepared(_)
            ));
            let HostReply::ModelChunk(chunk) = port.call(&control, pull_call()).unwrap() else {
                panic!("expected the source error chunk before the close");
            };
            assert!(!chunk.done);
            assert_eq!(chunk.events.len(), 1);
            let closed = port.call(&control, close_call());
            let cleanup_after = port.cleanup_count_for_test();
            (closed, cleanup_after)
        }
    })
    .await
    .unwrap();

    assert!(
        matches!(closed, Ok(HostReply::ModelClosed)),
        "the close must settle instead of reporting an unknown outcome: {closed:?}"
    );
    assert_eq!(cleanup_after, 0);
    let calls = observed.calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|m| m.as_str() == "model.delivery_advance")
            .count(),
        2,
        "the initial and the close-rerouted advance, nothing more"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|m| m.as_str() == "model.session_abort")
            .count(),
        1
    );
}

#[test]
fn invocation_profile_carries_the_exact_first_pull_hints_through_segmented_input() {
    use agentcpm_iface::model_session::{
        MODEL_INVOCATION_PROFILE_V1, REQUIRED_MODEL_INVOCATION_PROFILE,
    };
    let mut codec = ModelSessionCodec::new(tests::context()).with_invocation_context();
    let open = resource(codec.begin(&prepare_call()).unwrap(), "model.session_open");
    assert_eq!(
        open.payload["extensions"][REQUIRED_MODEL_INVOCATION_PROFILE],
        MODEL_INVOCATION_PROFILE_V1
    );
    let mut response = opened();
    response["negotiated_profiles"]
        .as_array_mut()
        .unwrap()
        .push(json!(MODEL_INVOCATION_PROFILE_V1));
    assert!(matches!(
        codec.resume(&prepare_call(), response).unwrap(),
        ResourceStep::Reply(_)
    ));
    let call = pull_call();
    let mut step = codec.begin(&call).unwrap();
    let mut bytes = Vec::new();
    loop {
        let ResourceStep::Request(next) = step else {
            panic!("expected input request")
        };
        if next.method == "model.input_commit" {
            break;
        }
        assert_eq!(next.payload["content_type"], MODEL_INVOCATION_PROFILE_V1);
        let hex = next.payload["content_hex"].as_str().unwrap();
        bytes.extend(
            (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()),
        );
        step = codec
            .resume(
                &call,
                json!({"model_session_id":"ms-1","input_id":"pd-input-1","status":"accepted"}),
            )
            .unwrap();
    }
    let input = pilotdeck_protocol::model_invocation::decode(&bytes).unwrap();
    assert_eq!(&input.request, request().wire.request());
    assert_eq!(input.context, pilotdeck_transport::model_hints(&hints()));
}

fn enter_consumer_demand_advance(codec: &mut ModelSessionCodec) -> HostCall {
    use agentcpm_iface::model_session::MODEL_CONSUMER_DEMAND_PROFILE_V1;
    resource(codec.begin(&prepare_call()).unwrap(), "model.session_open");
    let mut open = opened();
    open["negotiated_profiles"]
        .as_array_mut()
        .unwrap()
        .push(json!(MODEL_CONSUMER_DEMAND_PROFILE_V1));
    assert!(matches!(
        codec.resume(&prepare_call(), open).unwrap(),
        ResourceStep::Reply(_)
    ));
    let call = pull_call();
    let mut step = codec.begin(&call).unwrap();
    loop {
        let ResourceStep::Request(req) = step else {
            panic!("expected input append or commit")
        };
        if req.method == "model.input_commit" {
            break;
        }
        step = codec
            .resume(
                &call,
                json!({"model_session_id":"ms-1","input_id":"pd-input-1","status":"accepted"}),
            )
            .unwrap();
    }
    resource(
        codec
            .resume(
                &call,
                json!({"model_session_id":"ms-1","input_id":"pd-input-1","job_id":"job-1","admitted":true}),
            )
            .unwrap(),
        "model.delivery_advance",
    );
    call
}

#[test]
fn retry_rejected_handles_handshake_busy_cap_reset_and_closing_fail_closed() {
    use agentcpm_scheduler_protocol::Failure;
    use std::time::Duration;

    let mut codec = ModelSessionCodec::new(tests::context()).with_consumer_demand();
    let call = enter_consumer_demand_advance(&mut codec);

    let handshake_failure = Failure::new(
        "MODEL_SESSION_BAD_STATE",
        "delivery profile, owner, generation or commit handshake is not ready",
    );
    let busy_failure = Failure::new("MODEL_SESSION_BAD_STATE", "Busy");
    let non_handshake = Failure::new("MODEL_SESSION_BAD_STATE", "unrelated bad state");
    let wrong_code = Failure::new("OTHER_CODE", "commit handshake is not ready");

    // Wrong method / non-handshake / wrong code fail closed immediately without consuming retry budget.
    assert!(
        codec
            .retry_rejected("model.events_read", &handshake_failure)
            .is_none()
    );
    assert!(
        codec
            .retry_rejected("model.delivery_advance", &non_handshake)
            .is_none()
    );
    assert!(
        codec
            .retry_rejected("model.delivery_advance", &wrong_code)
            .is_none()
    );

    // First 150 transient handshake / Busy rejections return 20ms Delay(model.delivery_advance).
    for i in 0..150 {
        let err = if i % 2 == 0 {
            &handshake_failure
        } else {
            &busy_failure
        };
        let Some(ResourceStep::Delay { duration, next }) =
            codec.retry_rejected("model.delivery_advance", err)
        else {
            panic!("expected retry at attempt {i}");
        };
        assert_eq!(duration, Duration::from_millis(20));
        let ResourceStep::Request(req) = *next else {
            panic!("expected model.delivery_advance request");
        };
        assert_eq!(req.method, "model.delivery_advance");
    }

    // 151st attempt fails closed -> None, delegating to reconcile_failure.
    assert!(
        codec
            .retry_rejected("model.delivery_advance", &handshake_failure)
            .is_none()
    );
    let reconcile = codec.reconcile_failure(
        &call,
        "model.delivery_advance",
        &handshake_failure,
        &PortError::InvalidResponse {
            message: "max retries exhausted".into(),
        },
        true,
    );
    assert!(reconcile.is_some());

    // Advance succeeds after recovery and a subsequent ModelPull resets the retry counter.
    resource(
        codec
            .resume(
                &call,
                json!({"model_session_id":"ms-1","job_id":"job-1","input_id":"pd-input-1","previous_event_seq":null}),
            )
            .unwrap(),
        "model.events_read",
    );
    let mut source = structured(1, "text_delta", r#"{"type":"text_delta","text":"hi"}"#);
    source["config_revision"] = json!(0);
    assert!(matches!(
        codec
            .resume(&call, page(vec![source], "c1", false, false))
            .unwrap(),
        ResourceStep::Reply(_)
    ));

    // New ModelPull enters Phase::Advance and resets retry budget to 150 again.
    resource(codec.begin(&call).unwrap(), "model.delivery_advance");
    assert!(
        codec
            .retry_rejected("model.delivery_advance", &busy_failure)
            .is_some()
    );

    // Closing model refuses retries.
    resource(codec.begin(&close_call()).unwrap(), "model.session_abort");
    assert!(
        codec
            .retry_rejected("model.delivery_advance", &handshake_failure)
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kernel_host_port_retries_transient_delivery_advance_and_fails_closed_on_other_errors() {
    use agentcpm_iface::BoxFuture;
    use agentcpm_iface::model_session::MODEL_CONSUMER_DEMAND_PROFILE_V1;
    use agentcpm_scheduler_protocol::{
        self as p, Client, ControlKind, ControlOutcome, Destination, Event, Failure, Invocation,
        Result as PResult, State, StateChange, VersionedState, Wait,
    };
    use std::sync::{Arc, Mutex};

    struct ScriptedModelClient {
        advance_failures: Mutex<Vec<Failure>>,
        calls: Mutex<Vec<String>>,
    }

    fn unused<T>() -> BoxFuture<'static, PResult<T>> {
        Box::pin(async { Err(Failure::external("UNUSED", "unused")) })
    }

    impl Client for ScriptedModelClient {
        fn get(&self, _: &str) -> BoxFuture<'_, PResult<p::CallView>> {
            unused()
        }
        fn find(&self, _: &str, _: &str) -> BoxFuture<'_, PResult<Option<p::CallView>>> {
            unused()
        }
        fn wait(&self, _: &str) -> BoxFuture<'_, PResult<p::CallView>> {
            unused()
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
            unused()
        }
        fn deliver(&self, _: &str, _: Destination, _: Event) -> BoxFuture<'_, PResult<()>> {
            unused()
        }
        fn invoke(&self, _: Invocation) -> BoxFuture<'_, PResult<String>> {
            unused()
        }
        fn children(&self, _: &str) -> BoxFuture<'_, PResult<Vec<p::CallView>>> {
            unused()
        }
        fn control(
            &self,
            _: &str,
            _: ControlKind,
            _: String,
        ) -> BoxFuture<'_, PResult<ControlOutcome>> {
            unused()
        }
        fn respond(
            &self,
            _: &str,
            _: &str,
            _: Vec<p::Message>,
            _: &str,
        ) -> BoxFuture<'_, PResult<()>> {
            unused()
        }
        fn read_state(&self, _: &str) -> BoxFuture<'_, PResult<VersionedState>> {
            unused()
        }
        fn write_state(&self, _: StateChange) -> BoxFuture<'_, PResult<u64>> {
            unused()
        }
        fn resource(&self, method: &str, _payload: Value) -> BoxFuture<'_, PResult<Value>> {
            let method = method.to_owned();
            Box::pin(async move {
                self.calls.lock().unwrap().push(method.clone());
                match method.as_str() {
                    "model.session_open" => {
                        let mut open = opened();
                        open["negotiated_profiles"]
                            .as_array_mut()
                            .unwrap()
                            .push(json!(MODEL_CONSUMER_DEMAND_PROFILE_V1));
                        Ok(open)
                    }
                    "model.input_append" => Ok(
                        json!({"model_session_id":"ms-1","input_id":"pd-input-1","status":"accepted"}),
                    ),
                    "model.input_commit" => Ok(
                        json!({"model_session_id":"ms-1","input_id":"pd-input-1","job_id":"job-1","admitted":true}),
                    ),
                    "model.delivery_advance" => {
                        let mut queue = self.advance_failures.lock().unwrap();
                        if !queue.is_empty() {
                            return Err(queue.remove(0));
                        }
                        Ok(
                            json!({"model_session_id":"ms-1","job_id":"job-1","input_id":"pd-input-1","previous_event_seq":null}),
                        )
                    }
                    "model.events_read" => {
                        let mut source =
                            structured(1, "text_delta", r#"{"type":"text_delta","text":"ok"}"#);
                        source["config_revision"] = json!(0);
                        Ok(page(vec![source], "c1", false, false))
                    }
                    _ => Err(Failure::external("UNEXPECTED_METHOD", method)),
                }
            })
        }
        fn events(&self, _: u64) -> BoxFuture<'_, PResult<Vec<Event>>> {
            unused()
        }
        fn wait_events(&self, _: u64) -> BoxFuture<'_, PResult<Vec<Event>>> {
            unused()
        }
    }

    let handle = tokio::runtime::Handle::current();
    let control = CallControl {
        cancellation: Cancellation::default(),
        deadline: None,
    };

    // 1. Transient handshake & Busy errors retry and complete successfully.
    let transient_client = Arc::new(ScriptedModelClient {
        advance_failures: Mutex::new(vec![
            Failure::new(
                "MODEL_SESSION_BAD_STATE",
                "delivery profile, owner, generation or commit handshake is not ready",
            ),
            Failure::new("MODEL_SESSION_BAD_STATE", "Busy"),
        ]),
        calls: Mutex::new(Vec::new()),
    });
    tokio::task::spawn_blocking({
        let client = transient_client.clone();
        let control = control.clone();
        let handle = handle.clone();
        move || {
            let ctx = tests::context();
            let codec = ModelSessionCodec::new(ctx.clone()).with_consumer_demand();
            let mut port =
                crate::resources::KernelHostPort::new(client.clone(), ctx, handle, Box::new(codec));
            assert!(matches!(
                port.call(&control, prepare_call()).unwrap(),
                HostReply::ModelPrepared(_)
            ));
            let HostReply::ModelChunk(chunk) = port.call(&control, pull_call()).unwrap() else {
                panic!("expected streamed model chunk after transient advance retries");
            };
            assert_eq!(chunk.events.len(), 1);
            assert!(!chunk.done);
            let calls = client.calls.lock().unwrap();
            assert_eq!(
                calls
                    .iter()
                    .filter(|m| m.as_str() == "model.delivery_advance")
                    .count(),
                3
            );
        }
    })
    .await
    .unwrap();

    // 2. Non-handshake error does not retry and fails closed via reconciliation.
    let permanent_client = Arc::new(ScriptedModelClient {
        advance_failures: Mutex::new(vec![Failure::new(
            "MODEL_SESSION_BAD_STATE",
            "permanent session state corruption",
        )]),
        calls: Mutex::new(Vec::new()),
    });
    tokio::task::spawn_blocking({
        let client = permanent_client.clone();
        let control = control.clone();
        move || {
            let ctx = tests::context();
            let codec = ModelSessionCodec::new(ctx.clone()).with_consumer_demand();
            let mut port =
                crate::resources::KernelHostPort::new(client.clone(), ctx, handle, Box::new(codec));
            port.call(&control, prepare_call()).unwrap();
            let err = port.call(&control, pull_call()).unwrap_err();
            assert!(matches!(err, PortError::AcceptedUnknown { .. }));
            let calls = client.calls.lock().unwrap();
            assert_eq!(
                calls
                    .iter()
                    .filter(|m| m.as_str() == "model.delivery_advance")
                    .count(),
                1
            );
            assert_eq!(
                calls
                    .iter()
                    .filter(|m| m.as_str() == "model.events_read")
                    .count(),
                1
            );
        }
    })
    .await
    .unwrap();
}

#[test]
fn source_error_model_close_advances_delivery_and_settles_with_local_release() {
    use agentcpm_iface::model_session::{
        MODEL_CONSUMER_DEMAND_PROFILE_V1, MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1,
    };
    let mut codec =
        ModelSessionCodec::new_with_local_resource_facts(tests::context()).with_consumer_demand();
    resource(codec.begin(&prepare_call()).unwrap(), "model.session_open");
    let mut open = opened();
    open["negotiated_profiles"]
        .as_array_mut()
        .unwrap()
        .push(json!(MODEL_CONSUMER_DEMAND_PROFILE_V1));
    open["negotiated_profiles"]
        .as_array_mut()
        .unwrap()
        .push(json!(MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1));
    codec.resume(&prepare_call(), open).unwrap();

    let pull = pull_call();
    let mut step = codec.begin(&pull).unwrap();
    loop {
        let ResourceStep::Request(req) = step else {
            panic!("expected input append or commit");
        };
        if req.method == "model.input_commit" {
            break;
        }
        step = codec
            .resume(
                &pull,
                json!({"model_session_id":"ms-1","input_id":"pd-input-1","status":"accepted"}),
            )
            .unwrap();
    }
    resource(
        codec
            .resume(
                &pull,
                json!({"model_session_id":"ms-1","input_id":"pd-input-1","job_id":"job-1","admitted":true}),
            )
            .unwrap(),
        "model.delivery_advance",
    );
    resource(
        codec
            .resume(
                &pull,
                json!({"model_session_id":"ms-1","job_id":"job-1","input_id":"pd-input-1","previous_event_seq":null}),
            )
            .unwrap(),
        "model.events_read",
    );
    let mut err_ev = structured(
        2,
        "error",
        r#"{"type":"error","error":{"type":"context_length_exceeded","message":"too long"}}"#,
    );
    err_ev["config_revision"] = json!(0);
    let ResourceStep::Reply(reply) = codec
        .resume(
            &pull,
            page(
                vec![event(1, "response_started"), err_ev],
                "c2",
                false,
                false,
            ),
        )
        .unwrap()
    else {
        panic!("expected chunk with error event");
    };
    assert!(matches!(*reply, HostReply::ModelChunk(ref c) if !c.done && c.events.len() == 1));

    // Close triggers delivery_advance so yielded source can emit LocalResourcesReleased + SessionClosed.
    let close = close_call();
    let adv = resource(codec.begin(&close).unwrap(), "model.delivery_advance");
    assert_eq!(adv.payload["previous_event_seq"], json!("2"));
    assert_eq!(
        codec.resource_effect(&pull, "model.delivery_advance"),
        pilotdeck_protocol::admission::CallEffect::ReadOnly
    );
    assert_eq!(
        codec.resource_effect(&pull, "model.events_read"),
        pilotdeck_protocol::admission::CallEffect::ReadOnly
    );
    resource(
        codec
            .resume(
                &close,
                json!({"model_session_id":"ms-1","job_id":"job-1","input_id":"pd-input-1","previous_event_seq":"2"}),
            )
            .unwrap(),
        "model.events_read",
    );
    let mut failed = event(3, "response_failed");
    failed["code"] = json!(400);
    failed["message"] = json!("context length exceeded");
    let mut released = event(4, "local_resources_released");
    released["input_id"] = json!("pd-input-1");
    released["response_id"] = json!("response-1");
    released["job_id"] = json!("job-1");
    released["config_revision"] = json!(0);
    let mut closed = event(5, "session_closed");
    closed["terminal"] = json!("closed");
    let settled = codec
        .resume(
            &close,
            page(vec![failed, released, closed], "c5", true, true),
        )
        .unwrap();
    assert!(matches!(
        settled,
        ResourceStep::Reply(ref r) if matches!(**r, HostReply::ModelClosed)
    ));
    assert!(codec.cleanup_calls().is_empty());
}

#[test]
fn abort_model_close_requires_local_release_before_settling_unknown_session_end() {
    for with_release in [true, false] {
        let mut codec = demand_after_first_yield(true);
        let close = close_call();
        resource(codec.begin(&close).unwrap(), "model.session_abort");
        resource(
            codec
                .resume(&close, json!({"model_session_id":"ms-1"}))
                .unwrap(),
            "model.events_read",
        );
        let mut events = Vec::new();
        let mut seq = 3;
        if with_release {
            let mut released = event(seq, "local_resources_released");
            released["input_id"] = json!("pd-input-1");
            released["response_id"] = json!("response-1");
            released["job_id"] = json!("job-1");
            released["config_revision"] = json!(0);
            events.push(released);
            seq += 1;
        }
        let mut closed = event(seq, "session_closed");
        closed["terminal"] = json!("unknown");
        events.push(closed);
        let result = codec.resume(&close, page(events, &format!("c{seq}"), true, true));
        if with_release {
            assert!(matches!(
                result.unwrap(),
                ResourceStep::Reply(ref r) if matches!(**r, HostReply::ModelClosed)
            ));
            assert!(codec.cleanup_calls().is_empty());
        } else {
            assert!(matches!(result, Err(PortError::AcceptedUnknown { .. })));
            assert_eq!(codec.cleanup_calls().len(), 1);
        }
    }
}

#[test]
fn model_pull_delays_on_local_release_and_interrupt_before_abort_close() {
    for include_closed_unknown in [false, true] {
        let mut codec = demand_after_first_yield(true);
        let pull = pull_call();
        resource(codec.begin(&pull).unwrap(), "model.delivery_advance");
        resource(
            codec
                .resume(
                    &pull,
                    json!({"model_session_id":"ms-1","job_id":"job-1","input_id":"pd-input-1","previous_event_seq":"2"}),
                )
                .unwrap(),
            "model.events_read",
        );
        let mut released = event(3, "local_resources_released");
        released["input_id"] = json!("pd-input-1");
        released["response_id"] = json!("response-1");
        released["job_id"] = json!("job-1");
        released["config_revision"] = json!(0);

        let mut interrupted = event(4, "response_interrupted");
        interrupted["input_id"] = json!("pd-input-1");
        interrupted["response_id"] = json!("response-1");
        interrupted["delivery_stopped"] = json!(true);
        interrupted["backend_stop"] = json!("unknown");
        interrupted["config_revision"] = json!(0);

        let mut events = vec![released, interrupted];
        if include_closed_unknown {
            let mut closed = event(5, "session_closed");
            closed["terminal"] = json!("unknown");
            events.push(closed);
        }

        let step = codec
            .resume(
                &pull,
                page(
                    events,
                    if include_closed_unknown { "c5" } else { "c4" },
                    include_closed_unknown,
                    include_closed_unknown,
                ),
            )
            .unwrap();
        assert!(
            matches!(
                step,
                ResourceStep::Delay { ref next, .. }
                    if matches!(&**next, ResourceStep::Request(req) if req.method == "model.events_read")
            ),
            "expected Delay -> model.events_read so KernelHostPort observes in-flight cancel, got something else"
        );
        codec.resource_failed(
            &pull,
            &PortError::Cancelled {
                cause: CancelCause::Requested("cancel".into()),
            },
        );

        let close = close_call();
        if include_closed_unknown {
            let settled = codec.begin(&close).unwrap();
            assert!(matches!(
                settled,
                ResourceStep::Reply(ref r) if matches!(**r, HostReply::ModelClosed)
            ));
        } else {
            resource(codec.begin(&close).unwrap(), "model.session_abort");
            resource(
                codec
                    .resume(&close, json!({"model_session_id":"ms-1"}))
                    .unwrap(),
                "model.events_read",
            );
            let mut closed = event(5, "session_closed");
            closed["terminal"] = json!("unknown");
            let settled = codec
                .resume(&close, page(vec![closed], "c5", true, true))
                .unwrap();
            assert!(matches!(
                settled,
                ResourceStep::Reply(ref r) if matches!(**r, HostReply::ModelClosed)
            ));
        }
        assert!(codec.cleanup_calls().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delivery_advance_cancelled_interleaving_converges_to_cancelled_not_unknown() {
    use agentcpm_iface::BoxFuture;
    use agentcpm_iface::model_session::MODEL_CONSUMER_DEMAND_PROFILE_V1;
    use agentcpm_scheduler_protocol::{
        self as p, Client, ControlKind, ControlOutcome, Destination, Event, Failure, Invocation,
        Result as PResult, State, StateChange, VersionedState, Wait,
    };
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    enum CancelMode {
        SyncOnSecondAdvance(Failure),
        DelayedAfterSecondAdvance(Failure, Duration),
    }

    struct InterleavedClient {
        mode: CancelMode,
        cancellation: Cancellation,
        advance_count: AtomicUsize,
        events_count: AtomicUsize,
        calls: Mutex<Vec<String>>,
    }

    fn unused<T>() -> BoxFuture<'static, PResult<T>> {
        Box::pin(async { Err(Failure::external("UNUSED", "unused")) })
    }

    impl Client for InterleavedClient {
        fn get(&self, _: &str) -> BoxFuture<'_, PResult<p::CallView>> {
            unused()
        }
        fn find(&self, _: &str, _: &str) -> BoxFuture<'_, PResult<Option<p::CallView>>> {
            unused()
        }
        fn wait(&self, _: &str) -> BoxFuture<'_, PResult<p::CallView>> {
            unused()
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
            unused()
        }
        fn deliver(&self, _: &str, _: Destination, _: Event) -> BoxFuture<'_, PResult<()>> {
            unused()
        }
        fn invoke(&self, _: Invocation) -> BoxFuture<'_, PResult<String>> {
            unused()
        }
        fn children(&self, _: &str) -> BoxFuture<'_, PResult<Vec<p::CallView>>> {
            unused()
        }
        fn control(
            &self,
            _: &str,
            _: ControlKind,
            _: String,
        ) -> BoxFuture<'_, PResult<ControlOutcome>> {
            unused()
        }
        fn respond(
            &self,
            _: &str,
            _: &str,
            _: Vec<p::Message>,
            _: &str,
        ) -> BoxFuture<'_, PResult<()>> {
            unused()
        }
        fn read_state(&self, _: &str) -> BoxFuture<'_, PResult<VersionedState>> {
            unused()
        }
        fn write_state(&self, _: StateChange) -> BoxFuture<'_, PResult<u64>> {
            unused()
        }
        fn resource(&self, method: &str, _payload: Value) -> BoxFuture<'_, PResult<Value>> {
            let method = method.to_owned();
            Box::pin(async move {
                self.calls.lock().unwrap().push(method.clone());
                match method.as_str() {
                    "model.session_open" => {
                        let mut open = opened();
                        open["negotiated_profiles"]
                            .as_array_mut()
                            .unwrap()
                            .push(json!(MODEL_CONSUMER_DEMAND_PROFILE_V1));
                        Ok(open)
                    }
                    "model.input_append" => Ok(
                        json!({"model_session_id":"ms-1","input_id":"pd-input-1","status":"accepted"}),
                    ),
                    "model.input_commit" => Ok(
                        json!({"model_session_id":"ms-1","input_id":"pd-input-1","job_id":"job-1","admitted":true}),
                    ),
                    "model.delivery_advance" => {
                        let n = self.advance_count.fetch_add(1, Ordering::SeqCst);
                        if n == 0 {
                            return Ok(
                                json!({"model_session_id":"ms-1","job_id":"job-1","input_id":"pd-input-1","previous_event_seq":null}),
                            );
                        }
                        match &self.mode {
                            CancelMode::SyncOnSecondAdvance(failure) => {
                                self.cancellation
                                    .cancel(CancelCause::Requested("e2e_cancel".into()));
                                Err(failure.clone())
                            }
                            CancelMode::DelayedAfterSecondAdvance(failure, delay) => {
                                let cancellation = self.cancellation.clone();
                                let delay = *delay;
                                tokio::spawn(async move {
                                    tokio::time::sleep(delay).await;
                                    cancellation
                                        .cancel(CancelCause::Requested("e2e_cancel".into()));
                                });
                                Err(failure.clone())
                            }
                        }
                    }
                    "model.events_read" => {
                        let n = self.events_count.fetch_add(1, Ordering::SeqCst);
                        if n == 0 {
                            let mut source =
                                structured(1, "text_delta", r#"{"type":"text_delta","text":"ok"}"#);
                            source["config_revision"] = json!(0);
                            Ok(page(vec![source], "c1", false, false))
                        } else {
                            let mut closed = event(2, "session_closed");
                            closed["terminal"] = json!("closed");
                            Ok(page(vec![closed], "c2", true, true))
                        }
                    }
                    "model.session_abort" => Ok(json!({"model_session_id":"ms-1"})),
                    _ => Err(Failure::external("UNEXPECTED_METHOD", method)),
                }
            })
        }
        fn events(&self, _: u64) -> BoxFuture<'_, PResult<Vec<Event>>> {
            unused()
        }
        fn wait_events(&self, _: u64) -> BoxFuture<'_, PResult<Vec<Event>>> {
            unused()
        }
    }

    let handle = tokio::runtime::Handle::current();
    for mode in [
        CancelMode::SyncOnSecondAdvance(Failure::new(
            "MODEL_SESSION_BAD_STATE",
            "original delivery permit revoked, expired or cancelled",
        )),
        CancelMode::DelayedAfterSecondAdvance(
            Failure::new("KERNEL_REJECTED", "scheduler.resource.rejected")
                .with("rpc_error", json!({"reason":"BAD_STATE"})),
            Duration::from_millis(30),
        ),
        CancelMode::DelayedAfterSecondAdvance(
            Failure::new(
                "OUTCOME_UNKNOWN",
                "scheduler.resource.accepted_delivery_unconfirmed",
            )
            .with(
                "cause",
                json!({
                    "code": "STEP_AUTHORITY_REVOKED",
                    "message_key": "scheduler.step.authority_revoked"
                }),
            ),
            Duration::from_millis(40),
        ),
    ] {
        let cancellation = Cancellation::default();
        let control = CallControl {
            cancellation: cancellation.clone(),
            deadline: None,
        };
        let client = Arc::new(InterleavedClient {
            mode,
            cancellation: cancellation.clone(),
            advance_count: AtomicUsize::new(0),
            events_count: AtomicUsize::new(0),
            calls: Mutex::new(Vec::new()),
        });
        let observed = client.clone();
        let handle = handle.clone();
        let (err, close_reply, cleanup_after) = tokio::task::spawn_blocking(move || {
            let ctx = tests::context();
            let codec = ModelSessionCodec::new(ctx.clone()).with_consumer_demand();
            let mut port =
                crate::resources::KernelHostPort::new(client, ctx, handle, Box::new(codec));
            assert!(matches!(
                port.call(&control, prepare_call()).unwrap(),
                HostReply::ModelPrepared(_)
            ));
            let HostReply::ModelChunk(chunk) = port.call(&control, pull_call()).unwrap() else {
                panic!("expected first streamed chunk before abort interleaving");
            };
            assert!(!chunk.done);
            let err = port.call(&control, pull_call()).unwrap_err();
            let cleanup = CallControl {
                cancellation: control.cancellation.cleanup_scope(),
                deadline: None,
            };
            let close_reply = port.call(&cleanup, close_call()).unwrap();
            let cleanup_after = port.cleanup_count_for_test();
            (err, close_reply, cleanup_after)
        })
        .await
        .unwrap();

        assert!(
            matches!(
                err,
                PortError::Cancelled {
                    cause: CancelCause::Requested(_)
                }
            ),
            "expected soft cancellation rather than AcceptedUnknown, got {err:?}"
        );
        assert!(matches!(close_reply, HostReply::ModelClosed));
        assert_eq!(cleanup_after, 0);
        let calls = observed.calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .filter(|m| m.as_str() == "model.delivery_advance")
                .count(),
            2
        );
        assert!(calls.iter().any(|m| m == "model.session_abort"));
    }
}

#[test]
fn model_pull_delays_on_closed_session_end_with_interrupted_response() {
    let mut codec = demand_after_first_yield(false);
    let pull = pull_call();
    resource(codec.begin(&pull).unwrap(), "model.delivery_advance");
    resource(
        codec
            .resume(
                &pull,
                json!({"model_session_id":"ms-1","job_id":"job-1","input_id":"pd-input-1","previous_event_seq":"1"}),
            )
            .unwrap(),
        "model.events_read",
    );

    let mut interrupted = event(2, "response_interrupted");
    interrupted["input_id"] = json!("pd-input-1");
    interrupted["response_id"] = json!("response-1");
    interrupted["delivery_stopped"] = json!(true);
    interrupted["backend_stop"] = json!("stopped");
    interrupted["config_revision"] = json!(0);

    let mut closed = event(3, "session_closed");
    closed["terminal"] = json!("closed");

    let step = codec
        .resume(&pull, page(vec![interrupted, closed], "c3", true, true))
        .unwrap();
    assert!(
        matches!(
            step,
            ResourceStep::Delay { ref next, .. }
                if matches!(&**next, ResourceStep::Request(req) if req.method == "model.events_read")
        ),
        "expected Delay -> model.events_read on Closed + Interrupted so KernelHostPort observes cancel"
    );
    codec.resource_failed(
        &pull,
        &PortError::Cancelled {
            cause: CancelCause::Requested("cancel".into()),
        },
    );
    let settled = codec.begin(&close_call()).unwrap();
    assert!(matches!(
        settled,
        ResourceStep::Reply(ref r) if matches!(**r, HostReply::ModelClosed)
    ));
    assert!(codec.cleanup_calls().is_empty());
}

/// The Turn 4 wire capture showed the close's confirmation read failing after
/// the session_close ack (cursor never advanced, the cleanup close re-read the
/// same page). Whatever breaks that tail, an already acked close must not turn
/// the whole turn into an unknown outcome, and the cleanup close must still
/// settle afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kernel_host_port_close_read_tail_failure_stays_a_known_failure() {
    use agentcpm_iface::BoxFuture;
    use agentcpm_iface::model_session::{
        MODEL_CONSUMER_DEMAND_PROFILE_V1, MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1,
    };
    use agentcpm_scheduler_protocol::{
        self as p, Client, ControlKind, ControlOutcome, Destination, Event, Failure, Invocation,
        Result as PResult, State, StateChange, VersionedState, Wait,
    };
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    #[derive(Clone, Copy, Debug)]
    enum TailMode {
        TransportFailure,
        MalformedPage,
    }

    struct CloseTailClient {
        mode: TailMode,
        advance_count: AtomicUsize,
        events_count: AtomicUsize,
        calls: Mutex<Vec<String>>,
    }

    fn unused<T>() -> BoxFuture<'static, PResult<T>> {
        Box::pin(async { Err(Failure::external("UNUSED", "unused")) })
    }

    impl Client for CloseTailClient {
        fn get(&self, _: &str) -> BoxFuture<'_, PResult<p::CallView>> {
            unused()
        }
        fn find(&self, _: &str, _: &str) -> BoxFuture<'_, PResult<Option<p::CallView>>> {
            unused()
        }
        fn wait(&self, _: &str) -> BoxFuture<'_, PResult<p::CallView>> {
            unused()
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
            unused()
        }
        fn deliver(&self, _: &str, _: Destination, _: Event) -> BoxFuture<'_, PResult<()>> {
            unused()
        }
        fn invoke(&self, _: Invocation) -> BoxFuture<'_, PResult<String>> {
            unused()
        }
        fn children(&self, _: &str) -> BoxFuture<'_, PResult<Vec<p::CallView>>> {
            unused()
        }
        fn control(
            &self,
            _: &str,
            _: ControlKind,
            _: String,
        ) -> BoxFuture<'_, PResult<ControlOutcome>> {
            unused()
        }
        fn respond(
            &self,
            _: &str,
            _: &str,
            _: Vec<p::Message>,
            _: &str,
        ) -> BoxFuture<'_, PResult<()>> {
            unused()
        }
        fn read_state(&self, _: &str) -> BoxFuture<'_, PResult<VersionedState>> {
            unused()
        }
        fn write_state(&self, _: StateChange) -> BoxFuture<'_, PResult<u64>> {
            unused()
        }
        fn resource(&self, method: &str, _payload: Value) -> BoxFuture<'_, PResult<Value>> {
            let method = method.to_owned();
            Box::pin(async move {
                self.calls.lock().unwrap().push(method.clone());
                match method.as_str() {
                    "model.session_open" => {
                        let mut open = opened();
                        open["negotiated_profiles"]
                            .as_array_mut()
                            .unwrap()
                            .push(json!(MODEL_CONSUMER_DEMAND_PROFILE_V1));
                        open["negotiated_profiles"]
                            .as_array_mut()
                            .unwrap()
                            .push(json!(MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1));
                        Ok(open)
                    }
                    "model.input_append" => Ok(
                        json!({"model_session_id":"ms-1","input_id":"pd-input-1","status":"accepted"}),
                    ),
                    "model.input_commit" => Ok(
                        json!({"model_session_id":"ms-1","input_id":"pd-input-1","job_id":"job-1","admitted":true}),
                    ),
                    "model.delivery_advance" => {
                        let n = self.advance_count.fetch_add(1, Ordering::SeqCst);
                        let previous = if n == 0 { Value::Null } else { json!("2") };
                        Ok(
                            json!({"model_session_id":"ms-1","job_id":"job-1","input_id":"pd-input-1","previous_event_seq":previous}),
                        )
                    }
                    "model.events_read" => {
                        let n = self.events_count.fetch_add(1, Ordering::SeqCst);
                        match n {
                            // The pull delivers the source error and stops.
                            0 => {
                                let mut error_event = structured(
                                    2,
                                    "error",
                                    r#"{"type":"error","error":{"type":"context_length_exceeded","message":"too long"}}"#,
                                );
                                error_event["config_revision"] = json!(0);
                                Ok(page(
                                    vec![event(1, "response_started"), error_event],
                                    "c2",
                                    false,
                                    false,
                                ))
                            }
                            // The close-rerouted advance drains the held facts,
                            // which drives the explicit session_close.
                            1 => {
                                let mut released = event(3, "local_resources_released");
                                released["input_id"] = json!("pd-input-1");
                                released["response_id"] = json!("response-1");
                                released["job_id"] = json!("job-1");
                                released["config_revision"] = json!(0);
                                let mut failed = event(4, "response_failed");
                                failed["code"] = json!(400);
                                failed["message"] = json!("context length exceeded");
                                Ok(page(vec![released, failed], "c4", false, false))
                            }
                            // The confirmation read after the acked close fails.
                            2 => match self.mode {
                                TailMode::TransportFailure => Err(Failure::new(
                                    "PROVIDER_CLOSED",
                                    "scheduler.provider_closed.peer_closed",
                                )),
                                TailMode::MalformedPage => Ok(json!({
                                    "session_id":"session-1","model_session_id":"ms-1",
                                    "events":[{"type":"model_session.event","session_id":"session-1",
                                        "model_session_id":"ms-1","event_seq":"5","kind":"session_closed",
                                        "terminal":"not-a-terminal"}],
                                    "cursor":"c5","has_more":false,"terminal":true,"subscription_closed":false
                                })),
                            },
                            // The turn-end cleanup close re-reads and settles.
                            _ => {
                                let mut closed = event(5, "session_closed");
                                closed["terminal"] = json!("closed");
                                Ok(page(vec![closed], "c5", true, false))
                            }
                        }
                    }
                    "model.session_close" => Ok(
                        json!({"already_closed":false,"closed_by_backend":true,"model_session_id":"ms-1","state":"closed"}),
                    ),
                    _ => Err(Failure::external("UNEXPECTED_METHOD", method)),
                }
            })
        }
        fn events(&self, _: u64) -> BoxFuture<'_, PResult<Vec<Event>>> {
            unused()
        }
        fn wait_events(&self, _: u64) -> BoxFuture<'_, PResult<Vec<Event>>> {
            unused()
        }
    }

    for mode in [TailMode::TransportFailure, TailMode::MalformedPage] {
        let handle = tokio::runtime::Handle::current();
        let control = CallControl {
            cancellation: Cancellation::default(),
            deadline: None,
        };
        let client = Arc::new(CloseTailClient {
            mode,
            advance_count: AtomicUsize::new(0),
            events_count: AtomicUsize::new(0),
            calls: Mutex::new(Vec::new()),
        });
        let observed = client.clone();
        let (first_close, settled_close, cleanup_after) = tokio::task::spawn_blocking({
            let client = client.clone();
            let control = control.clone();
            let handle = handle.clone();
            move || {
                let ctx = tests::context();
                let codec = ModelSessionCodec::new_with_local_resource_facts(ctx.clone())
                    .with_consumer_demand();
                let mut port =
                    crate::resources::KernelHostPort::new(client, ctx, handle, Box::new(codec));
                assert!(matches!(
                    port.call(&control, prepare_call()).unwrap(),
                    HostReply::ModelPrepared(_)
                ));
                let HostReply::ModelChunk(chunk) = port.call(&control, pull_call()).unwrap() else {
                    panic!("expected the source error chunk before the close");
                };
                assert!(!chunk.done);
                assert_eq!(chunk.events.len(), 1);
                let first_close = port.call(&control, close_call());
                // The turn keeps its source-error disposition; the turn-end
                // cleanup close must still settle the already acked session.
                let settled_close = port.call(&control, close_call());
                let cleanup_after = port.cleanup_count_for_test();
                (first_close, settled_close, cleanup_after)
            }
        })
        .await
        .unwrap();

        assert!(
            matches!(first_close, Err(PortError::HostFailure { .. })),
            "a failed close read tail must stay a known failure for {mode:?}, got {first_close:?}"
        );
        assert!(
            matches!(settled_close, Ok(HostReply::ModelClosed)),
            "the cleanup close must settle after the failed tail for {mode:?}, got {settled_close:?}"
        );
        assert_eq!(cleanup_after, 0);
        let calls = observed.calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .filter(|m| m.as_str() == "model.session_close")
                .count(),
            1,
            "the close ack happens exactly once for {mode:?}"
        );
    }
}

/// The abort flow interrupts the close's confirmation reads after the kernel
/// already acknowledged the session abort. That tail must not report an
/// unknown outcome, or the cancelled turn would be turned into a failed one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kernel_host_port_cancelled_close_confirmation_keeps_the_known_outcome() {
    use agentcpm_iface::BoxFuture;
    use agentcpm_iface::model_session::{
        MODEL_CONSUMER_DEMAND_PROFILE_V1, MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1,
    };
    use agentcpm_scheduler_protocol::{
        self as p, Client, ControlKind, ControlOutcome, Destination, Event, Failure, Invocation,
        Result as PResult, State, StateChange, VersionedState, Wait,
    };
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    struct AbortingCloseClient {
        advance_count: AtomicUsize,
        events_count: AtomicUsize,
        calls: Mutex<Vec<String>>,
    }

    fn unused<T>() -> BoxFuture<'static, PResult<T>> {
        Box::pin(async { Err(Failure::external("UNUSED", "unused")) })
    }

    impl Client for AbortingCloseClient {
        fn get(&self, _: &str) -> BoxFuture<'_, PResult<p::CallView>> {
            unused()
        }
        fn find(&self, _: &str, _: &str) -> BoxFuture<'_, PResult<Option<p::CallView>>> {
            unused()
        }
        fn wait(&self, _: &str) -> BoxFuture<'_, PResult<p::CallView>> {
            unused()
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
            unused()
        }
        fn deliver(&self, _: &str, _: Destination, _: Event) -> BoxFuture<'_, PResult<()>> {
            unused()
        }
        fn invoke(&self, _: Invocation) -> BoxFuture<'_, PResult<String>> {
            unused()
        }
        fn children(&self, _: &str) -> BoxFuture<'_, PResult<Vec<p::CallView>>> {
            unused()
        }
        fn control(
            &self,
            _: &str,
            _: ControlKind,
            _: String,
        ) -> BoxFuture<'_, PResult<ControlOutcome>> {
            unused()
        }
        fn respond(
            &self,
            _: &str,
            _: &str,
            _: Vec<p::Message>,
            _: &str,
        ) -> BoxFuture<'_, PResult<()>> {
            unused()
        }
        fn read_state(&self, _: &str) -> BoxFuture<'_, PResult<VersionedState>> {
            unused()
        }
        fn write_state(&self, _: StateChange) -> BoxFuture<'_, PResult<u64>> {
            unused()
        }
        fn resource(&self, method: &str, _payload: Value) -> BoxFuture<'_, PResult<Value>> {
            let method = method.to_owned();
            Box::pin(async move {
                self.calls.lock().unwrap().push(method.clone());
                match method.as_str() {
                    "model.session_open" => {
                        let mut open = opened();
                        open["negotiated_profiles"]
                            .as_array_mut()
                            .unwrap()
                            .push(json!(MODEL_CONSUMER_DEMAND_PROFILE_V1));
                        open["negotiated_profiles"]
                            .as_array_mut()
                            .unwrap()
                            .push(json!(MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1));
                        Ok(open)
                    }
                    "model.input_append" => Ok(
                        json!({"model_session_id":"ms-1","input_id":"pd-input-1","status":"accepted"}),
                    ),
                    "model.input_commit" => Ok(
                        json!({"model_session_id":"ms-1","input_id":"pd-input-1","job_id":"job-1","admitted":true}),
                    ),
                    "model.delivery_advance" => {
                        let n = self.advance_count.fetch_add(1, Ordering::SeqCst);
                        let previous = if n == 0 { Value::Null } else { json!("1") };
                        Ok(
                            json!({"model_session_id":"ms-1","job_id":"job-1","input_id":"pd-input-1","previous_event_seq":previous}),
                        )
                    }
                    "model.events_read" => {
                        let n = self.events_count.fetch_add(1, Ordering::SeqCst);
                        match n {
                            // The pull yields one live delta and stays open.
                            0 => {
                                let mut delta = structured(
                                    2,
                                    "text_delta",
                                    r#"{"type":"text_delta","text":"STREAMING_BEFORE_ABORT "}"#,
                                );
                                delta["config_revision"] = json!(0);
                                Ok(page(
                                    vec![event(1, "response_started"), delta],
                                    "c2",
                                    false,
                                    false,
                                ))
                            }
                            // The confirmation read after the abort ack hangs
                            // until the caller's deadline interrupts it.
                            1 => std::future::pending::<PResult<Value>>().await,
                            _ => {
                                let mut closed = event(3, "session_closed");
                                closed["terminal"] = json!("closed");
                                Ok(page(vec![closed], "c3", true, true))
                            }
                        }
                    }
                    "model.session_abort" => Ok(json!({"model_session_id":"ms-1"})),
                    _ => Err(Failure::external("UNEXPECTED_METHOD", method)),
                }
            })
        }
        fn events(&self, _: u64) -> BoxFuture<'_, PResult<Vec<Event>>> {
            unused()
        }
        fn wait_events(&self, _: u64) -> BoxFuture<'_, PResult<Vec<Event>>> {
            unused()
        }
    }

    let handle = tokio::runtime::Handle::current();
    let control = CallControl {
        cancellation: Cancellation::default(),
        deadline: None,
    };
    let client = Arc::new(AbortingCloseClient {
        advance_count: AtomicUsize::new(0),
        events_count: AtomicUsize::new(0),
        calls: Mutex::new(Vec::new()),
    });
    let observed = client.clone();
    let (aborted_close, settled_close, cleanup_after) = tokio::task::spawn_blocking({
        let client = client.clone();
        let control = control.clone();
        let handle = handle.clone();
        move || {
            let ctx = tests::context();
            let codec = ModelSessionCodec::new_with_local_resource_facts(ctx.clone())
                .with_consumer_demand();
            let mut port =
                crate::resources::KernelHostPort::new(client, ctx, handle, Box::new(codec));
            assert!(matches!(
                port.call(&control, prepare_call()).unwrap(),
                HostReply::ModelPrepared(_)
            ));
            let HostReply::ModelChunk(chunk) = port.call(&control, pull_call()).unwrap() else {
                panic!("expected the source error chunk before the close");
            };
            assert!(!chunk.done);
            assert_eq!(chunk.events.len(), 1);
            // The user abort interrupts the close after the abort ack: the
            // confirmation read must not turn the cancellation unknown.
            let aborting = CallControl {
                cancellation: control.cancellation.clone(),
                deadline: Some(std::time::Instant::now() + std::time::Duration::from_millis(50)),
            };
            let aborted_close = port.call(&aborting, close_call());
            let settled_close = port.call(&control, close_call());
            let cleanup_after = port.cleanup_count_for_test();
            (aborted_close, settled_close, cleanup_after)
        }
    })
    .await
    .unwrap();

    assert!(
        !matches!(aborted_close, Err(PortError::AcceptedUnknown { .. })),
        "an interrupted close tail must stay a known outcome, got {aborted_close:?}"
    );
    assert!(
        matches!(settled_close, Ok(HostReply::ModelClosed)),
        "the follow-up close must settle, got {settled_close:?}"
    );
    assert_eq!(cleanup_after, 0);
    let calls = observed.calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|m| m.as_str() == "model.session_abort")
            .count(),
        1
    );
}

/// After an aborted step the kernel refuses further resource calls with
/// CALLER_ENDED: the request was never admitted, so the failure stays known.
/// The turn-end capture relies on this to remain advisory instead of turning
/// the cancelled turn into an unknown outcome.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kernel_host_port_caller_ended_is_an_affirmative_rejection() {
    use agentcpm_iface::BoxFuture;
    use agentcpm_iface::model_session::{
        MODEL_CONSUMER_DEMAND_PROFILE_V1, MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1,
    };
    use agentcpm_scheduler_protocol::{
        self as p, Client, ControlKind, ControlOutcome, Destination, Event, Failure, Invocation,
        Result as PResult, State, StateChange, VersionedState, Wait,
    };
    use std::sync::Arc;

    struct CallerEndedClient;

    fn unused<T>() -> BoxFuture<'static, PResult<T>> {
        Box::pin(async { Err(Failure::external("UNUSED", "unused")) })
    }

    impl Client for CallerEndedClient {
        fn get(&self, _: &str) -> BoxFuture<'_, PResult<p::CallView>> {
            unused()
        }
        fn find(&self, _: &str, _: &str) -> BoxFuture<'_, PResult<Option<p::CallView>>> {
            unused()
        }
        fn wait(&self, _: &str) -> BoxFuture<'_, PResult<p::CallView>> {
            unused()
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
            unused()
        }
        fn deliver(&self, _: &str, _: Destination, _: Event) -> BoxFuture<'_, PResult<()>> {
            unused()
        }
        fn invoke(&self, _: Invocation) -> BoxFuture<'_, PResult<String>> {
            unused()
        }
        fn children(&self, _: &str) -> BoxFuture<'_, PResult<Vec<p::CallView>>> {
            unused()
        }
        fn control(
            &self,
            _: &str,
            _: ControlKind,
            _: String,
        ) -> BoxFuture<'_, PResult<ControlOutcome>> {
            unused()
        }
        fn respond(
            &self,
            _: &str,
            _: &str,
            _: Vec<p::Message>,
            _: &str,
        ) -> BoxFuture<'_, PResult<()>> {
            unused()
        }
        fn read_state(&self, _: &str) -> BoxFuture<'_, PResult<VersionedState>> {
            unused()
        }
        fn write_state(&self, _: StateChange) -> BoxFuture<'_, PResult<u64>> {
            unused()
        }
        fn resource(&self, method: &str, _payload: Value) -> BoxFuture<'_, PResult<Value>> {
            let method = method.to_owned();
            Box::pin(async move {
                match method.as_str() {
                    "model.session_open" => {
                        let mut open = opened();
                        open["negotiated_profiles"]
                            .as_array_mut()
                            .unwrap()
                            .push(json!(MODEL_CONSUMER_DEMAND_PROFILE_V1));
                        open["negotiated_profiles"]
                            .as_array_mut()
                            .unwrap()
                            .push(json!(MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1));
                        Ok(open)
                    }
                    // The kernel already ended this step's resource caller:
                    // the very first effectful request is refused admission.
                    _ => Err(Failure::new(
                        "CALLER_ENDED",
                        "scheduler.caller_ended.resource_caller_ended",
                    )),
                }
            })
        }
        fn events(&self, _: u64) -> BoxFuture<'_, PResult<Vec<Event>>> {
            unused()
        }
        fn wait_events(&self, _: u64) -> BoxFuture<'_, PResult<Vec<Event>>> {
            unused()
        }
    }

    let handle = tokio::runtime::Handle::current();
    let control = CallControl {
        cancellation: Cancellation::default(),
        deadline: None,
    };
    let result = tokio::task::spawn_blocking(move || {
        let ctx = tests::context();
        let codec =
            ModelSessionCodec::new_with_local_resource_facts(ctx.clone()).with_consumer_demand();
        let mut port = crate::resources::KernelHostPort::new(
            Arc::new(CallerEndedClient),
            ctx,
            handle,
            Box::new(codec),
        );
        assert!(matches!(
            port.call(&control, prepare_call()).unwrap(),
            HostReply::ModelPrepared(_)
        ));
        // The pull's first effectful request (input append) is refused.
        port.call(&control, pull_call())
    })
    .await
    .unwrap();

    assert!(
        matches!(
            result,
            Err(PortError::HostFailure { ref code, .. }) if code.equals_str("CALLER_ENDED")
        ),
        "CALLER_ENDED must stay an affirmative rejection, got {result:?}"
    );
}
