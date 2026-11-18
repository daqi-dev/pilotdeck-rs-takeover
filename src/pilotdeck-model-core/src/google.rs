//! Google 的思考 token、缓存和结束原因语义。
use crate::field;
use pilotdeck_protocol::{JsObject, JsValue};

/// `normalizeGoogleFinishReason`.
pub fn normalize_finish_reason(reason: &JsValue) -> &'static str {
    let reason = match reason {
        JsValue::String(value) => String::from_utf16_lossy(value.units()).to_uppercase(),
        _ => return "unknown",
    };
    match reason.as_str() {
        "STOP" => "stop",
        "MAX_TOKENS" => "length",
        "MALFORMED_FUNCTION_CALL" | "UNEXPECTED_TOOL_CALL" => "tool_call",
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "IMAGE_SAFETY" => {
            "content_filter"
        }
        _ => "unknown",
    }
}

fn read_number(value: &JsValue) -> Option<f64> {
    value.as_number().filter(|value| value.is_finite())
}
fn sum_defined(values: &[Option<f64>]) -> Option<f64> {
    let mut any = false;
    let mut total = 0.0;
    for value in values.iter().flatten() {
        any = true;
        total += value;
    }
    any.then_some(total)
}

/// `normalizeGoogleUsage`. This is a protocol-specific projection, unlike the
/// OpenAI/Responses shared usage shape: output includes thoughts, input is
/// clamped at zero, and no cache-write/native-cost fields are exposed.
pub fn normalize_usage(raw: &JsValue) -> Option<JsValue> {
    raw.as_object()?;
    let prompt = read_number(field(raw, "promptTokenCount"));
    let response = read_number(field(raw, "candidatesTokenCount"))
        .or_else(|| read_number(field(raw, "responseTokenCount")));
    let thinking = read_number(field(raw, "thoughtsTokenCount"));
    let output = sum_defined(&[response, thinking]);
    let cache_read = read_number(field(raw, "cachedContentTokenCount"));
    let input = prompt.map(|prompt| (prompt - cache_read.unwrap_or(0.0)).max(0.0));
    let total =
        read_number(field(raw, "totalTokenCount")).or_else(|| sum_defined(&[prompt, output]));
    let mut usage = JsObject::new();
    usage.insert(
        "inputTokens".into(),
        input.map_or(JsValue::Undefined, JsValue::Number),
    );
    usage.insert(
        "outputTokens".into(),
        output.map_or(JsValue::Undefined, JsValue::Number),
    );
    usage.insert(
        "cacheReadTokens".into(),
        cache_read.map_or(JsValue::Undefined, JsValue::Number),
    );
    usage.insert(
        "totalTokens".into(),
        total.map_or(JsValue::Undefined, JsValue::Number),
    );
    let defined = [input, output, cache_read, total]
        .iter()
        .any(Option::is_some);
    defined.then(|| JsValue::Object(usage))
}
