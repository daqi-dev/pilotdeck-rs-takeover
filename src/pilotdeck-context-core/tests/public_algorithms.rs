//! External-consumer checks for the extracted boundary; the unchanged Gateway
//! fixture replay remains the fixed-TS byte-for-byte compaction oracle.
use pilotdeck_context_core::{
    MicroCompactionEngine, MicroCompactionEngineOptions, MicroCompactionInput, SnipCallOptions,
    SnipEngine, SnipEngineOptions, budget, tool_pair, wire,
};
use pilotdeck_protocol::{JsObject, JsString, JsValue, parse_json};
use std::cell::Cell;

fn json(value: &str) -> JsValue {
    parse_json(value).unwrap()
}
fn messages(value: &str) -> Vec<JsValue> {
    match json(value) {
        JsValue::Array(ref messages) => messages.clone(),
        _ => panic!("message array"),
    }
}

#[test]
fn budget_preserves_usage_provenance_and_reserved_output_thresholds() {
    let metadata = match json(
        r#"{"reservedOutputTokens":200,"usageTokens":720,"localEstimateTokens":600,"displayTokens":720,"source":"provider","exact":true}"#,
    ) {
        JsValue::Object(ref metadata) => metadata.clone(),
        _ => unreachable!(),
    };
    let snapshot = budget::snapshot_from_tokens(720.0, 1000.0, &metadata, 0.8, 0.9);
    assert_eq!(
        snapshot.get("maxContextTokens"),
        Some(&JsValue::Number(800.0))
    );
    assert_eq!(snapshot.get("state"), Some(&JsValue::from("blocking")));
    assert_eq!(
        snapshot.get("estimateSource"),
        Some(&JsValue::from("usage"))
    );
    assert!(snapshot.get("displayTokens").is_none());
    assert_eq!(
        snapshot.get("localEstimateTokens"),
        Some(&JsValue::Number(600.0))
    );
    assert_eq!(snapshot.get("exact"), Some(&JsValue::Bool(true)));
    let decision = budget::evaluate_snapshot(snapshot.clone());
    assert_eq!(
        decision.get("reason"),
        Some(&JsValue::from("blocking_threshold"))
    );
    assert_eq!(decision.get("snapshot"), Some(&snapshot));
    let warning = budget::snapshot_from_tokens(640.0, 1000.0, &metadata, 0.8, 0.9);
    assert_eq!(
        budget::evaluate_snapshot(warning).get("reason"),
        Some(&JsValue::from("warning_threshold"))
    );
    assert_eq!(
        budget::evaluate_snapshot(budget::snapshot_from_tokens(
            20.0,
            1000.0,
            &JsObject::new(),
            0.8,
            0.9
        ))
        .get("type"),
        Some(&JsValue::from("skip"))
    );
}

#[test]
fn budget_nonfinite_values_keep_javascript_semantics() {
    let mut metadata = JsObject::new();
    metadata.insert(
        "reservedOutputTokens".into(),
        JsValue::Number(f64::INFINITY),
    );
    let snapshot = budget::snapshot_from_tokens(50.0, 100.0, &metadata, 0.8, 0.9);
    assert_eq!(
        snapshot.get("maxContextTokens"),
        Some(&JsValue::Number(100.0))
    );
    assert_eq!(
        snapshot.get("reservedOutputTokens"),
        Some(&JsValue::Number(f64::INFINITY))
    );
    let snapshot = budget::snapshot_from_tokens(50.0, f64::NAN, &JsObject::new(), 0.8, 0.9);
    assert!(
        snapshot
            .get("maxContextTokens")
            .and_then(JsValue::as_number)
            .unwrap()
            .is_nan()
    );
    assert_eq!(snapshot.get("ratio"), Some(&JsValue::Number(0.0)));
}

#[test]
fn pair_cleanup_preserves_utf16_ids_and_media_reference_contract() {
    let input = messages(
        r#"[
      {"role":"assistant","content":[{"type":"tool_call","id":"\ud800","name":"read_file"},{"type":"tool_call","id":"orphan","name":"bash"}]},
      {"role":"user","content":[{"type":"tool_result","toolCallId":"\ud800","content":[]},{"type":"media_reference","toolCallId":"orphan","uri":"x"}]},
      {"role":"system","content":[]}
    ]"#,
    );
    let results = tool_pair::collect_tool_result_ids(&input);
    assert_eq!(
        results.iter().map(JsString::units).collect::<Vec<_>>(),
        vec![&[0xd800][..]]
    );
    let calls = tool_pair::strip_unpaired_tool_calls(&input, &results);
    let cleaned =
        tool_pair::strip_unpaired_tool_results(&calls, &tool_pair::collect_tool_call_ids(&calls));
    assert_eq!(cleaned.len(), 2);
    assert_eq!(wire::content(&cleaned[0]).len(), 1);
    assert_eq!(wire::content(&cleaned[1]).len(), 1);
    assert_eq!(
        wire::content(&cleaned[0])[0]
            .get("id")
            .and_then(JsValue::as_string)
            .unwrap()
            .units(),
        &[0xd800]
    );
}

