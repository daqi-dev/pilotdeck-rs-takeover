//! 无 I/O 的公共模型请求清洗及响应形状判定。
use crate::{field, is};
use pilotdeck_protocol::{JsObject, JsValue};

/// `sanitizeProviderBody({ ...body, ...provider.extraBody })`.
pub fn final_body(body: &JsValue, provider: &JsValue) -> JsValue {
    let mut out = body.as_object().cloned().unwrap_or_default();
    if let Some(extra) = field(provider, "extraBody").as_object() {
        for (key, value) in extra.own_entries() {
            out.insert(key.clone(), value.clone());
        }
    }
    let mut result = JsObject::new();
    for (key, value) in out.own_entries() {
        if key.equals_str("temperature") {
            continue;
        }
        let value = match value {
            JsValue::Object(config) if key.equals_str("generationConfig") => {
                let mut kept = JsObject::new();
                for (k, v) in config.own_entries() {
                    if !k.equals_str("temperature") {
                        kept.insert(k.clone(), v.clone());
                    }
                }
                JsValue::Object(kept)
            }
            other => other.clone(),
        };
        result.insert(key.clone(), value);
    }
    JsValue::Object(result)
}

/// `isExpectedProviderResponseShape(protocol, body)` for the non-streaming
/// `complete()` path (`providerEndpoint.ts:130-143`). The caller only consults
/// this for an OK response with more than one endpoint candidate; streaming
/// accepts every OK response and single-endpoint calls never inspect the body.
/// Google is not a `complete()` protocol (it stays `UnsupportedProtocol`).
pub fn expected_complete_response_shape(protocol: &JsValue, body: &JsValue) -> bool {
    let JsValue::Object(record) = body else {
        return false;
    };
    if is(protocol, "anthropic") {
        return matches!(record.get("content"), Some(JsValue::Array(_)))
            || matches!(
                record.get("type"),
                Some(JsValue::String(value)) if value.equals_str("message")
            );
    }
    if is(protocol, "openai-responses") {
        return matches!(
            record.get("object"),
            Some(JsValue::String(value)) if value.equals_str("response")
        ) || matches!(record.get("output"), Some(JsValue::Array(_)))
            || matches!(record.get("output_text"), Some(JsValue::String(_)));
    }
    matches!(record.get("choices"), Some(JsValue::Array(_)))
}
