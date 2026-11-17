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
