//! Shared pure provider message, tool and schema projections.
use super::error::BodyError;
use pilotdeck_protocol::{JsObject, JsString, JsValue, stringify_json};

static UNDEFINED: JsValue = JsValue::Undefined;

pub fn field<'a>(value: &'a JsValue, key: &str) -> &'a JsValue {
    value.get(key).unwrap_or(&UNDEFINED)
}
pub fn object<const N: usize>(entries: [(&str, JsValue); N]) -> JsValue {
    let mut result = JsObject::new();
    for (key, value) in entries {
        if !value.is_undefined() {
            result.insert(key.into(), value);
        }
    }
    JsValue::Object(result)
}
pub fn array(value: &JsValue) -> &[JsValue] {
    if let JsValue::Array(values) = value {
        values
    } else {
        &[]
    }
}
pub fn is(value: &JsValue, text: &str) -> bool {
    value.as_string().is_some_and(|s| s.equals_str(text))
}
pub fn text(value: &JsValue) -> String {
    String::from_utf16_lossy(&super::js::to_string_units(value).unwrap_or_default())
}
pub fn truthy(value: &JsValue) -> bool {
    match value {
        JsValue::Undefined | JsValue::Null => false,
        JsValue::Bool(v) => *v,
        JsValue::Number(v) => *v != 0.0 && !v.is_nan(),
        JsValue::String(v) => !v.is_empty(),
        _ => true,
    }
}
pub fn present(value: &JsValue) -> bool {
    !matches!(value, JsValue::Undefined | JsValue::Null)
}
pub fn put(value: &mut JsValue, key: &str, item: JsValue) {
    if let JsValue::Object(object) = value {
        object.insert(key.into(), item);
    }
}
pub fn cache_control() -> JsValue {
    object([("type", "ephemeral".into()), ("ttl", "5m".into())])
}

