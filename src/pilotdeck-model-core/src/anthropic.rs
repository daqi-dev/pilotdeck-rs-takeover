//! Anthropic 结束原因和用量规范化。
use crate::field;
use pilotdeck_protocol::{JsObject, JsValue};

/// `normalizeAnthropicFinishReason`.
pub fn normalize_finish_reason(reason: &JsValue) -> &'static str {
    match reason.as_string().and_then(|s| s.to_utf8().ok()).as_deref() {
        Some("end_turn" | "stop_sequence") => "stop",
        Some("max_tokens") => "length",
        Some("tool_use") => "tool_call",
        Some("refusal") => "content_filter",
        _ => "unknown",
    }
}

/// `normalizeAnthropicUsage`.
pub fn normalize_usage(raw: &JsValue) -> Option<JsValue> {
    let JsValue::Object(_) = raw else {
        return None;
    };
    let number = |key: &str| match field(raw, key) {
        JsValue::Number(n) if n.is_finite() => Some(*n),
        _ => None,
    };
    let values = [
        number("input_tokens"),
        number("output_tokens"),
        number("cache_read_input_tokens"),
        number("cache_creation_input_tokens"),
    ];
    let total = values.iter().flatten().copied().reduce(|a, b| a + b);
    if values.iter().all(Option::is_none) && total.is_none() {
        return None;
    }
    let mut out = JsObject::new();
    for (key, value) in [
        "inputTokens",
        "outputTokens",
        "cacheReadTokens",
        "cacheWriteTokens",
        "totalTokens",
    ]
    .iter()
    .zip(values.iter().copied().chain([total]))
    {
        out.insert(
            (*key).into(),
            value.map_or(JsValue::Undefined, JsValue::Number),
        );
    }
    Some(JsValue::Object(out))
}
