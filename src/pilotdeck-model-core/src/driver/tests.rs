use super::*;
use crate::codec::{CodecError, limits::LimitGuard, projection::object};
use pilotdeck_protocol::{JsObject, JsString, JsValue, parse_json};
use sha2::{Digest, Sha256};
use std::path::Path;

fn field<'a>(value: &'a JsValue, key: &str) -> &'a JsValue {
    value
        .get(key)
        .unwrap_or_else(|| panic!("missing fixture field {key}"))
}
fn array(value: &JsValue) -> &[JsValue] {
    let JsValue::Array(values) = value else {
        panic!("expected fixture array")
    };
    values
}
fn decode(value: &JsValue) -> JsValue {
    match value {
        JsValue::Object(value) if value.get("$undefined") == Some(&JsValue::Bool(true)) => {
            JsValue::Undefined
        }
        JsValue::Object(value) if value.get("$number").is_some() => JsValue::Number(
            match value
                .get("$number")
                .unwrap()
                .as_string()
                .unwrap()
                .to_utf8()
                .unwrap()
                .as_str()
            {
                "NaN" => f64::NAN,
                "+Infinity" => f64::INFINITY,
                "-Infinity" => f64::NEG_INFINITY,
                "-0" => -0.0,
                other => panic!("unknown numeric tag {other}"),
            },
        ),
        JsValue::Object(value) => {
            let mut out = JsObject::new();
            for (key, value) in value.entries() {
                out.insert(key.clone(), decode(value));
            }
            JsValue::Object(out)
        }
        JsValue::Array(values) => JsValue::Array(values.iter().map(decode).collect()),
        other => other.clone(),
    }
}
// Keep object order, undefined fields, UTF-16, NaN, and signed zero observable.
fn same(left: &JsValue, right: &JsValue) -> bool {
    match (left, right) {
        (JsValue::Number(a), JsValue::Number(b)) => {
            (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits()
        }
        (JsValue::Array(a), JsValue::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same(a, b))
        }
        (JsValue::Object(a), JsValue::Object(b)) => {
            a.len() == b.len()
                && a.entries()
                    .iter()
                    .zip(b.entries())
                    .all(|((ka, a), (kb, b))| ka == kb && same(a, b))
        }
        _ => left == right,
    }
}
fn fixture() -> JsValue {
    let bytes = include_str!("../../tests/fixtures/driver.json");
    assert_eq!(
        format!("{:x}", Sha256::digest(bytes.as_bytes())),
        "33b81b1e11658c8cae6f13eb9aee29f55d02eef3807182db4d6f23e9a57a8285"
    );
    let raw = parse_json(bytes).unwrap();
    let baseline = std::env::var_os("PDRS_BASELINE");
    crate::codec::test_baseline::check_impl(&raw, baseline.as_deref().map(Path::new)).unwrap();
    decode(&raw)
}
fn retry_result(case: &JsValue) -> JsValue {
    let input = field(case, "input");
    let provider = field(input, "provider");
    let attempt = field(input, "attempt").as_number().unwrap();
    let random = field(case, "random").as_number().unwrap();
    let (max, delay) = if field(input, "kind")
        .as_string()
        .unwrap()
        .equals_str("request")
    {
        retry_decide_request(provider, attempt, random)
    } else {
        retry_decide(
            provider,
            attempt,
            input.get("retryAfterMs").and_then(JsValue::as_number),
            random,
        )
    };
    object([
        ("maxRetries", JsValue::Number(max)),
        ("delayMs", JsValue::Number(delay)),
    ])
}
fn snapshot(checkpoint: &Checkpoint) -> JsValue {
    object([
        (
            "partialText",
            JsValue::String(JsString::from_utf16(checkpoint.partial_text().to_vec())),
        ),
        ("hasText", JsValue::Bool(checkpoint.has_text())),
        (
            "canContinueText",
            JsValue::Bool(checkpoint.can_continue_text()),
        ),
        ("interruption", checkpoint.interruption()),
    ])
}

