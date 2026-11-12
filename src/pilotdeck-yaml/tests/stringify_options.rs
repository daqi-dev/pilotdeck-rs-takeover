//! Fixed-baseline yaml@2.9.0 output for gateway setup's saveYamlConfig options.
//! The recorder exercises new object documents and existing scalar/flow styles.
use pilotdeck_protocol::{JsString, JsValue, parse_json};
use pilotdeck_yaml::{
    Options, parse_document,
    stringify::{EditableDocument, StringifyOptions},
};

#[test]
fn configurable_output_matches_the_recorded_baseline() {
    let fixture = include_str!("fixtures/stringify-options.json");
    let expected: serde_json::Value = serde_json::from_str(fixture).unwrap();
    let ordered = parse_json(fixture).unwrap();
    let Some(JsValue::Array(ordered_cases)) = ordered.get("cases") else {
        panic!("ordered fixture cases");
    };
    assert_eq!(expected["yaml"], "2.9.0");
    let mut checked = 0;
    let mut option_sensitive = 0;
    for (case, ordered_case) in expected["cases"]
        .as_array()
        .unwrap()
        .iter()
        .zip(ordered_cases)
    {
        let name = case["name"].as_str().unwrap();
        let source = case["src"].as_str().unwrap_or("");
        let parsed = parse_document(&JsString::from(source), Options::default());
        assert!(parsed.errors.is_empty(), "{name}: input parse failed");
        let mut document = EditableDocument::new(parsed);
        if let Some(value) = ordered_case.get("value") {
            document.set_in(&[], value).unwrap();
        }
        let options = StringifyOptions {
            line_width: case["options"]["lineWidth"]
                .as_u64()
                .map_or(80, |n| n as usize),
            single_quote: case["options"]["singleQuote"].as_bool(),
        };
        let actual = document.to_yaml_string_with_options(options).unwrap();
        assert_eq!(actual, case["output"].as_str().unwrap(), "{name}");
        let default = document.to_yaml_string().unwrap();
        assert_eq!(
            default,
            document
                .to_yaml_string_with_options(StringifyOptions::default())
                .unwrap(),
            "{name}: default API changed"
        );
        if actual != default {
            option_sensitive += 1;
        }
        checked += 1;
    }
    assert_eq!(checked, 70, "every recorded sample must run");
    assert!(
        option_sensitive >= 20,
        "fixtures must reject a printer that ignores options"
    );
}
