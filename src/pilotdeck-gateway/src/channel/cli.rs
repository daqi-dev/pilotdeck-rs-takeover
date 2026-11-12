//! `CliChannel`'s text rendering (src/adapters/channel/cli/cli-render.ts):
//! the plain stdout stream of the default `pilotdeck` command. Fragments are
//! built as UTF-16 like the JS template literals; the caller writes them the
//! way Node's stdout does (UTF-8, a lone surrogate as U+FFFD).
use crate::js_value::to_string_units;
use pilotdeck_protocol::{JsValue, stringify_json};

/// `CliChannel.channelKey`.
pub const CHANNEL_KEY: &str = "cli";

/// `defaultCliSessionKey(projectKey = process.cwd())` (CliChannel.ts:98-100);
/// the CLI never passes a project key, so it is always the working directory.
pub fn default_session_key(cwd: &str) -> String {
    format!("cli:project={cwd}:default")
}

/// `renderCliEvent(event)`: the text written for one gateway event, or
/// `None` when nothing is written (an unrendered type, or a falsy result).
pub fn render_event(event: &JsValue) -> Option<String> {
    let kind = event.get("type")?.as_string()?;
    const UNDEFINED: &JsValue = &JsValue::Undefined;
    let field = |name: &str| event.get(name).unwrap_or(UNDEFINED);
    let mut out: Vec<u16> = Vec::new();
    let push = |out: &mut Vec<u16>, text: &str| out.extend(text.encode_utf16());
    // `${value}`: String(value).
    let text = |out: &mut Vec<u16>, value: &JsValue| {
        out.extend(to_string_units(value).unwrap_or_default());
    };
    match kind.to_utf8().ok()?.as_str() {
        "assistant_text_delta" => {
            // `return event.text`: written only when truthy; a string here.
            let value = field("text");
            if !truthy(value) {
                return None;
            }
            text(&mut out, value);
        }
        "assistant_thinking_delta" => {
            push(&mut out, "[thinking] ");
            text(&mut out, field("text"));
        }
        "tool_call_started" => {
            push(&mut out, "\n[tool:");
            text(&mut out, field("name"));
            push(&mut out, "] started");
            if truthy(field("argsPreview")) {
                push(&mut out, " ");
                text(&mut out, field("argsPreview"));
            }
            push(&mut out, "\n");
        }
        "tool_call_finished" => {
            push(&mut out, "[tool:");
            text(&mut out, field("toolCallId"));
            push(
                &mut out,
                if truthy(field("ok")) {
                    "] ok"
                } else {
                    "] error"
                },
            );
            if truthy(field("resultPreview")) {
                push(&mut out, " ");
                text(&mut out, field("resultPreview"));
            }
            push(&mut out, "\n");
        }
        "permission_request" => {
            push(&mut out, "[permission] ");
            text(&mut out, field("toolName"));
            push(&mut out, " requires approval (");
            text(&mut out, field("requestId"));
            push(&mut out, ")\n");
        }
        "structured_output" => {
            push(&mut out, "\n[structured_output] ");
            // `JSON.stringify(undefined)` is `undefined`, printed as such.
            match stringify_json(field("payload")).ok().flatten() {
                Some(json) => push(&mut out, &json),
                None => push(&mut out, "undefined"),
            }
            push(&mut out, "\n");
        }
        "plan_mode_changed" => {
            push(&mut out, "\n[mode] ");
            text(&mut out, field("mode"));
            push(&mut out, "\n");
        }
        "error" => {
            push(&mut out, "\n[error:");
            match field("code") {
                JsValue::Undefined | JsValue::Null => push(&mut out, "gateway_error"),
                code => text(&mut out, code),
            }
            push(&mut out, "] ");
            text(&mut out, field("message"));
            push(&mut out, "\n");
        }
        _ => return None,
    }
    Some(String::from_utf16_lossy(&out))
}

fn truthy(value: &JsValue) -> bool {
    match value {
        JsValue::Undefined | JsValue::Null => false,
        JsValue::Bool(value) => *value,
        JsValue::Number(number) => *number != 0.0 && !number.is_nan(),
        JsValue::String(text) => !text.is_empty(),
        JsValue::Array(_) | JsValue::Object(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilotdeck_protocol::parse_json;

    fn render(json: &str) -> Option<String> {
        render_event(&parse_json(json).unwrap())
    }

    #[test]
    fn renders_each_event_like_cli_render() {
        assert_eq!(
            render(r#"{"type":"assistant_text_delta","text":"hi"}"#).as_deref(),
            Some("hi")
        );
        assert_eq!(render(r#"{"type":"assistant_text_delta","text":""}"#), None);
        assert_eq!(
            render(r#"{"type":"assistant_thinking_delta","text":"t"}"#).as_deref(),
            Some("[thinking] t")
        );
        assert_eq!(
            render(r#"{"type":"tool_call_started","name":"Read","argsPreview":"{\"a\":1}"}"#)
                .as_deref(),
            Some("\n[tool:Read] started {\"a\":1}\n")
        );
        assert_eq!(
            render(r#"{"type":"tool_call_started","name":"Read","argsPreview":""}"#).as_deref(),
            Some("\n[tool:Read] started\n")
        );
        assert_eq!(
            render(r#"{"type":"tool_call_finished","toolCallId":"c1","ok":false}"#).as_deref(),
            Some("[tool:c1] error\n")
        );
        assert_eq!(
            render(
                r#"{"type":"tool_call_finished","toolCallId":"c1","ok":true,"resultPreview":"x"}"#
            )
            .as_deref(),
            Some("[tool:c1] ok x\n")
        );
        assert_eq!(
            render(r#"{"type":"permission_request","toolName":"Bash","requestId":"r"}"#).as_deref(),
            Some("[permission] Bash requires approval (r)\n")
        );
        assert_eq!(
            render(r#"{"type":"structured_output","payload":{"b":1,"a":[true]}}"#).as_deref(),
            Some("\n[structured_output] {\"b\":1,\"a\":[true]}\n")
        );
        assert_eq!(
            render(r#"{"type":"structured_output"}"#).as_deref(),
            Some("\n[structured_output] undefined\n")
        );
        assert_eq!(
            render(r#"{"type":"plan_mode_changed","mode":"plan"}"#).as_deref(),
            Some("\n[mode] plan\n")
        );
        assert_eq!(
            render(r#"{"type":"error","message":"m"}"#).as_deref(),
            Some("\n[error:gateway_error] m\n")
        );
        assert_eq!(
            render(r#"{"type":"error","code":"x","message":"m"}"#).as_deref(),
            Some("\n[error:x] m\n")
        );
        assert_eq!(render(r#"{"type":"final"}"#), None);
        assert_eq!(render(r#"{"text":"x"}"#), None);
    }
}
