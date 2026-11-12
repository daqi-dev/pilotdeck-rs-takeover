use crate::channel::sms;
use pilotdeck_protocol::{JsObject, JsString, JsValue};
pub const DELIVERABLE_HINT: &str = "[WeCom attachment hint: If the user explicitly asks you to send a generated file to this WeCom chat, save it to an absolute local path and include MEDIA:/absolute/path in your final answer. Do not use MEDIA for files that were only read or analyzed internally.]";
pub fn object(entries: impl IntoIterator<Item = (&'static str, JsValue)>) -> JsValue {
    let mut obj = JsObject::new();
    for (k, v) in entries {
        obj.insert(k.into(), v);
    }
    JsValue::Object(obj)
}
pub fn string(value: &JsValue, key: &str) -> JsString {
    sms::js_string_of(
        value
            .get(key)
            .filter(|v| !sms::nullish(v))
            .unwrap_or(&JsValue::String("".into())),
    )
}
pub fn trim(value: JsString) -> JsString {
    JsString::from_utf16(sms::trim_units(value.units()).to_vec())
}
pub fn lower(value: &JsString) -> String {
    String::from_utf16_lossy(value.units()).to_lowercase()
}
pub fn list(value: Option<&JsValue>) -> Vec<JsString> {
    match value {
        Some(JsValue::Array(items)) => items
            .iter()
            .map(sms::js_string_of)
            .map(trim)
            .filter(|v| !v.is_empty())
            .collect(),
        Some(JsValue::String(s)) => s
            .units()
            .split(|unit| *unit == 44)
            .map(|units| trim(JsString::from_utf16(units.to_vec())))
            .filter(|v| !v.is_empty())
            .collect(),
        _ => vec![],
    }
}
fn lower_units(value: &JsString) -> JsString {
    let mut output = Vec::new();
    let mut run = String::new();
    for decoded in std::char::decode_utf16(value.units().iter().copied()) {
        match decoded {
            Ok(c) => run.push(c),
            Err(error) => {
                output.extend(run.to_lowercase().encode_utf16());
                run.clear();
                output.push(error.unpaired_surrogate());
            }
        }
    }
    output.extend(run.to_lowercase().encode_utf16());
    JsString::from_utf16(output)
}
pub fn equal_folded(left: &JsString, right: &JsString) -> bool {
    lower_units(left) == lower_units(right)
}
pub fn entry_matches(entries: &[JsString], value: &JsString) -> bool {
    let normalized = |value: &JsString| {
        let value = lower_units(&trim(value.clone()));
        let units = value.units();
        for prefix in ["wecom:user:", "wecom:group:"] {
            let prefix = prefix.encode_utf16().collect::<Vec<_>>();
            if units.starts_with(&prefix) {
                return JsString::from_utf16(units[prefix.len()..].to_vec());
            }
        }
        value
    };
    let value = normalized(value);
    entries.iter().any(|entry| {
        let entry = normalized(entry);
        entry.equals_str("*") || entry == value
    })
}

pub fn message_text(body: &JsValue) -> JsString {
    let kind = lower(&string(body, "msgtype"));
    let mut parts = Vec::new();
    if kind == "mixed" {
        if let Some(JsValue::Array(items)) = body.get("mixed").and_then(|m| m.get("msg_item")) {
            for item in items {
                if lower(&string(item, "msgtype")) == "text" {
                    if let Some(text) = item.get("text") {
                        let c = trim(string(text, "content"));
                        if !c.is_empty() {
                            parts.push(c);
                        }
                    }
                }
            }
        }
    } else {
        if let Some(text) = body.get("text") {
            let c = trim(string(text, "content"));
            if !c.is_empty() {
                parts.push(c);
            }
        }
        if kind == "voice" || kind == "appmsg" {
            if let Some(value) = body.get(&kind) {
                let c = trim(string(
                    value,
                    if kind == "voice" { "content" } else { "title" },
                ));
                if !c.is_empty() {
                    parts.push(c);
                }
            }
        }
    }
    if parts.is_empty() {
        if let Some(quote) = body.get("quote") {
            let kind = lower(&string(quote, "msgtype"));
            if kind == "text" || kind == "voice" {
                if let Some(value) = quote.get(&kind) {
                    return trim(string(value, "content"));
                }
            }
        }
    }
    let mut result = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            result.push(10);
        }
        result.extend_from_slice(part.units());
    }
    trim(JsString::from_utf16(result))
}
pub fn strip_mention(text: &JsString) -> JsString {
    // JS /^@\S+\s*/ removes a leading mention token only.
    let units = text.units();
    if units.first() != Some(&64) {
        return trim(text.clone());
    }
    let end = units
        .iter()
        .position(|u| sms::trim_units(&[*u]).is_empty())
        .unwrap_or(units.len());
    if end <= 1 {
        return trim(text.clone());
    }
    trim(JsString::from_utf16(units[end..].to_vec()))
}
pub fn markdown(text: &JsString) -> JsValue {
    object([
        ("msgtype", JsValue::from("markdown")),
        (
            "markdown",
            object([(
                "content",
                JsValue::String(JsString::from_utf16(
                    text.units()[..text.units().len().min(4000)].to_vec(),
                )),
            )]),
        ),
    ])
}
pub fn response_error(response: &JsValue) -> Option<String> {
    let body = response.get("body");
    let code = body
        .and_then(|v| v.get("errcode"))
        .filter(|v| !sms::nullish(v))
        .or_else(|| response.get("errcode"));
    if code.is_none() || matches!(code,Some(JsValue::Number(v)) if *v==0.0) {
        return None;
    }
    let msg = body
        .and_then(|v| v.get("errmsg"))
        .filter(|v| !sms::nullish(v))
        .or_else(|| response.get("errmsg"))
        .filter(|v| !sms::nullish(v))
        .unwrap_or(&JsValue::String("error".into()))
        .clone();
    Some(format!(
        "WeCom errcode {}: {}",
        String::from_utf16_lossy(sms::js_string_of(code.unwrap()).units()),
        String::from_utf16_lossy(sms::js_string_of(&msg).units())
    ))
}

pub fn render(event: &JsValue) -> Option<JsString> {
    if event
        .get("type")
        .and_then(JsValue::as_string)
        .is_some_and(|kind| kind.equals_str("tool_call_finished"))
    {
        if event.get("ok").is_some_and(sms::truthy) {
            return Some("".into());
        }
        let name = sms::js_string_of(
            event
                .get("toolName")
                .filter(|v| !sms::nullish(v))
                .or_else(|| event.get("toolCallId"))
                .unwrap_or(&JsValue::Undefined),
        );
        return Some(sms::concat_units(&[
            &"\n⚠️ ".encode_utf16().collect::<Vec<_>>(),
            name.units(),
            &" failed\n".encode_utf16().collect::<Vec<_>>(),
        ]));
    }
    crate::channel::dingtalk::wire::render(event)
}
