//! cc7c4e6 `homeassistant-render.ts`; fragments preserve JS UTF-16 strings.
use pilotdeck_protocol::{JsString, JsValue};

use crate::channel::sms::{js_string_of, nullish, truthy};

fn field<'a>(value: &'a JsValue, name: &str) -> &'a JsValue {
    static UNDEFINED: JsValue = JsValue::Undefined;
    value.get(name).unwrap_or(&UNDEFINED)
}

fn surround(before: &str, value: JsString, after: &str) -> JsString {
    JsString::from_utf16(
        before
            .encode_utf16()
            .chain(value.units().iter().copied())
            .chain(after.encode_utf16())
            .collect(),
    )
}

pub fn render_homeassistant_event(event: &JsValue) -> Option<JsString> {
    let kind = field(event, "type").as_string()?;
    if kind.equals_str("assistant_text_delta") {
        let text = field(event, "text");
        return (!nullish(text)).then(|| js_string_of(text));
    }
    if kind.equals_str("assistant_thinking_delta") || kind.equals_str("tool_call_started") {
        return Some("".into());
    }
    if kind.equals_str("tool_call_finished") {
        if truthy(field(event, "ok")) {
            return Some("".into());
        }
        let name = field(event, "toolName");
        return Some(surround(
            "\n⚠️ ",
            js_string_of(if nullish(name) {
                field(event, "toolCallId")
            } else {
                name
            }),
            " failed\n",
        ));
    }
    if kind.equals_str("error") {
        return Some(surround(
            "\n❌ ",
            js_string_of(field(event, "message")),
            "\n",
        ));
    }
    if kind.equals_str("elicitation_request") {
        let mut lines = Vec::new();
        if let JsValue::Array(questions) = field(event, "questions") {
            for question in questions {
                if truthy(field(question, "header")) {
                    lines.push(surround(
                        "**",
                        js_string_of(field(question, "header")),
                        "**",
                    ));
                }
                if truthy(field(question, "question")) {
                    lines.push(js_string_of(field(question, "question")));
                }
                if let JsValue::Array(options) = field(question, "options") {
                    for (index, option) in options.iter().enumerate() {
                        lines.push(surround(
                            &format!("{}. ", index + 1),
                            js_string_of(field(option, "label")),
                            "",
                        ));
                    }
                }
            }
        }
        if lines.is_empty() {
            return Some("".into());
        }
        let mut units = vec![10];
        for line in lines {
            units.extend(line.units());
            units.push(10);
        }
        return Some(JsString::from_utf16(units));
    }
    None
}
