use pilotdeck_model_core::codec::js::StackOverflow;
use pilotdeck_protocol::{JsObject, JsString, JsValue};

enum StableFrame<'a> {
    Array(&'a [JsValue], usize),
    Object(Vec<(&'a JsString, &'a JsValue)>, usize),
}

/// `context/cache/CachePlan.ts:stableSerialize`, including JS's `undefined`
/// interpolation and UTF-16 key ordering.
///
/// The baseline recurses and V8 throws `RangeError: Maximum call stack size
/// exceeded` past a caller-, shape- and JIT-dependent depth (2026-10-23,
/// Node v22.22.0: a 2398-level nested-array `inputSchema` succeeds through
/// both real entries while a 2100-level nested object overflows). No fixed
/// constant reproduces that boundary, so this walk uses an explicit stack
/// that never exhausts the Rust stack and keeps each caller's own pre-merge
/// depth behavior via `depth_bound`:
///
/// - the runtime entries (the coordinator's cache plans and the message
///   fingerprints below) pass the shared `js_value::MAX_ARRAY_DEPTH`
///   container gate and report [`StackOverflow`] past it, as they always
///   have;
/// - the host entry passes `None`, its pre-merge ungated walk.
///
/// Output is appended into one buffer, avoiding the per-level strings that
/// made the recursive form O(depth²). Both sides' residual differences
/// against V8 are registered in `specs/gateway-parity/accepted-diffs.md`
/// A6-JSVALUE-DEPTH-001 (unapproved).
pub(crate) fn stable_serialize(
    value: &JsValue,
    depth_bound: Option<usize>,
) -> Result<String, StackOverflow> {
    let mut out = String::new();
    let mut frames: Vec<StableFrame<'_>> = Vec::new();
    let mut next = Some(value);
    loop {
        if let Some(value) = next.take() {
            match value {
                JsValue::Array(items) => {
                    if depth_bound.is_some_and(|bound| frames.len() >= bound) {
                        return Err(StackOverflow);
                    }
                    out.push('[');
                    frames.push(StableFrame::Array(items, 0));
                }
                JsValue::Object(object) => {
                    if depth_bound.is_some_and(|bound| frames.len() >= bound) {
                        return Err(StackOverflow);
                    }
                    let mut entries: Vec<(&JsString, &JsValue)> = object
                        .entries()
                        .iter()
                        .map(|(key, value)| (key, value))
                        .collect();
                    entries.sort_by(|a, b| a.0.units().cmp(b.0.units()));
                    out.push('{');
                    frames.push(StableFrame::Object(entries, 0));
                }
                JsValue::Undefined => out.push_str("undefined"),
                value => out.push_str(&stringify_scalar(value)),
            }
        }
        let last = frames.len();
        if last == 0 {
            return Ok(out);
        }
        match &mut frames[last - 1] {
            StableFrame::Array(items, index) => {
                if let Some(item) = items.get(*index) {
                    if *index > 0 {
                        out.push(',');
                    }
                    *index += 1;
                    if !item.is_undefined() {
                        next = Some(item);
                    }
                } else {
                    out.push(']');
                    frames.pop();
                }
            }
            StableFrame::Object(entries, index) => {
                if let Some((key, value)) = entries.get(*index) {
                    if *index > 0 {
                        out.push(',');
                    }
                    *index += 1;
                    out.push_str(&stringify_scalar(&JsValue::String((*key).clone())));
                    out.push(':');
                    next = Some(value);
                } else {
                    out.push('}');
                    frames.pop();
                }
            }
        }
    }
}
fn stringify_scalar(value: &JsValue) -> String {
    pilotdeck_protocol::stringify_json(value)
        .ok()
        .flatten()
        .unwrap_or_else(|| "null".into())
}

pub(crate) fn object(fields: Vec<(&str, JsValue)>) -> JsValue {
    let mut object = JsObject::new();
    for (key, value) in fields {
        object.insert(key.into(), value);
    }
    JsValue::Object(object)
}
