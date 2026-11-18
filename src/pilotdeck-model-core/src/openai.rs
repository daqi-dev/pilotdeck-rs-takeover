//! OpenAI Chat Completions 与 Responses 共享用量语义。
use crate::field;
use pilotdeck_protocol::{JsObject, JsValue};

fn is_record(value: &JsValue) -> bool {
    matches!(value, JsValue::Object(_))
}

pub fn normalize_finish_reason(reason: &JsValue) -> &'static str {
    match reason
        .as_string()
        .and_then(|text| text.to_utf8().ok())
        .as_deref()
    {
        Some("stop") => "stop",
        Some("length") => "length",
        Some("tool_calls" | "function_call") => "tool_call",
        Some("content_filter" | "content_filter_results") => "content_filter",
        _ => "unknown",
    }
}

fn read_number(value: &JsValue) -> Option<f64> {
    value.as_number().filter(|number| number.is_finite())
}

fn sum_defined(values: &[Option<f64>]) -> Option<f64> {
    let defined: Vec<f64> = values.iter().flatten().copied().collect();
    (!defined.is_empty()).then(|| defined.iter().sum())
}

/// `normalizeOpenAIUsage(raw)`.
/// `normalizeOpenAIUsage`; also used by the Responses stream.
pub fn normalize_usage(raw: &JsValue) -> Option<JsValue> {
    if !is_record(raw) {
        return None;
    }
    let prompt = read_number(field(raw, "prompt_tokens"))
        .or_else(|| read_number(field(raw, "input_tokens")));
    let output = read_number(field(raw, "completion_tokens"))
        .or_else(|| read_number(field(raw, "output_tokens")));
    let native_cost = read_number(field(raw, "cost"))
        .or_else(|| read_number(field(raw, "total_cost")))
        .or_else(|| read_number(field(raw, "estimated_cost")));
    let details = if is_record(field(raw, "prompt_tokens_details")) {
        Some(field(raw, "prompt_tokens_details"))
    } else if is_record(field(raw, "input_tokens_details")) {
        Some(field(raw, "input_tokens_details"))
    } else {
        None
    };
    let cache_read = details
        .and_then(|details| read_number(field(details, "cached_tokens")))
        .or_else(|| read_number(field(raw, "cache_read_input_tokens")));
    let cache_write = details
        .and_then(|details| read_number(field(details, "cache_write_tokens")))
        .or_else(|| read_number(field(raw, "cache_creation_input_tokens")));
    let input =
        prompt.map(|prompt| prompt - cache_read.unwrap_or(0.0) - cache_write.unwrap_or(0.0));
    let total = read_number(field(raw, "total_tokens")).or_else(|| sum_defined(&[prompt, output]));
    let values = [input, output, cache_read, cache_write, total, native_cost];
    if values.iter().all(Option::is_none) {
        return None;
    }
    let mut usage = JsObject::new();
    for (key, value) in [
        "inputTokens",
        "outputTokens",
        "cacheReadTokens",
        "cacheWriteTokens",
        "totalTokens",
        "nativeCost",
    ]
    .iter()
    .zip(values)
    {
        usage.insert(
            (*key).into(),
            value.map_or(JsValue::Undefined, JsValue::Number),
        );
    }
    Some(JsValue::Object(usage))
}
