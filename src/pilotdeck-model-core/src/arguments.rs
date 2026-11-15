//! UTF-16 工具参数解析，保留原解析优先级、修复标记与孤立代理码元。
use pilotdeck_protocol::JsValue;

/// UTF-16 JSON text as a Rust string for `parse_json`. A lone surrogate is
/// only legal inside a JSON string, where `\uXXXX` denotes the same unit; one
/// that would follow an escaping backslash stays invalid (U+FFFD), and
/// anywhere else both forms are a syntax error, as in `JSON.parse`. Shared by
/// the Anthropic and OpenAI tool-argument parsers (pr5).
pub fn json_text(units: &[u16]) -> String {
    let mut out = String::with_capacity(units.len());
    let mut backslashes = 0usize;
    for item in char::decode_utf16(units.iter().copied()) {
        match item {
            Ok(c) => {
                out.push(c);
                backslashes = if c == '\\' { backslashes + 1 } else { 0 };
            }
            Err(error) => {
                if backslashes % 2 == 1 {
                    out.push('\u{FFFD}');
                } else {
                    out.push_str(&format!("\\u{:04x}", error.unpaired_surrogate()));
                }
                backslashes = 0;
            }
        }
    }
    out
}

/// `JSON.parse(text)`, else `JSON.parse(jsonrepair(text))`, as every stream
/// normalizer parses tool-call arguments (anthropic, openai, openai-responses).
/// The bool is `wasRepaired`; `None` when both parses fail. Callers handle
/// their own empty-buffer default first.
pub fn parse_tool_arguments(units: &[u16]) -> Option<(JsValue, bool)> {
    let parse = |units: &[u16]| pilotdeck_protocol::parse_json(&json_text(units)).ok();
    if let Some(input) = parse(units) {
        return Some((input, false));
    }
    let repaired = crate::jsonrepair::jsonrepair(units).ok()?;
    parse(&repaired).map(|input| (input, true))
}
