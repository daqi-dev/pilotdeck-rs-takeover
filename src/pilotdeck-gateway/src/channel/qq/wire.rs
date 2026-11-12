use crate::channel::sms;
use pilotdeck_protocol::{JsString, JsValue};

pub(super) struct Envelope {
    pub text: JsString,
}

pub(super) fn identify(token: &JsString) -> JsValue {
    super::object(&[
        ("op", JsValue::Number(2.0)),
        (
            "d",
            super::object(&[
                (
                    "token",
                    JsValue::String(JsString::from(format!(
                        "QQBot {}",
                        String::from_utf16_lossy(token.units())
                    ))),
                ),
                ("intents", JsValue::Number(33554432.0)),
                (
                    "shard",
                    JsValue::Array(vec![JsValue::Number(0.0), JsValue::Number(1.0)]),
                ),
                (
                    "properties",
                    super::object(&[
                        ("$os", JsValue::from("linux")),
                        ("$browser", JsValue::from("pilotdeck")),
                        ("$device", JsValue::from("pilotdeck")),
                    ]),
                ),
            ]),
        ),
    ])
}

/// QQ counts UTF-16 units, keeps spaces, and consumes exactly one boundary LF.
pub(super) fn chunk_text(text: &JsString, max: usize) -> Vec<JsString> {
    let max = max.max(1);
    let mut rest = text.units();
    if rest.len() <= max {
        return vec![text.clone()];
    }
    let mut chunks = Vec::new();
    while !rest.is_empty() {
        if rest.len() <= max {
            chunks.push(JsString::from_utf16(rest.to_vec()));
            break;
        }
        let split = rest[..=max]
            .iter()
            .rposition(|v| *v == 10)
            .filter(|v| (*v as f64) >= max as f64 * 0.3)
            .unwrap_or(max);
        chunks.push(JsString::from_utf16(rest[..split].to_vec()));
        rest = &rest[split..];
        if rest.first() == Some(&10) {
            rest = &rest[1..];
        }
    }
    chunks
}

pub(super) fn render_event(event: &JsValue) -> Option<JsString> {
    if event
        .get("type")
        .and_then(JsValue::as_string)
        .is_some_and(|v| v.equals_str("tool_call_finished"))
        && !event.get("ok").is_some_and(sms::truthy)
    {
        let name = event
            .get("toolName")
            .filter(|v| !sms::nullish(v))
            .or_else(|| event.get("toolCallId"))
            .map(sms::js_string_of)
            .unwrap_or_else(|| JsString::from("undefined"));
        return Some(sms::concat_units(&[
            &"\n⚠️ ".encode_utf16().collect::<Vec<_>>(),
            name.units(),
            &" 执行失败\n".encode_utf16().collect::<Vec<_>>(),
        ]));
    }
    sms::render::render_sms_event(event)
}
