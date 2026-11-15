//! Replay the unchanged TS oracle used by Gateway's compaction_engine test.
use pilotdeck_context_runtime::compaction::strip_multimedia::strip_multimedia_from_messages;
use pilotdeck_protocol::{JsValue, parse_json, stringify_json};

#[test]
fn deterministic_strip_replays_ts_oracle() {
    let oracle = parse_json(include_str!(
        "../../pilotdeck-gateway/tests/fixtures/compaction-engine/oracle.json"
    ))
    .unwrap();
    assert_eq!(
        oracle.get("sourceCommit"),
        Some(&JsValue::from("cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85"))
    );
    let Some(JsValue::Array(cases)) = oracle.get("stripCases") else {
        panic!("missing stripCases")
    };
    assert_eq!(cases.len(), 1);
    for case in cases {
        let Some(JsValue::Array(input)) = case.get("input") else {
            panic!("missing input")
        };
        let actual = JsValue::Array(strip_multimedia_from_messages(input));
        assert_eq!(
            stringify_json(&actual),
            stringify_json(case.get("expected").expect("missing expected"))
        );
    }
}