#[test]
fn trim_and_user_anchor_keep_lone_surrogates_and_ecmascript_whitespace() {
    let raw = JsString::from_utf16(vec![0xfeff, 0x00a0, 0xd800, 0x2028]);
    assert_eq!(wire::trim(&raw).units(), &[0xd800]);
    let message = wire::object([
        ("role", "user".into()),
        ("content", JsValue::Array(vec![wire::text_block(raw)])),
    ]);
    assert!(tool_pair::is_real_user_request_message(&message));
    assert!(!tool_pair::is_real_user_request_message(&json(
        r#"{"role":"user","metadata":{"synthetic":true},"content":[{"type":"text","text":"continue"}]}"#
    )));
    assert!(!tool_pair::is_real_user_request_message(&json(
        r#"{"role":"user","content":[{"type":"text","text":"  <memory-context>state"}]}"#
    )));
}

#[test]
fn snip_consumes_injected_counter_and_preserves_error_identity() {
    let input = messages(
        r#"[
      {"role":"user","content":[{"type":"text","text":"old request"}]},
      {"role":"assistant","content":[{"type":"text","text":"old answer"}]},
      {"role":"user","content":[{"type":"text","text":"new request"}]},
      {"role":"assistant","content":[{"type":"text","text":"new answer"}]}
    ]"#,
    );
    let calls = Cell::new(0);
    let counter = |messages: &[JsValue]| -> Result<usize, &'static str> {
        calls.set(calls.get() + 1);
        Ok(messages.len() * 10)
    };
    let engine = SnipEngine::new(SnipEngineOptions::default());
    let result = engine
        .snip(
            &input,
            SnipCallOptions {
                target_tokens: Some(1.0),
                target_total_tokens: None,
            },
            &counter,
        )
        .unwrap();
    assert!(calls.get() > 1);
    assert!(result.applied);
    assert!(result.messages.iter().any(|m| {
        wire::content(m)
            .iter()
            .any(|b| b.get("text") == Some(&JsValue::from("new request")))
    }));
    assert!(!result.messages.iter().any(|m| {
        wire::content(m)
            .iter()
            .any(|b| b.get("text") == Some(&JsValue::from("old answer")))
    }));
    assert_eq!(
        engine
            .snip(&input, SnipCallOptions::default(), &|_| Err::<usize, _>(
                "counter unavailable"
            ))
            .unwrap_err(),
        "counter unavailable"
    );
}

#[test]
fn micro_uses_callers_projection_counter_and_protection() {
    let text = "0123456789".repeat(40);
    let input = messages(&format!(
        r#"[
      {{"role":"assistant","content":[{{"type":"tool_call","id":"c1","name":"read_file"}}]}},
      {{"role":"user","content":[{{"type":"tool_result","toolCallId":"c1","content":[{{"type":"text","text":"{text}"}}]}}]}}
    ]"#
    ));
    let projections = Cell::new(0);
    let flatten = |block: &JsValue| {
        projections.set(projections.get() + 1);
        wire::string_field(&wire::content(block)[0], "text")
            .unwrap()
            .clone()
    };
    let counter = |text: &JsString| Ok::<usize, &'static str>(text.len_utf16());
    let options = MicroCompactionEngineOptions {
        keep_latest: Some(0.0),
        trim_to_tokens: Some(80.0),
        ..Default::default()
    };
    let engine = MicroCompactionEngine::new(options.clone());
    let result = engine
        .apply(&MicroCompactionInput::new(&input), &counter, &flatten)
        .unwrap();
    assert_eq!(result.rewritten, 1);
    assert_eq!(result.tool_call_ids, vec![JsString::from("c1")]);
    assert_eq!(projections.get(), 1);
    assert!(format!("{:?}", result.messages).contains("Old tool result content compacted"));
    let protected = MicroCompactionEngine::new(MicroCompactionEngineOptions {
        protected_tool_names: Some(vec!["read_file".into()]),
        ..options
    });
    assert_eq!(
        protected
            .apply(&MicroCompactionInput::new(&input), &counter, &flatten)
            .unwrap()
            .rewritten,
        0
    );
    assert_eq!(
        engine
            .apply(
                &MicroCompactionInput::new(&input),
                &|_| Err::<usize, _>("tokenizer error"),
                &flatten
            )
            .unwrap_err(),
        "tokenizer error"
    );
}
