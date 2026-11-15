use pilotdeck_model_core::{anthropic, arguments, google, openai, request};
use pilotdeck_protocol::{JsString, JsValue, parse_json, stringify_json};

fn field<'a>(value: &'a JsValue, key: &str) -> &'a JsValue {
    value.get(key).expect("fixture field")
}

fn cases<'a>(value: &'a JsValue, key: &str) -> Vec<&'a JsValue> {
    let JsValue::Array(items) = field(value, key) else {
        panic!("fixture cases must be array")
    };
    items.iter().collect()
}

fn text(value: &JsValue) -> String {
    value.as_string().unwrap().to_utf8().unwrap()
}

fn json(value: &JsValue) -> String {
    stringify_json(value).unwrap().unwrap_or_default()
}

#[test]
fn public_normalization_matches_fixed_ts_execution() {
    let fixture = parse_json(include_str!("fixtures/normalization.json")).unwrap();
    assert_eq!(
        text(field(&fixture, "sourceCommit")),
        "cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85"
    );
    let usage_cases = cases(&fixture, "usageCases");
    let finish_cases = cases(&fixture, "finishCases");
    let body_cases = cases(&fixture, "bodyCases");
    assert_eq!(
        (usage_cases.len(), finish_cases.len(), body_cases.len()),
        (52, 64, 4)
    );
    for case in usage_cases {
        let protocol = text(field(case, "protocol"));
        let raw = field(case, "raw");
        let actual = match protocol.as_str() {
            "anthropic" => anthropic::normalize_usage(raw),
            "openai" | "openai-responses" => openai::normalize_usage(raw),
            "google" => google::normalize_usage(raw),
            other => panic!("unexpected protocol {other}"),
        };
        assert_eq!(
            json(&actual.unwrap_or(JsValue::Null)),
            json(field(case, "expected")),
            "usage {protocol}: {raw:?}"
        );
    }
    for case in finish_cases {
        let protocol = text(field(case, "protocol"));
        let raw = field(case, "raw");
        let actual = match protocol.as_str() {
            "anthropic" => anthropic::normalize_finish_reason(raw),
            "openai" | "openai-responses" => openai::normalize_finish_reason(raw),
            "google" => google::normalize_finish_reason(raw),
            other => panic!("unexpected protocol {other}"),
        };
        assert_eq!(
            actual,
            text(field(case, "expected")),
            "finish {protocol}: {raw:?}"
        );
    }
    for case in body_cases {
        let actual = request::final_body(field(case, "body"), field(case, "provider"));
        assert_eq!(json(&actual), json(field(case, "expected")));
    }
}

#[test]
fn utf16_arguments_keep_lone_surrogates_and_repair_identity() {
    let mut raw: Vec<u16> = "{\"v\":\"".encode_utf16().collect();
    raw.push(0xd800);
    raw.extend("\"}".encode_utf16());
    let (value, repaired) = arguments::parse_tool_arguments(&raw).unwrap();
    assert!(!repaired);
    assert_eq!(field(&value, "v").as_string().unwrap().units(), [0xd800]);

    let broken: Vec<u16> = "{v:'fixed',}".encode_utf16().collect();
    let (value, repaired) = arguments::parse_tool_arguments(&broken).unwrap();
    assert!(repaired);
    assert_eq!(json(&value), r#"{"v":"fixed"}"#);

    let literal = JsValue::String(JsString::from_utf16(vec![0xd800]));
    assert_eq!(json(&literal), r#""\ud800""#);
}

#[test]
fn response_shape_checks_protocol_specific_envelopes() {
    for (protocol, raw, expected) in [
        ("anthropic", r#"{"type":"message"}"#, true),
        ("anthropic", r#"{"content":[]}"#, true),
        ("anthropic", r#"{"choices":[]}"#, false),
        ("openai-responses", r#"{"output_text":""}"#, true),
        ("openai-responses", r#"{"output":[]}"#, true),
        ("openai-responses", r#"{"choices":[]}"#, false),
        ("openai", r#"{"choices":[]}"#, true),
        ("openai", r#"{"output":[]}"#, false),
        ("openai", "[]", false),
    ] {
        assert_eq!(
            request::expected_complete_response_shape(&protocol.into(), &parse_json(raw).unwrap()),
            expected
        );
    }
}
