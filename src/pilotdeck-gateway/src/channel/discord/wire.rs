use crate::channel::sms;
use pilotdeck_protocol::{JsString, JsValue};

pub(super) fn encode(value: &JsString) -> String {
    crate::channel::http_client::encode_uri_component(&String::from_utf16_lossy(value.units()))
}

pub(super) fn message(raw: &JsValue, own_id: Option<&JsString>) -> Option<JsString> {
    let author = raw.get("author")?;
    if author.get("bot").is_some_and(sms::truthy) {
        return None;
    }
    if own_id.is_some_and(|id| author.get("id").and_then(JsValue::as_string) == Some(id)) {
        return None;
    }
    if raw
        .get("type")
        .and_then(JsValue::as_number)
        .is_some_and(|kind| !matches!(kind as i64, 0 | 19 | 20 | 23))
    {
        return None;
    }
    let chat = raw.get("channel_id").and_then(JsValue::as_string)?;
    if chat.is_empty() {
        return None;
    }
    let text = raw
        .get("content")
        .filter(|v| !sms::nullish(v))
        .map(sms::js_string_of)
        .unwrap_or_default();
    let text = JsString::from_utf16(sms::trim_units(text.units()).to_vec());
    if text.is_empty() { None } else { Some(text) }
}

/// `chunkText`: JS length, lastIndexOf and slice all count UTF-16 code units.
pub(super) fn chunk_text(text: &JsString) -> Vec<JsString> {
    const MAX: usize = 2000;
    let mut rest = text.units();
    let mut chunks = Vec::new();
    if rest.len() <= MAX {
        return vec![text.clone()];
    }
    while rest.len() > MAX {
        let mut split = rest[..=MAX].iter().rposition(|v| *v == 10);
        if split.is_none_or(|n| n < MAX / 2) {
            split = rest[..=MAX].iter().rposition(|v| *v == 32);
        }
        let split = split.filter(|n| *n >= MAX / 2).unwrap_or(MAX);
        chunks.push(JsString::from_utf16(rest[..split].to_vec()));
        rest = &rest[split..];
        while rest.first() == Some(&10) {
            rest = &rest[1..];
        }
    }
    if !rest.is_empty() {
        chunks.push(JsString::from_utf16(rest.to_vec()));
    }
    chunks
}
