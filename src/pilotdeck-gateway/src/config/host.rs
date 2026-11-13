//! Native representation of the fixed JS catalog's inherited provider lookup.
use super::{
    Error,
    value::{builtin_property, js_string},
};
use pilotdeck_protocol::{JsString, JsValue};

/// Called only after the provider parser and at the actual selection lookup.
/// A missing Object.prototype provider is truthy in JS, but its `.models` is
/// undefined, so indexing that value throws rather than returning not-found.
pub(super) fn inherited_provider_error(provider: &JsString, model: &JsString) -> Option<Error> {
    if !builtin_property(provider) {
        return None;
    }
    let mut error = Error::new(
        "gateway_request_failed",
        format!(
            "Cannot read properties of undefined (reading '{}')",
            js_string(&JsValue::String(model.clone()))
        ),
    );
    error.name = "TypeError";
    Some(error)
}
