//! Differential check of `stringify::EditableDocument` against yaml@2.9.0:
//! `fixtures/stringify-cases.json` is recorded by `record_stringify.mjs`
//! (parseDocument + setIn/deleteIn + toString under Node v22).
use pilotdeck_protocol::{JsString, JsValue, parse_json};
use pilotdeck_yaml::{
    Options, parse_document,
    stringify::{EditableDocument, StringifyError},
};

#[test]
fn edits_and_to_string_match_the_recorded_baseline() {
    let text = include_str!("fixtures/stringify-cases.json");
    let cases: serde_json::Value = serde_json::from_str(text).unwrap();
    let mut failures = Vec::new();
    let mut checked = 0;
    for case in cases.as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let src = case["src"].as_str().unwrap();
        let doc = parse_document(
            &JsString::from(src),
            Options {
                pretty_errors: false,
                ..Options::default()
            },
        );
        let mut ed = EditableDocument::new(doc);
        let mut result: Result<(), StringifyError> = Ok(());
        for op in case["ops"].as_array().unwrap() {
            let kind = op[0].as_str().unwrap();
            let path: Vec<&str> = op[1]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p.as_str().unwrap())
                .collect();
            // Re-read the value from the fixture text so object key order
            // (insertion order, as in JS) survives.
            let value = match op.get(2) {
                Some(v) => ordered_value(text, v),
                None => JsValue::Null,
            };
            result = if kind == "set" && !matches!(value, JsValue::Null) {
                ed.set_in(&path, &value)
            } else {
                ed.delete_in(&path).map(|_| ())
            };
            if result.is_err() {
                break;
            }
        }
        let actual = result.and_then(|()| ed.to_yaml_string());
        let label = format!("{name} {}", case["ops"]);
        checked += 1;
        match (&actual, case["output"].as_str(), case["error"].as_str()) {
            (Ok(out), Some(expected), _) if out == expected => {}
            (Err(StringifyError::Error(message)), _, Some(expected)) if message == expected => {}
            _ => failures.push(format!(
                "{label}\n  expected: {:?} / {:?}\n  actual:   {actual:?}",
                case["output"], case["error"]
            )),
        }
    }
    assert!(checked > 100, "only {checked} cases checked");
    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The JS-ordered value of an op argument: serde_json sorts object keys, so
/// objects are located in the fixture text and parsed with the JS parser.
fn ordered_value(text: &str, value: &serde_json::Value) -> JsValue {
    let all = parse_json(text).unwrap();
    let JsValue::Array(cases) = &all else {
        panic!("fixture root");
    };
    for case in cases {
        if let Some(JsValue::Array(ops)) = case.get("ops") {
            for op in ops {
                if let JsValue::Array(parts) = op
                    && let Some(candidate) = parts.get(2)
                    && same(candidate, value)
                {
                    return candidate.clone();
                }
            }
        }
    }
    panic!("value not found in fixture");
}

fn same(a: &JsValue, b: &serde_json::Value) -> bool {
    match (a, b) {
        (JsValue::Null, serde_json::Value::Null) => true,
        (JsValue::Bool(x), serde_json::Value::Bool(y)) => x == y,
        (JsValue::Number(x), serde_json::Value::Number(y)) => Some(*x) == y.as_f64(),
        (JsValue::String(x), serde_json::Value::String(y)) => x.equals_str(y),
        (JsValue::Object(x), serde_json::Value::Object(y)) => {
            x.entries().len() == y.len()
                && x.entries().iter().all(|(k, v)| {
                    k.to_utf8()
                        .ok()
                        .and_then(|k| y.get(&k))
                        .is_some_and(|w| same(v, w))
                })
        }
        _ => false,
    }
}