pub fn budget_only_model(model_id: &str) -> bool {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN
        .get_or_init(|| {
            regex::Regex::new(
                r"claude-(?:3|haiku-3|haiku-4|(?:sonnet|opus)-4(?:[.-][0-5](?:[^0-9]|$)|-20|$))",
            )
            .expect("static regex")
        })
        .is_match(model_id)
}
pub fn tool_choice(choice: &JsValue, anthropic: bool) -> JsValue {
    if !truthy(choice) {
        return JsValue::Undefined;
    }
    for name in ["auto", "none", "required"] {
        if is(choice, name) {
            return if anthropic {
                object([("type", if name == "required" { "any" } else { name }.into())])
            } else {
                choice.clone()
            };
        }
    }
    object([
        ("type", if anthropic { "tool" } else { "function" }.into()),
        ("name", field(choice, "name").clone()),
    ])
}
pub fn anthropic_block(block: &JsValue) -> JsValue {
    let tag = text(field(block, "type"));
    match tag.as_str() {
        "text" => object([
            ("type", "text".into()),
            ("text", field(block, "text").clone()),
        ]),
        "thinking" => object([
            ("type", "thinking".into()),
            ("thinking", field(block, "text").clone()),
            (
                "signature",
                if truthy(field(block, "signature")) {
                    field(block, "signature").clone()
                } else {
                    JsValue::Undefined
                },
            ),
        ]),
        "image" | "pdf" | "audio" => {
            let source = if tag != "pdf" && is(field(block, "source"), "url") {
                object([
                    ("type", "url".into()),
                    ("url", field(block, "data").clone()),
                ])
            } else {
                object([
                    ("type", "base64".into()),
                    ("media_type", field(block, "mimeType").clone()),
                    ("data", field(block, "data").clone()),
                ])
            };
            object([
                (
                    "type",
                    if tag == "pdf" {
                        "document"
                    } else {
                        tag.as_str()
                    }
                    .into(),
                ),
                ("source", source),
            ])
        }
        "tool_call" => object([
            ("type", "tool_use".into()),
            ("id", field(block, "id").clone()),
            ("name", field(block, "name").clone()),
            ("input", field(block, "input").clone()),
        ]),
        "tool_result" | "tool_result_reference" => object([
            ("type", "tool_result".into()),
            ("tool_use_id", field(block, "toolCallId").clone()),
            (
                "content",
                JsValue::Array(if tag == "tool_result" {
                    array(field(block, "content"))
                        .iter()
                        .map(|b| {
                            if ["text", "image", "pdf"]
                                .iter()
                                .any(|tag| is(field(b, "type"), tag))
                            {
                                anthropic_block(b)
                            } else {
                                JsValue::Undefined
                            }
                        })
                        .collect()
                } else {
                    vec![object([
                        ("type", "text".into()),
                        ("text", reference_text(block)),
                    ])]
                }),
            ),
            ("is_error", field(block, "isError").clone()),
        ]),
        "media_reference" => object([
            ("type", "text".into()),
            ("text", field(block, "preview").clone()),
        ]),
        _ => JsValue::Undefined,
    }
}
pub fn concatenate(parts: &[JsValue]) -> JsValue {
    let mut units = Vec::new();
    for part in parts {
        units.extend(super::js::to_string_units(part).unwrap_or_default());
    }
    JsValue::String(JsString::from_utf16(units))
}
pub fn reference_text(block: &JsValue) -> JsValue {
    if !truthy(field(block, "hasMore")) {
        return field(block, "preview").clone();
    }
    let path = if present(field(block, "readFilePath")) {
        field(block, "readFilePath")
    } else {
        field(block, "path")
    };
    concatenate(&[
        field(block, "preview").clone(), "\n\n[Tool result preview only: original ".into(), field(block, "originalBytes").clone(),
        " bytes. Full output was saved at: ".into(), path.clone(),
        ". To inspect it, first search within the persisted result with grep({ pattern: \"<keyword>\", path: \"".into(), path.clone(),
        "\", output_mode: \"content\", head_limit: 20 }) for relevant names, IDs, errors, URLs, dates, or candidate terms. Then read only the matching neighborhood with read_file({ file_path: \"".into(), path.clone(),
        "\", offset: <line>, limit: 80 }) when surrounding context is needed. Avoid paging through the whole file from offset 1 unless the task truly requires a complete sequential review. If the task depends on complete lists, counts, search candidates, evidence checks, or long page content, use grep/refined searches and targeted reads on the persisted result before concluding.]".into(),
    ])
}
pub fn responses_input(request: &JsValue) -> Result<Vec<JsValue>, BodyError> {
    let mut input = Vec::new();
    for message in array(field(request, "messages")) {
        let mut content = Vec::new();
        for block in array(field(message, "content")) {
            let tag = text(field(block, "type"));
            if ["tool_call", "tool_result", "tool_result_reference"].contains(&tag.as_str()) {
                flush(&mut input, &mut content, field(message, "role"));
                if tag == "tool_call" {
                    let args = field(block, "input");
                    let args = if present(args) {
                        args.clone()
                    } else {
                        object([])
                    };
                    input.push(object([
                        ("type", "function_call".into()),
                        ("call_id", field(block, "id").clone()),
                        ("name", field(block, "name").clone()),
                        (
                            "arguments",
                            stringify_json(&args)
                                .map_err(|e| BodyError::new(e.to_string()))?
                                .unwrap_or_default()
                                .as_str()
                                .into(),
                        ),
                    ]));
                } else {
                    let output = if tag == "tool_result_reference" {
                        reference_text(block)
                    } else {
                        flatten_result(block)?
                    };
                    input.push(object([
                        ("type", "function_call_output".into()),
                        ("call_id", field(block, "toolCallId").clone()),
                        ("output", output),
                    ]));
                    let visual: Vec<_> = (if tag == "tool_result" {
                        array(field(block, "content"))
                    } else {
                        &[]
                    })
                    .iter()
                    .filter(|b| is(field(b, "type"), "image") || is(field(b, "type"), "pdf"))
                    .map(responses_part)
                    .collect();
                    if !visual.is_empty() {
                        let mut parts = vec![object([
                            ("type", "input_text".into()),
                            ("text", "[Visual content from tool result]".into()),
                        ])];
                        parts.extend(visual);
                        input.push(object([
                            ("role", "user".into()),
                            ("content", JsValue::Array(parts)),
                        ]));
                    }
                }
            } else {
                let part = responses_part(block);
                if !part.is_undefined() {
                    content.push(part);
                }
            }
        }
        flush(&mut input, &mut content, field(message, "role"));
    }
    Ok(input)
}
fn flush(input: &mut Vec<JsValue>, content: &mut Vec<JsValue>, role: &JsValue) {
    if !content.is_empty() {
        input.push(object([
            ("role", role.clone()),
            ("content", JsValue::Array(std::mem::take(content))),
        ]));
    }
}
fn responses_part(block: &JsValue) -> JsValue {
    let tag = text(field(block, "type"));
    match tag.as_str() {
        "text" | "thinking" | "media_reference" => object([
            ("type", "input_text".into()),
            (
                "text",
                field(
                    block,
                    if tag == "media_reference" {
                        "preview"
                    } else {
                        "text"
                    },
                )
                .clone(),
            ),
        ]),
        "image" => object([
            ("type", "input_image".into()),
            (
                "image_url",
                if is(field(block, "source"), "url") {
                    field(block, "data").clone()
                } else {
                    data_url(block)
                },
            ),
            ("detail", field(block, "detail").clone()),
        ]),
        "pdf" => object([
            ("type", "input_file".into()),
            ("filename", "document.pdf".into()),
            ("file_data", data_url(block)),
        ]),
        "audio" => object([
            ("type", "input_text".into()),
            (
                "text",
                if is(field(block, "source"), "url") {
                    concatenate(&[
                        "[Audio URL: ".into(),
                        field(block, "data").clone(),
                        "]".into(),
                    ])
                } else {
                    "[Audio content omitted]".into()
                },
            ),
        ]),
        _ => JsValue::Undefined,
    }
}
fn data_url(block: &JsValue) -> JsValue {
    concatenate(&[
        "data:".into(),
        field(block, "mimeType").clone(),
        ";base64,".into(),
        field(block, "data").clone(),
    ])
}
pub fn normalize_schema(value: &JsValue) -> Result<JsValue, BodyError> {
    super::json_walk::transform(value, false, |raw| {
        let mut normalized = JsObject::new();
        for (key, value) in raw.entries() {
            if key.equals_str("__proto__") {
                if matches!(
                    value,
                    JsValue::Object(_) | JsValue::Array(_) | JsValue::Null
                ) {
                    normalized.set_prototype(value.clone());
                }
            } else {
                normalized.insert(key.clone(), value.clone());
            }
        }
        if normalized.get_property("type").is_none() {
            let kind = if let Some(value) = normalized.get_property("const") {
                literal_type(value)
            } else {
                let types: Vec<_> = array(normalized.get_property("enum").unwrap_or(&UNDEFINED))
                    .iter()
                    .map(literal_type)
                    .collect();
                if types.is_empty() || types.iter().any(Option::is_none) {
                    None
                } else if types.iter().all(|t| *t == types[0]) {
                    types[0]
                } else if types
                    .iter()
                    .all(|t| matches!(t, Some("integer" | "number")))
                {
                    Some("number")
                } else {
                    None
                }
            };
            if let Some(kind) = kind {
                normalized.insert("type".into(), kind.into());
            }
        }
        let kind = normalized.get_property("type").unwrap_or(&UNDEFINED);
        if (is(kind, "array") || array(kind).iter().any(|k| is(k, "array")))
            && normalized.get_property("items").is_none()
        {
            normalized.insert("items".into(), object([]));
        }
        normalized
    })
}
fn literal_type(value: &JsValue) -> Option<&'static str> {
    match value {
        JsValue::String(_) => Some("string"),
        JsValue::Bool(_) => Some("boolean"),
        JsValue::Number(n) if n.is_finite() => Some(if n.fract() == 0.0 {
            "integer"
        } else {
            "number"
        }),
        JsValue::Null => Some("null"),
        JsValue::Array(_) => Some("array"),
        JsValue::Object(_) => Some("object"),
        _ => None,
    }
}

