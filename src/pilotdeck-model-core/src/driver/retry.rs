//! Native retry delays; clocks, random draws and sleeping remain in the host.
use crate::codec::projection::field;
use pilotdeck_protocol::{JsValue, js_math::min as js_math_min};

pub(super) fn retry_number(provider: &JsValue, key: &str) -> Option<f64> {
    match field(field(provider, "retry"), key) {
        JsValue::Undefined | JsValue::Null => None,
        value => Some(crate::codec::js::to_number(value).unwrap_or(f64::NAN)),
    }
}

/// `createNativeRetryPolicy({ defaultMaxRetries: 2 }).decide({ kind: "stream" })`.
pub fn retry_decide(
    provider: &JsValue,
    attempt: f64,
    retry_after: Option<f64>,
    random: f64,
) -> (f64, f64) {
    let max_retries = retry_number(provider, "streamMaxRetries").unwrap_or(2.0);
    let max_delay = retry_number(provider, "maxDelayMs").unwrap_or(8_000.0);
    if let Some(after) = retry_after {
        return (max_retries, js_math_min(after, max_delay));
    }
    let base = retry_number(provider, "baseDelayMs").unwrap_or(500.0);
    let jitter = retry_number(provider, "jitter").unwrap_or(0.75);
    let deterministic = base * (attempt + 1.0);
    (
        max_retries,
        js_math_min(deterministic + deterministic * jitter * random, max_delay),
    )
}

/// `createNativeRetryPolicy({defaultMaxRetries: 2, defaultJitter: 0})` for
/// `kind: "request"` (complete uses a linear, unjittered request retry).
pub fn retry_decide_request(provider: &JsValue, attempt: f64, random: f64) -> (f64, f64) {
    let max_retries = retry_number(provider, "requestMaxRetries").unwrap_or(2.0);
    let max_delay = retry_number(provider, "maxDelayMs").unwrap_or(8_000.0);
    let base = retry_number(provider, "baseDelayMs").unwrap_or(500.0);
    let jitter = retry_number(provider, "jitter").unwrap_or(0.0);
    let deterministic = base * (attempt + 1.0);
    (
        max_retries,
        js_math_min(deterministic + deterministic * jitter * random, max_delay),
    )
}
