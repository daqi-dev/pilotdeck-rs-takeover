//! Original edgeclaw-capture-turn conversionSpecs and expected conversions.
use pilotdeck_context_runtime::memory::{
    CanonicalMessagesToMemoryMessagesOptions, canonical_messages_to_memory_messages,
};
use pilotdeck_protocol::{JsObject, JsValue, parse_json, stringify_json};

#[test]
fn canonical_conversion_replays_ts_oracle() {
    let oracle = parse_json(include_str!(
        "../../pilotdeck-gateway/tests/fixtures/edgeclaw-capture-turn/oracle.json"
    ))
    .unwrap();
    assert_eq!(
        oracle.get("sourceCommit"),
        Some(&JsValue::from("cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85"))
    );
    let Some(JsValue::Array(specs)) = oracle.get("conversionSpecs") else {
        panic!("missing conversionSpecs")
    };
    let mut actual = Vec::new();
    for spec in specs {
        let Some(JsValue::Array(input)) = spec.get("messages") else {
            panic!("missing messages")
        };
        let include = spec.get("includeForkCarryover").and_then(JsValue::as_bool);
        let messages = canonical_messages_to_memory_messages(
            input,
            CanonicalMessagesToMemoryMessagesOptions {
                include_fork_carryover: include,
            },
            &|v| pilotdeck_model_core::codec::js::display(v).ok(),
        )
        .unwrap();
        let mut item = JsObject::new();
        item.insert("name".into(), spec.get("name").unwrap().clone());
        item.insert(
            "includeForkCarryover".into(),
            include.map(JsValue::Bool).unwrap_or(JsValue::Null),
        );
        item.insert(
            "result".into(),
            JsValue::Array(messages.iter().map(|v| v.to_js()).collect()),
        );
        actual.push(JsValue::Object(item));
    }
    assert_eq!(
        stringify_json(&JsValue::Array(actual)),
        stringify_json(oracle.get("conversions").unwrap())
    );
}
