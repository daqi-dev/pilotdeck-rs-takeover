//! Shared JS value construction and JSON field decoding for the builtin tool
//! modules (`file_ops`, `search_ops`, `time`, `todo`).
//!
//! Single-copy home of the small object/string builders and of the common
//! field-decoding rules whose per-module wrappers only differ in the error
//! type they construct.

use pilotdeck_protocol::{JsObject, JsString, JsValue, parse_json};

/// Build a [`JsObject`] from a fixed-size key/value list.
pub(crate) fn object<const N: usize>(entries: [(&str, JsValue); N]) -> JsObject {
    let mut result = JsObject::new();
    for (key, value) in entries {
        result.insert(key.into(), value);
    }
    result
}

/// Wrap a value as a JSON string [`JsValue`].
pub(crate) fn string(value: impl Into<JsString>) -> JsValue {
    JsValue::String(value.into())
}

/// Parse one of the builtin tool input schema constants into a [`JsObject`].
///
/// The constants are compile-time JSON object literals; the panic branch is a
/// defensive assertion for corrupted constants and is unreachable for the
/// shipped schemas.
pub(crate) fn schema(json: &str) -> JsObject {
    match parse_json(json).as_ref() {
        Ok(JsValue::Object(object)) => object.clone(),
        _ => panic!("builtin input schema must be a valid JSON object"),
    }
}

fn required_string_value(obj: &JsObject, key: &str) -> Result<String, String> {
    let Some(val) = obj.get(key) else {
        return Err(format!("missing required string field '{key}'"));
    };
    let Some(s) = val.as_string() else {
        return Err(format!("'{key}' must be a string"));
    };
    s.to_utf8()
        .map_err(|_| format!("'{key}' contains unpaired UTF-16 surrogates"))
}

/// Decode a required, non-empty UTF-8 string field, returning the shared
/// error message on failure.
pub(crate) fn require_utf8_string(obj: &JsObject, key: &str) -> Result<String, String> {
    let value = required_string_value(obj, key)?;
    if value.is_empty() {
        return Err(format!("{key} must not be empty"));
    }
    Ok(value)
}

/// Decode a required UTF-8 string field that may be empty.
pub(crate) fn require_utf8_string_allow_empty(obj: &JsObject, key: &str) -> Result<String, String> {
    required_string_value(obj, key)
}

/// Decode an optional UTF-8 string field; empty values are rejected unless
/// `allow_empty` is set.
pub(crate) fn optional_utf8_string(
    obj: &JsObject,
    key: &str,
    allow_empty: bool,
) -> Result<Option<String>, String> {
    let value = match obj.get(key) {
        None | Some(JsValue::Null | JsValue::Undefined) => None,
        Some(val) => {
            let Some(s) = val.as_string() else {
                return Err(format!("'{key}' must be a string"));
            };
            Some(
                s.to_utf8()
                    .map_err(|_| format!("'{key}' contains unpaired UTF-16 surrogates"))?,
            )
        }
    };
    match value {
        Some(text) if !allow_empty && text.is_empty() => Err(format!("'{key}' must not be empty")),
        other => Ok(other),
    }
}

/// Decode a JSON number that must be a finite integer within `min..=usize::MAX`.
pub(crate) fn parse_positive_usize(val: &JsValue, key: &str, min: usize) -> Result<usize, String> {
    let Some(n) = val.as_number() else {
        return Err(format!("'{key}' must be an integer"));
    };
    if !n.is_finite() || n.fract() != 0.0 || n < (min as f64) || n > (usize::MAX as f64) {
        return Err(format!("'{key}' must be an integer >= {min}"));
    }
    Ok(n as usize)
}
