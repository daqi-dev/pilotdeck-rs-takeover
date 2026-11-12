//! Signal's line stream is deliberately not a generic SSE parser: each
//! `data:` line is one JSON value, and an unterminated line survives reconnect.
use pilotdeck_protocol::{JsString, JsValue};

use crate::channel::sms::{concat_units, nullish};
use crate::runtime::tools::builtin::bash::runner::Utf8Stream;

pub(super) struct Envelope {
    pub text: JsString,
    pub source: Option<JsString>,
    pub chat: Option<JsString>,
}

fn nonempty_string(value: Option<&JsValue>) -> Option<JsString> {
    value
        .and_then(JsValue::as_string)
        .filter(|s| !s.is_empty())
        .cloned()
}

pub(super) fn extract(raw: &JsValue) -> Option<Envelope> {
    let envelope = raw.get("envelope")?;
    let sent = envelope
        .get("syncMessage")
        .and_then(|v| v.get("sentMessage"));
    let dm = envelope.get("dataMessage");
    let text = nonempty_string(sent.and_then(|v| v.get("message")))
        .or_else(|| nonempty_string(dm.and_then(|v| v.get("message"))))?;
    let source = nonempty_string(envelope.get("source"))
        .or_else(|| nonempty_string(envelope.get("sourceNumber")));
    let group = dm
        .and_then(|v| v.get("groupInfo"))
        .and_then(|v| v.get("groupId"))
        .filter(|v| !nullish(v))
        .or_else(|| {
            sent.and_then(|v| v.get("groupInfo"))
                .and_then(|v| v.get("groupId"))
        });
    let chat = group
        .and_then(JsValue::as_string)
        .map(|id| concat_units(&[&"group:".encode_utf16().collect::<Vec<_>>(), id.units()]))
        .or_else(|| {
            source
                .as_ref()
                .map(|id| concat_units(&[&[100, 109, 58], id.units()]))
        });
    Some(Envelope { text, source, chat })
}

pub(super) fn strip_chat_prefix(chat: &JsString) -> JsString {
    let units = chat.units();
    let start = if units.starts_with(&[100, 109, 58]) {
        3
    } else if units.starts_with(&[103, 114, 111, 117, 112, 58]) {
        6
    } else {
        0
    };
    JsString::from_utf16(units[start..].to_vec())
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

/// The decoder is per connection; carry is owned by the receive loop. Do not
/// flush the decoder at EOF: TS discards its incomplete UTF-8 tail there.
pub(super) fn lines(decoder: &mut Utf8Stream, carry: &mut String, bytes: &[u8]) -> Vec<String> {
    carry.push_str(&decoder.decode(bytes));
    let Some(last) = carry.rfind('\n') else {
        return Vec::new();
    };
    let complete = carry[..last]
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned())
        .collect();
    *carry = carry[last + 1..].to_owned();
    complete
}
