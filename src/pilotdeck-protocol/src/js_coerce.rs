//! JavaScript `String(value)` for wire values.
//!
//! Wire values come from `JSON.parse`, so they carry no user functions: an
//! object converts through `Object.prototype.toString` (`[object Object]`)
//! unless it has an own `toString` key, which is then a non-callable data
//! property; `OrdinaryToPrimitive` skips it, `valueOf` returns the object
//! itself, and the conversion throws `TypeError: Cannot convert object to
//! primitive value`. Arrays convert through `Array.prototype.join(",")`, where
//! `null` / `undefined` elements become empty strings.
//!
//! The walk is iterative. V8 recurses for nested arrays and throws a
//! `RangeError` once its native stack is exhausted; that depth depends on the
//! process stack size and frame layout (Node v22.22.0 on macOS: nesting of
//! 2000 converts, 5000 throws), so it is not reproduced here.

use crate::{JsString, JsValue, stringify_json};

/// The only exception `String(value)` can raise for a wire value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToPrimitiveError;

impl ToPrimitiveError {
    /// V8's message for the thrown `TypeError`.
    pub const MESSAGE: &'static str = "Cannot convert object to primitive value";
}

/// JavaScript `Number.prototype.toString()` (radix 10).
pub fn number_to_js_string(number: f64) -> JsString {
    if number.is_nan() {
        "NaN".into()
    } else if number == f64::INFINITY {
        "Infinity".into()
    } else if number == f64::NEG_INFINITY {
        "-Infinity".into()
    } else {
        // JSON.stringify of a finite number is its ToString.
        stringify_json(&JsValue::Number(number))
            .ok()
            .flatten()
            .unwrap_or_default()
            .as_str()
            .into()
    }
}

/// JavaScript `String(value)`.
pub fn to_js_string(value: &JsValue) -> Result<JsString, ToPrimitiveError> {
    enum Part<'a> {
        Value(&'a JsValue, bool),
        Comma,
    }
    let mut pending = vec![Part::Value(value, false)];
    let mut out: Vec<u16> = Vec::new();
    while let Some(part) = pending.pop() {
        let (value, in_array) = match part {
            Part::Comma => {
                out.push(u16::from(b','));
                continue;
            }
            Part::Value(value, in_array) => (value, in_array),
        };
        match value {
            JsValue::Undefined | JsValue::Null if in_array => {}
            JsValue::Undefined => out.extend("undefined".encode_utf16()),
            JsValue::Null => out.extend("null".encode_utf16()),
            JsValue::Bool(value) => {
                out.extend(if *value { "true" } else { "false" }.encode_utf16())
            }
            JsValue::String(text) => out.extend_from_slice(text.units()),
            JsValue::Number(number) => out.extend_from_slice(number_to_js_string(*number).units()),
            JsValue::Array(items) => {
                for (index, item) in items.iter().enumerate().rev() {
                    pending.push(Part::Value(item, true));
                    if index > 0 {
                        pending.push(Part::Comma);
                    }
                }
            }
            JsValue::Object(object) if object.contains_key("toString") => {
                return Err(ToPrimitiveError);
            }
            JsValue::Object(_) => out.extend("[object Object]".encode_utf16()),
        }
    }
    Ok(JsString::from_utf16(out))
}

/// JavaScript `values.join(separator)` over wire values.
pub fn join_js_strings<'a>(
    values: impl IntoIterator<Item = &'a JsValue>,
    separator: &str,
) -> Result<JsString, ToPrimitiveError> {
    let separator: Vec<u16> = separator.encode_utf16().collect();
    let mut out = Vec::new();
    for (index, value) in values.into_iter().enumerate() {
        if index > 0 {
            out.extend_from_slice(&separator);
        }
        if !matches!(value, JsValue::Undefined | JsValue::Null) {
            out.extend_from_slice(to_js_string(value)?.units());
        }
    }
    Ok(JsString::from_utf16(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_json;

    fn s(raw: &str) -> String {
        to_js_string(&parse_json(raw).unwrap())
            .unwrap()
            .to_utf8()
            .unwrap()
    }

    #[test]
    fn matches_node_string_conversion() {
        // Expected values from Node v22.22.0: String(JSON.parse(raw)).
        assert_eq!(s("7"), "7");
        assert_eq!(s("null"), "null");
        assert_eq!(s("true"), "true");
        assert_eq!(s("{\"a\":1}"), "[object Object]");
        assert_eq!(s("{\"valueOf\":1}"), "[object Object]");
        assert_eq!(s("[1,[2,null,\"z\"],true]"), "1,2,,z,true");
        assert_eq!(s("1.5e21"), "1.5e+21");
        assert_eq!(s("-0"), "0");
        assert_eq!(s("1e-7"), "1e-7");
        assert_eq!(s("[[],[[]],{}]"), ",,[object Object]");
        assert_eq!(
            to_js_string(&parse_json("[1,{\"toString\":2}]").unwrap()),
            Err(ToPrimitiveError)
        );
    }

    #[test]
    fn deep_arrays_do_not_recurse() {
        let mut value = JsValue::Number(1.0);
        for _ in 0..100_000 {
            value = JsValue::Array(vec![value]);
        }
        assert_eq!(to_js_string(&value).unwrap().to_utf8().unwrap(), "1");
        std::mem::forget(value);
    }

    #[test]
    fn join_skips_nullish() {
        let values = [JsValue::from("a"), JsValue::Null, JsValue::Number(7.0)];
        assert_eq!(
            join_js_strings(&values, "\n").unwrap().to_utf8().unwrap(),
            "a\n\n7"
        );
    }
}