fn accounting_result(block: &JsValue) -> Result<JsValue, BodyError> {
    let tag = property(Some(block), "type")?
        .and_then(JsValue::as_string)
        .and_then(|s| s.to_utf8().ok());
    Ok(match tag.as_deref() {
        Some("text") => block.get("text").cloned().unwrap_or(JsValue::Undefined),
        Some("image" | "pdf") => {
            let pdf = tag.as_deref() == Some("pdf");
            let mut units: Vec<u16> = (if pdf { "[PDF: " } else { "[Image: " })
                .encode_utf16()
                .collect();
            units.extend_from_slice(template_string(block.get("mimeType"))?.units());
            units.extend(", ".encode_utf16());
            if !pdf
                && block
                    .get("source")
                    .and_then(JsValue::as_string)
                    .is_some_and(|value| value.equals_str("url"))
            {
                units.extend_from_slice(template_string(block.get("data"))?.units());
            } else {
                units
                    .extend_from_slice(template_string(Some(&length(block.get("data"))?))?.units());
                units.extend(" base64 characters".encode_utf16());
            }
            if pdf && block.get("pages").is_some_and(truthy) {
                units.extend(", ".encode_utf16());
                units.extend_from_slice(template_string(block.get("pages"))?.units());
                units.extend(" pages".encode_utf16());
            }
            units.push(u16::from(b']'));
            JsValue::String(JsString::from_utf16(units))
        }
        _ => JsValue::Undefined,
    })
}
pub fn flatten_result(block: &JsValue) -> Result<JsValue, BodyError> {
    let content = block.get("content");
    property(content, "map")?; // Nullish receivers throw before the call.
    let Some(JsValue::Array(items)) = content else {
        return Err(BodyError::new("content.map is not a function"));
    };
    let mapped = items
        .iter()
        .map(accounting_result)
        .collect::<Result<Vec<_>, _>>()?;
    let mut units = Vec::new();
    for (index, value) in mapped.iter().enumerate() {
        if index > 0 {
            units.push(u16::from(b'\n'));
        }
        if !matches!(value, JsValue::Null | JsValue::Undefined) {
            units.extend_from_slice(template_string(Some(value))?.units());
        }
    }
    Ok(JsValue::String(JsString::from_utf16(units)))
}
fn template_string(value: Option<&JsValue>) -> Result<JsString, BodyError> {
    let undefined = JsValue::Undefined;
    let value = value.unwrap_or(&undefined);
    check_primitive_conversion(value)?;
    super::js::to_string_units(value)
        .map(JsString::from_utf16)
        .map_err(|_| conversion_error())
}
// Wire JSON cannot contain callable properties. An own toString shadows the
// inherited conversion method; valueOf cannot supply a primitive either.
// Array#join visits elements, but ordinary object properties are not visited.
fn check_primitive_conversion(value: &JsValue) -> Result<(), BodyError> {
    let mut frames = vec![std::slice::from_ref(value).iter()];
    while let Some(frame) = frames.last_mut() {
        match frame.next() {
            Some(JsValue::Object(object)) if object.get("toString").is_some() => {
                return Err(BodyError::new("Cannot convert object to primitive value"));
            }
            Some(JsValue::Array(items)) => {
                if frames.len() > super::js::MAX_ARRAY_DEPTH {
                    return Err(conversion_error());
                }
                frames.push(items.iter());
            }
            Some(_) => {}
            None => {
                frames.pop();
            }
        }
    }
    Ok(())
}
fn conversion_error() -> BodyError {
    BodyError::new(super::js::StackOverflow::MESSAGE)
}
fn property<'a>(value: Option<&'a JsValue>, field: &str) -> Result<Option<&'a JsValue>, BodyError> {
    match value {
        None | Some(JsValue::Undefined | JsValue::Null) => {
            let receiver = if matches!(value, Some(JsValue::Null)) {
                "null"
            } else {
                "undefined"
            };
            Err(BodyError::new(format!(
                "Cannot read properties of {receiver} (reading '{field}')"
            )))
        }
        Some(value) => Ok(value.get(field)),
    }
}
fn length(value: Option<&JsValue>) -> Result<JsValue, BodyError> {
    Ok(match value {
        Some(JsValue::String(text)) => JsValue::Number(text.len_utf16() as f64),
        Some(JsValue::Array(items)) => JsValue::Number(items.len() as f64),
        _ => property(value, "length")?
            .cloned()
            .unwrap_or(JsValue::Undefined),
    })
}
pub fn flatten_tool_result_block_text(block: &JsValue) -> JsString {
    let items = match field(block, "content") {
        JsValue::Array(items) => items.as_slice(),
        _ => return JsString::default(),
    };
    let mut units: Vec<u16> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            units.push(u16::from(b'\n'));
        }
        match field(item, "type")
            .as_string()
            .and_then(|kind| kind.to_utf8().ok())
            .as_deref()
        {
            Some("text") => {
                if let Some(text) = string_field(item, "text") {
                    units.extend_from_slice(text.units());
                }
            }
            Some("image") => {
                let mime = string_field(item, "mimeType").cloned().unwrap_or_default();
                let data = string_field(item, "data").cloned().unwrap_or_default();
                units.extend("[Image: ".encode_utf16());
                units.extend_from_slice(mime.units());
                units.extend(", ".encode_utf16());
                if string_field(item, "source").is_some_and(|source| source.equals_str("url")) {
                    units.extend_from_slice(data.units());
                } else {
                    units.extend(data.len_utf16().to_string().encode_utf16());
                    units.extend(" base64 characters".encode_utf16());
                }
                units.push(u16::from(b']'));
            }
            Some("pdf") => {
                let mime = string_field(item, "mimeType").cloned().unwrap_or_default();
                let data = string_field(item, "data").cloned().unwrap_or_default();
                units.extend("[PDF: ".encode_utf16());
                units.extend_from_slice(mime.units());
                units.extend(", ".encode_utf16());
                units.extend(data.len_utf16().to_string().encode_utf16());
                units.extend(" base64 characters".encode_utf16());
                let pages = field(item, "pages");
                if truthy(pages) {
                    units.extend(", ".encode_utf16());
                    units.extend_from_slice(&super::js::to_string_units(pages).unwrap_or_default());
                    units.extend(" pages".encode_utf16());
                }
                units.push(u16::from(b']'));
            }
            _ => {}
        }
    }
    JsString::from_utf16(units)
}

fn string_field<'a>(value: &'a JsValue, key: &str) -> Option<&'a JsString> {
    value.get(key).and_then(JsValue::as_string)
}
