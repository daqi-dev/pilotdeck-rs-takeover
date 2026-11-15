//! Original TS compaction-orchestrator recovery cases and expected results.
use pilotdeck_context_runtime::compaction::{
    ports::CompactionRecoveryPort, recovery::ContextOverflowRecovery,
};
use pilotdeck_protocol::{JsValue, parse_json, stringify_json};
static UNDEFINED: JsValue = JsValue::Undefined;
fn field<'a>(value: &'a JsValue, key: &str) -> &'a JsValue {
    value.get(key).unwrap_or(&UNDEFINED)
}
fn array(value: &JsValue) -> &[JsValue] {
    match value {
        JsValue::Array(items) => items,
        _ => panic!("expected array"),
    }
}
#[test]
fn recovery_replays_ts_oracle() {
    let oracle = parse_json(include_str!(
        "../../pilotdeck-gateway/tests/fixtures/compaction-orchestrator/oracle.json"
    ))
    .unwrap();
    assert_eq!(
        oracle.get("sourceCommit"),
        Some(&JsValue::from("cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85"))
    );
    let cases = array(field(&oracle, "recoveryCases"));
    assert_eq!(cases.len(), 41);
    for case in cases {
        let options = field(case, "options");
        let recovery = ContextOverflowRecovery {
            truncate_first_keep_ratio: field(options, "truncateFirstKeepRatio")
                .as_number()
                .unwrap_or(0.5),
            truncate_second_keep_ratio: field(options, "truncateSecondKeepRatio")
                .as_number()
                .unwrap_or(0.25),
        };
        let actual = recovery.decide(field(case, "input").clone()).unwrap();
        assert_eq!(
            stringify_json(&actual),
            stringify_json(field(case, "expected"))
        );
    }
    let cases = array(field(&oracle, "malformedRecoveryCases"));
    assert_eq!(cases.len(), 3);
    for case in cases {
        let error = ContextOverflowRecovery::default()
            .decide(field(case, "input").clone())
            .unwrap_err();
        let actual = JsValue::String(error.module_error.0);
        assert_eq!(
            stringify_json(&actual),
            stringify_json(field(case, "expected"))
        );
    }
}
