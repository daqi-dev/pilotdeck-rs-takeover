//! Keep the original reverse-key JSON text, rather than a JSON.stringify'd
//! object that already has JavaScript property order before Rust parses it.
use pilotdeck_protocol::{JsString, parse_json};
use pilotdeck_yaml::{
    Options, parse_document,
    stringify::{EditableDocument, StringifyOptions},
};

#[test]
fn raw_object_keys_match_fixed_yaml_enumeration_without_reordering_parsed_maps() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/stringify-key-order.json")).unwrap();
    assert_eq!(
        fixture["baseline"],
        "cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85"
    );
    assert_eq!(fixture["node"], "v22.22.0");
    assert_eq!(fixture["yaml"], "2.9.0");
    let cases = fixture["cases"]
        .as_array()
        .expect("key-order fixture cases");
    assert_eq!(cases.len(), 13, "all key-order cells must run");

    for case in cases {
        let name = case["name"].as_str().unwrap();
        let raw_json = case["raw_json"]
            .as_str()
            .expect("unmodified raw JSON input");
        let value = parse_json(raw_json).expect("raw key-order JSON");
        if name == "descending-top/default" {
            // Guard against accidentally normalizing the input in the test
            // recorder/parser. The pre-fix entries() loop starts with 10;
            // the recorded baseline output starts with 1 and must reject it.
            assert!(value.as_object().unwrap().entries()[0].0.equals_str("10"));
            assert!(case["output"].as_str().unwrap().starts_with("\"1\": one\n"));
        }
        let source = case["src"].as_str().unwrap();
        let parsed = parse_document(&JsString::from(source), Options::default());
        assert!(parsed.errors.is_empty(), "{name}: source must parse");
        let mut document = EditableDocument::new(parsed);
        let path: Vec<&str> = case["path"]
            .as_array()
            .unwrap()
            .iter()
            .map(|part| part.as_str().unwrap())
            .collect();
        document.set_in(&path, &value).unwrap();
        let output = document
            .to_yaml_string_with_options(StringifyOptions {
                line_width: case["options"]["lineWidth"].as_u64().unwrap_or(80) as usize,
                single_quote: case["options"]["singleQuote"].as_bool(),
            })
            .unwrap();
        assert_eq!(output, case["output"].as_str().unwrap(), "{name}");
    }
}
