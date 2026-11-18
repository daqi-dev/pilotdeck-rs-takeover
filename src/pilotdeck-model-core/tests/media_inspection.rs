use pilotdeck_model_core::request::{MediaInspectionError, inspect_request_media};
use pilotdeck_protocol::{JsObject, JsValue, parse_json};

fn json(text: &str) -> JsValue {
    parse_json(text).unwrap()
}
fn object(fields: &[(&str, JsValue)]) -> JsValue {
    let mut result = JsObject::new();
    for (key, value) in fields {
        result.insert((*key).into(), value.clone());
    }
    JsValue::Object(result)
}
fn request(blocks: Vec<JsValue>) -> JsValue {
    object(&[(
        "messages",
        JsValue::Array(vec![object(&[
            ("role", "user".into()),
            ("content", JsValue::Array(blocks)),
        ])]),
    )])
}
fn replace(value: &JsValue, key: &str, field: JsValue) -> JsValue {
    let mut result = value.as_object().unwrap().clone();
    result.insert(key.into(), field);
    JsValue::Object(result)
}
fn omit(value: &JsValue, key: &str) -> JsValue {
    let mut result = JsObject::new();
    for (name, field) in value.as_object().unwrap().entries() {
        if !name.equals_str(key) {
            result.insert(name.clone(), field.clone());
        }
    }
    JsValue::Object(result)
}
fn variants() -> Vec<(JsValue, bool, &'static [&'static str])> {
    vec![
        (
            json(r#"{"type":"text","text":"\ud800😀"}"#),
            false,
            &["type", "text"],
        ),
        (
            json(
                r#"{"type":"thinking","text":"reason","signature":"\udfff","reasoningContent":"x"}"#,
            ),
            false,
            &["type", "text"],
        ),
        (
            json(
                r#"{"type":"image","source":"url","data":"https://example.invalid/image","mimeType":"image/png"}"#,
            ),
            true,
            &["type", "source", "data", "mimeType"],
        ),
        (
            json(
                r#"{"type":"pdf","source":"base64","data":"YQ==","mimeType":"application/pdf","bytes":1}"#,
            ),
            true,
            &["type", "source", "data", "mimeType", "bytes"],
        ),
        (
            json(r#"{"type":"audio","source":"base64","data":"YQ==","mimeType":"audio/wav"}"#),
            true,
            &["type", "source", "data", "mimeType"],
        ),
        (
            json(r#"{"type":"tool_call","id":"call","name":"read"}"#),
            false,
            &["type", "id", "name"],
        ),
        (
            json(
                r#"{"type":"tool_result","toolCallId":"call","content":[{"type":"text","text":"answer"}]}"#,
            ),
            false,
            &["type", "toolCallId", "content"],
        ),
        (
            json(
                r#"{"type":"tool_result_reference","toolCallId":"call","path":"/unread/result","originalBytes":1,"preview":"text","hasMore":false,"mimeType":"text/plain"}"#,
            ),
            true,
            &[
                "type",
                "toolCallId",
                "path",
                "originalBytes",
                "preview",
                "hasMore",
            ],
        ),
        (
            json(
                r#"{"type":"media_reference","path":"/unread/media","originalBytes":1,"preview":"image","hasMore":true,"mimeType":"image/png","mediaType":"image"}"#,
            ),
            true,
            &[
                "type",
                "path",
                "originalBytes",
                "preview",
                "hasMore",
                "mimeType",
                "mediaType",
            ],
        ),
    ]
}
fn unsupported(value: &JsValue) -> String {
    let error =
        inspect_request_media(value).expect_err("unknown or malformed content must be rejected");
    let MediaInspectionError::Unsupported { path, .. } = &error;
    assert!(path.starts_with('$'));
    let display = error.to_string();
    assert!(display.contains(path));
    display
}

#[test]
fn every_canonical_content_variant_is_classified_without_mutating_input() {
    for (block, expected, _) in variants() {
        let value = request(vec![block]);
        let original = value.clone();
        assert_eq!(inspect_request_media(&value), Ok(expected), "{value:?}");
        assert_eq!(value, original);
    }
    assert_eq!(
        inspect_request_media(&json(r#"{"messages":[]}"#)),
        Ok(false)
    );
    assert_eq!(inspect_request_media(&request(vec![])), Ok(false));
}
#[test]
fn nested_tool_results_accept_only_text_image_and_pdf() {
    for (index, expected) in [(0, false), (2, true), (3, true)] {
        let nested = variants()[index].0.clone();
        let result = object(&[
            ("type", "tool_result".into()),
            ("toolCallId", "call".into()),
            ("content", JsValue::Array(vec![nested])),
        ]);
        assert_eq!(inspect_request_media(&request(vec![result])), Ok(expected));
    }
    for index in [1, 4, 5, 6, 7, 8] {
        let result = object(&[
            ("type", "tool_result".into()),
            ("toolCallId", "call".into()),
            ("content", JsValue::Array(vec![variants()[index].0.clone()])),
        ]);
        assert!(unsupported(&request(vec![result])).contains("content[0].content[0].type"));
    }
}
#[test]
fn missing_or_wrong_required_fields_are_not_classified_as_text() {
    for (block, _, required) in variants() {
        for key in required {
            unsupported(&request(vec![omit(&block, key)]));
            unsupported(&request(vec![replace(&block, key, JsValue::Null)]));
            unsupported(&request(vec![replace(&block, key, JsValue::Undefined)]));
        }
    }
    for raw in [
        "null",
        "[]",
        "{}",
        r#"{"messages":null}"#,
        r#"{"messages":[null]}"#,
        r#"{"messages":[{"role":"tool","content":[]}]}"#,
        r#"{"messages":[{"role":"user"}]}"#,
        r#"{"messages":[{"role":"user","content":"text"}]}"#,
    ] {
        unsupported(&json(raw));
    }
    unsupported(&JsValue::Undefined);
}
#[test]
fn declared_optional_fields_and_discriminators_follow_strict_content_reader() {
    for (index, key) in [
        (0, "blockId"),
        (1, "signature"),
        (1, "reasoningContent"),
        (2, "bytes"),
        (2, "detail"),
        (3, "pages"),
        (4, "durationSeconds"),
        (6, "isError"),
        (7, "readFilePath"),
        (8, "toolCallId"),
        (8, "pages"),
        (8, "reason"),
    ] {
        let (block, expected, _) = &variants()[index];
        assert_eq!(
            inspect_request_media(&request(vec![replace(block, key, JsValue::Undefined)])),
            Ok(*expected)
        );
        unsupported(&request(vec![replace(block, key, JsValue::Null)]));
    }
    for (index, key, wrong) in [
        (2, "source", "file"),
        (2, "detail", "medium"),
        (3, "source", "url"),
        (3, "mimeType", "text/plain"),
        (4, "source", "file"),
        (8, "mediaType", "video"),
    ] {
        unsupported(&request(vec![replace(
            &variants()[index].0,
            key,
            wrong.into(),
        )]));
    }
    let timeline =
        json(r#"{"version":1,"turnId":"turn","id":"part","order":0,"revision":1,"offset":2}"#);
    let block = replace(&variants()[0].0, "timeline", timeline.clone());
    assert_eq!(inspect_request_media(&request(vec![block])), Ok(false));
    unsupported(&request(vec![replace(
        &variants()[0].0,
        "timeline",
        replace(&timeline, "version", JsValue::Number(2.0)),
    )]));
    unsupported(&request(vec![replace(
        &variants()[2].0,
        "timeline",
        JsValue::Null,
    )]));
}
#[test]
fn opaque_metadata_schema_tool_input_and_unknown_extensions_are_not_content() {
    let value = json(
        r#"{
      "messages":[{"role":"assistant","content":[
        {"type":"text","text":"\ud800","extension":{"type":"image"}},
        {"type":"tool_call","id":"call","name":"tool","input":{"type":"image","content":[{"type":"unknown"}]},"raw":{"type":"media_reference"}}
      ],"metadata":{"type":"unknown","content":[{"type":"audio"}]}}],
      "metadata":{"messages":[{"content":[{"type":"image"}]}]},
      "tools":[{"inputSchema":{"type":"pdf"}}],
      "outputSchema":{"schema":{"type":"media_reference"}},
      "extension":{"type":"tool_result_reference"}
    }"#,
    );
    let original = value.clone();
    assert_eq!(inspect_request_media(&value), Ok(false));
    assert_eq!(value, original);
    // The detector does not claim to normalize or validate message metadata.
    assert_eq!(
        inspect_request_media(&json(
            r#"{"messages":[{"role":"user","content":[],"metadata":42}]}"#
        )),
        Ok(false)
    );
}
#[test]
fn negative_controls_reject_unknown_content_even_after_media_was_found() {
    let media = variants()[2].0.clone();
    let unknown = json(r#"{"type":"future_media","data":"payload"}"#);
    for blocks in [
        vec![unknown.clone()],
        vec![media.clone(), unknown.clone()],
        vec![unknown.clone(), media.clone()],
    ] {
        unsupported(&request(blocks));
    }
    unsupported(&json(
        r#"{"messages":[{"role":"user","content":[{"type":"image","source":"url","data":"https://example.invalid","mimeType":"image/png"}]},{"role":"assistant","content":[{"type":"future_media"}]}]}"#,
    ));
    for blocks in [vec![media.clone(), unknown.clone()], vec![unknown, media]] {
        let result = object(&[
            ("type", "tool_result".into()),
            ("toolCallId", "call".into()),
            ("content", JsValue::Array(blocks)),
        ]);
        unsupported(&request(vec![result]));
    }
    // A plausible "known tag => media" shortcut must also reject malformed media.
    unsupported(&request(vec![json(r#"{"type":"image"}"#)]));
    // Lax BoundaryOnly may retain these values; inspection must never return false.
    for block in [
        json("[]"),
        json("null"),
        json("1"),
        json(r#"{"type":"text","text":3}"#),
    ] {
        unsupported(&request(vec![block]));
    }
}
