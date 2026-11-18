//! Pure fixed-baseline token-budget snapshots and auto-compaction decisions.
use crate::wire::string_field;
use pilotdeck_protocol::js_math::max as js_max;
use pilotdeck_protocol::{JsObject, JsValue};

/// Metadata matches snapshotFromTokens; absent properties remain absent.
pub fn snapshot_from_tokens(
    tokens: f64,
    context: f64,
    metadata: &JsObject,
    warning_ratio: f64,
    blocking_ratio: f64,
) -> JsValue {
    let reserved = js_max(
        metadata
            .get("reservedOutputTokens")
            .and_then(JsValue::as_number)
            .unwrap_or(0.0)
            .floor(),
        0.0,
    );
    let total = js_max(context.floor(), 1.0);
    let budget = if context.is_finite() && context > 0.0 {
        js_max(
            context.floor() - if reserved.is_finite() { reserved } else { 0.0 },
            1.0,
        )
    } else {
        context
    };
    let ratio = if budget > 0.0 { tokens / budget } else { 0.0 };
    let state = if ratio >= blocking_ratio {
        "blocking"
    } else if ratio >= warning_ratio {
        "warning"
    } else {
        "ok"
    };
    let mut result = JsObject::new();
    result.insert("tokens".into(), JsValue::Number(tokens));
    for key in ["localEstimateTokens", "displayTokens"] {
        if let Some(value) = metadata.get(key).filter(|v| !v.is_undefined())
            && (key != "displayTokens" || value.as_number() != Some(tokens))
        {
            result.insert(key.into(), value.clone());
        }
    }
    let usage = metadata
        .get("usageTokens")
        .is_some_and(|v| !v.is_undefined());
    result.insert(
        "estimateSource".into(),
        if usage { "usage" } else { "estimator" }.into(),
    );
    for key in [
        "usageTokens",
        "calibrationActualInputTokens",
        "calibrationEstimatedInputTokens",
        "breakdown",
    ] {
        if let Some(value) = metadata.get(key).filter(|v| !v.is_undefined()) {
            result.insert(key.into(), value.clone());
        }
    }
    for (key, number) in [
        ("totalContextTokens", total),
        ("maxContextTokens", budget),
        ("effectiveContextTokens", budget),
        ("maxOutputTokens", reserved),
        ("warningRatio", warning_ratio),
        ("blockingRatio", blocking_ratio),
    ] {
        result.insert(key.into(), JsValue::Number(number));
    }
    result.insert("state".into(), state.into());
    result.insert("ratio".into(), JsValue::Number(ratio));
    for key in ["source", "exact"] {
        if let Some(value) = metadata.get(key) {
            result.insert(key.into(), value.clone());
        }
    }
    result.insert("reservedOutputTokens".into(), JsValue::Number(reserved));
    if let Some(value) = metadata.get("estimatorError") {
        result.insert("estimatorError".into(), value.clone());
    }
    JsValue::Object(result)
}

/// Warning and blocking both trigger automatic compaction.
pub fn evaluate_snapshot(snapshot: JsValue) -> JsValue {
    let reason = match string_field(&snapshot, "state") {
        Some(s) if s.equals_str("blocking") => Some("blocking_threshold"),
        Some(s) if s.equals_str("warning") => Some("warning_threshold"),
        _ => None,
    };
    let mut result = JsObject::new();
    result.insert(
        "type".into(),
        if reason.is_some() { "trigger" } else { "skip" }.into(),
    );
    result.insert("snapshot".into(), snapshot);
    if let Some(reason) = reason {
        result.insert("reason".into(), reason.into());
    }
    JsValue::Object(result)
}