#[test]
fn native_retry_matches_direct_ts_execution_including_nan_and_signed_zero() {
    let fixture = fixture();
    let cases = array(field(&fixture, "retryCases"));
    assert_eq!(cases.len(), 30);
    for case in cases {
        let actual = retry_result(case);
        assert!(
            same(&actual, field(case, "expected")),
            "retry: {case:?}; actual: {actual:?}"
        );
    }
}
#[test]
fn checkpoint_matches_direct_ts_execution_after_every_event() {
    let fixture = fixture();
    let cases = array(field(&fixture, "checkpointCases"));
    assert_eq!(cases.len(), 9);
    for case in cases {
        let mut checkpoint = Checkpoint::default();
        let mut actual = vec![snapshot(&checkpoint)];
        for event in array(field(case, "events")) {
            checkpoint.on_event(event).unwrap();
            actual.push(snapshot(&checkpoint));
        }
        assert!(
            same(&JsValue::Array(actual), field(case, "snapshots")),
            "checkpoint: {case:?}"
        );
    }
}
#[test]
fn continuation_matches_direct_ts_execution_with_cache_invalidation_and_utf16() {
    let fixture = fixture();
    let cases = array(field(&fixture, "continuationCases"));
    assert_eq!(cases.len(), 6);
    for case in cases {
        let actual = continuation_request(
            field(case, "original"),
            field(case, "partial").as_string().unwrap().units(),
        );
        assert!(
            same(&actual, field(case, "expected")),
            "continuation: {case:?}; actual: {actual:?}"
        );
    }
}
#[test]
fn driver_oracle_negative_controls_reject_plausible_wrong_results() {
    let fixture = fixture();
    let case = &array(field(&fixture, "retryCases"))[0];
    let mut wrong = retry_result(case).as_object().unwrap().clone();
    wrong.insert("delayMs".into(), JsValue::Number(0.0));
    assert!(!same(&JsValue::Object(wrong), field(case, "expected")));
    let case = &array(field(&fixture, "continuationCases"))[1];
    let partial = String::from_utf16_lossy(field(case, "partial").as_string().unwrap().units());
    let wrong = continuation_request(
        field(case, "original"),
        &partial.encode_utf16().collect::<Vec<_>>(),
    );
    assert!(
        !same(&wrong, field(case, "expected")),
        "lossy surrogate replacement must fail"
    );
    let mut wrong = field(case, "expected").as_object().unwrap().clone();
    wrong.insert("cachePlan".into(), JsValue::Null);
    assert!(
        !same(&JsValue::Object(wrong), field(case, "expected")),
        "undefined must not collapse to null"
    );
    assert!(!same(&JsValue::Number(-0.0), &JsValue::Number(0.0)));
}
#[test]
fn checkpoint_string_cap_leaves_the_last_valid_state_intact() {
    let _limit = LimitGuard::set(3);
    let mut checkpoint = Checkpoint::default();
    checkpoint
        .on_event(&parse_json(r#"{"type":"text_delta","text":"😀x"}"#).unwrap())
        .unwrap();
    let error = checkpoint
        .on_event(&parse_json(r#"{"type":"text_delta","text":"y"}"#).unwrap())
        .unwrap_err();
    assert!(error.name().equals_str("RangeError"));
    assert!(error.message().equals_str("Invalid string length"));
    assert_eq!(
        checkpoint.partial_text(),
        "😀x".encode_utf16().collect::<Vec<_>>()
    );
}
#[test]
fn guard_retains_explicit_time_environment_and_error_priority() {
    let provider =
        parse_json(r#"{"retry":{"maxStreamingDurationMs":10,"repeatedChunkLimit":1}}"#).unwrap();
    let mut guard = StreamGuard::new(&provider, 100.0, Some("1000"));
    assert!(guard.check_duration(110.0).is_ok());
    let error = guard
        .observe(
            &parse_json(r#"{"type":"text_delta","text":"abc"}"#).unwrap(),
            111.0,
        )
        .unwrap_err();
    assert!(error.name().equals_str("MaxStreamingDurationError"));
    assert!(
        error
            .message()
            .equals_str("Stream exceeded max streaming duration of 10ms")
    );
    let empty = parse_json("{}").unwrap();
    let guard = StreamGuard::new(&empty, 0.0, Some(" 0x1 "));
    assert!(guard.check_duration(1000.0).is_ok());
    assert!(guard.check_duration(1001.0).is_err());
    for raw in ["", " \u{feff}", "0", "-1", "Infinity", "invalid"] {
        assert!(
            StreamGuard::new(&empty, 0.0, Some(raw))
                .check_duration(f64::MAX)
                .is_ok()
        );
    }
    let zero = parse_json(r#"{"retry":{"maxStreamingDurationMs":0}}"#).unwrap();
    assert!(
        StreamGuard::new(&zero, 0.0, Some("100"))
            .check_duration(1.0)
            .is_err()
    );
}
#[test]
fn repeated_chunk_guard_counts_utf16_and_preserves_error_text() {
    let provider = parse_json(r#"{"retry":{"repeatedChunkLimit":2}}"#).unwrap();
    let mut guard = StreamGuard::new(&provider, 0.0, None);
    let event = parse_json(r#"{"type":"text_delta","text":"\ud800\udfff\ud800"}"#).unwrap();
    guard.observe(&event, 0.0).unwrap();
    let error = guard.observe(&event, 0.0).unwrap_err();
    assert!(error.name().equals_str("RepeatedStreamingChunkError"));
    let mut expected: Vec<u16> = "The model is repeating the same chunk = "
        .encode_utf16()
        .collect();
    expected.extend([0xd800, 0xdfff, 0xd800]);
    expected.push(u16::from(b'.'));
    assert_eq!(error.message().units(), expected);
    let short = parse_json(r#"{"type":"text_delta","text":"😀"}"#).unwrap();
    for _ in 0..3 {
        guard.observe(&short, 0.0).unwrap();
    }
    guard.observe(&event, 0.0).unwrap();
    guard
        .observe(
            &parse_json(r#"{"type":"thinking_delta","text":"abc"}"#).unwrap(),
            0.0,
        )
        .unwrap();
    guard.observe(&event, 0.0).unwrap();
    assert!(matches!(
        guard.observe(&event, 0.0),
        Err(CodecError::Thrown { .. })
    ));
}
