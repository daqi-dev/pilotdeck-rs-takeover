use crate::channel::sms;
use pilotdeck_protocol::{JsString, JsValue};
pub const ROBOT_TOPIC: &str = "/v1.0/im/bot/messages/get";
pub fn text(data: &JsValue) -> JsString {
    let value = data.get("text");
    let result = match value {
        Some(JsValue::Object(obj)) if obj.get("content").is_some() => sms::js_string_of(
            obj.get("content")
                .filter(|v| !sms::nullish(v))
                .unwrap_or(&JsValue::String(JsString::from(""))),
        ),
        Some(JsValue::String(text)) => text.clone(),
        _ => {
            let rich = data
                .get("richText")
                .filter(|v| !sms::nullish(v))
                .or_else(|| data.get("rich_text"));
            let Some(JsValue::Array(items)) = rich else {
                return JsString::from("");
            };
            let parts: Vec<_> = items
                .iter()
                .filter(|v| matches!(v, JsValue::Object(_) | JsValue::Array(_)))
                .map(|v| {
                    sms::js_string_of(
                        v.get("text")
                            .filter(|v| !sms::nullish(v))
                            .unwrap_or(&JsValue::String(JsString::from(""))),
                    )
                })
                .filter(|v| !v.is_empty())
                .collect();
            let mut units = Vec::new();
            for (i, part) in parts.iter().enumerate() {
                if i > 0 {
                    units.push(32);
                }
                units.extend_from_slice(part.units());
            }
            JsString::from_utf16(units)
        }
    };
    JsString::from_utf16(sms::trim_units(result.units()).to_vec())
}
pub fn trusted_webhook(value: &JsString) -> bool {
    value.units().starts_with(
        &"https://api.dingtalk.com/"
            .encode_utf16()
            .collect::<Vec<_>>(),
    )
}
pub fn render(event: &JsValue) -> Option<JsString> {
    let kind = event.get("type").and_then(JsValue::as_string)?;
    if kind.equals_str("assistant_text_delta") {
        return event.get("text").and_then(JsValue::as_string).cloned();
    }
    if kind.equals_str("assistant_thinking_delta") || kind.equals_str("tool_call_started") {
        return Some("".into());
    }
    if kind.equals_str("tool_call_finished") {
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
            &" 执行失败\n".encode_utf16().collect::<Vec<_>>(),
        ]));
    }
    if kind.equals_str("error") {
        let message = sms::js_string_of(event.get("message").unwrap_or(&JsValue::Undefined));
        return Some(sms::concat_units(&[
            &"\n❌ ".encode_utf16().collect::<Vec<_>>(),
            message.units(),
            &[10],
        ]));
    }
    None
}
