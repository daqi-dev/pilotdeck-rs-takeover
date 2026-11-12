//! The external bridge's JSON contract, not a native WhatsApp protocol.
use pilotdeck_protocol::{JsString, JsValue};

use crate::channel::sms::{nullish, trim_units};

#[derive(Debug, PartialEq, Eq)]
pub(super) struct InboundMessage {
    pub id: JsString,
    pub chat: JsString,
    pub text: JsString,
}

pub(super) fn string(value: &JsValue) -> Result<JsString, String> {
    // JSON can shadow Object.prototype.toString with a non-callable value.
    // Array.join recursively applies the same conversion to its elements.
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            JsValue::Object(object) if object.contains_key("toString") => {
                return Err("TypeError: Cannot convert object to primitive value".into());
            }
            JsValue::Array(values) => pending.extend(values),
            _ => {}
        }
    }
    crate::js_value::to_string_units(value)
        .map(JsString::from_utf16)
        .map_err(|_| format!("RangeError: {}", crate::js_value::StackOverflow::MESSAGE))
}

fn field(value: &JsValue, names: &[&str]) -> Result<JsString, String> {
    let value = names
        .iter()
        .filter_map(|name| value.get(name))
        .find(|value| !nullish(value));
    let text = value.map(string).transpose()?.unwrap_or_default();
    Ok(JsString::from_utf16(trim_units(text.units()).to_vec()))
}

pub(super) fn messages(value: &JsValue) -> Result<Vec<InboundMessage>, String> {
    let items = match value {
        JsValue::Array(items) => Some(items),
        _ => ["messages", "data"]
            .iter()
            .find_map(|name| match value.get(name) {
                Some(JsValue::Array(items)) => Some(items),
                _ => None,
            }),
    };
    let mut output = Vec::new();
    for item in items
        .into_iter()
        .flatten()
        .filter(|item| matches!(item, JsValue::Object(_) | JsValue::Array(_)))
    {
        let id = field(item, &["id", "key", "messageId"])?;
        let chat = field(item, &["chatId", "chat_id", "from"])?;
        let text = field(item, &["text", "body", "content"])?;
        if !id.is_empty() && !chat.is_empty() {
            output.push(InboundMessage { id, chat, text });
        }
    }
    Ok(output)
}
