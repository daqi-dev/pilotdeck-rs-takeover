//! Host-owned SOP orchestration. The portable service evaluates lifecycle
//! transitions; it never receives host credentials or an execution capability.
mod client;
mod definitions;
mod factory;
mod node_factory;
pub(crate) mod nodes;
mod ports;
mod prompt;
mod proposal;
mod runtime;
mod store;
#[cfg(test)]
mod tests;
pub(crate) use runtime::SopRuntime;

use pilotdeck_protocol::{JsObject, JsString, JsValue};

#[derive(Clone, Debug)]
pub(crate) struct SopError {
    pub code: String,
    pub message: String,
    pub details: Option<Box<JsValue>>,
    pub retryability: Option<String>,
}
impl SopError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: None,
            retryability: None,
        }
    }
    fn retry(mut self, value: &str) -> Self {
        self.retryability = Some(value.into());
        self
    }
}
impl From<crate::module_host::approvals::ApprovalFailure> for SopError {
    fn from(error: crate::module_host::approvals::ApprovalFailure) -> Self {
        let code = error
            .code
            .and_then(|v| v.as_string().map(|v| String::from_utf16_lossy(v.units())))
            .unwrap_or_else(|| "SOP_STATE_STORAGE_FAILED".into());
        Self::new(&code, super::sop_error_message(&code))
    }
}
fn is(value: &JsValue, key: &str, text: &str) -> bool {
    value
        .get(key)
        .and_then(JsValue::as_string)
        .is_some_and(|v| v.equals_str(text))
}
fn string(value: &JsValue, key: &str) -> bool {
    value.get(key).is_some_and(|v| v.as_string().is_some())
}
fn record(value: &JsValue, key: &str) -> bool {
    value.get(key).is_some_and(|v| v.as_object().is_some())
}
fn text(value: &JsValue, key: &str) -> Option<String> {
    value.get(key)?.as_string()?.to_utf8().ok()
}
fn set(value: &mut JsValue, key: &str, next: JsValue) {
    if let JsValue::Object(object) = value {
        object.insert(key.into(), next);
    }
}
fn remove(value: &mut JsValue, key: &str) {
    if let JsValue::Object(object) = value {
        let mut next = JsObject::new();
        for (name, item) in object.entries() {
            if !name.equals_str(key) {
                next.insert(name.clone(), item.clone());
            }
        }
        *object = next;
    }
}
fn array(value: &JsValue, key: &str, predicate: impl Fn(&JsValue) -> bool) -> bool {
    matches!(value.get(key), Some(JsValue::Array(values)) if values.iter().all(predicate))
}
fn optional(value: &JsValue, key: &str, predicate: impl FnOnce(&JsValue) -> bool) -> bool {
    value
        .get(key)
        .is_none_or(|v| v.is_undefined() || predicate(v))
}
fn now() -> JsValue {
    crate::runtime::context::prompt::iso_timestamp_now()
        .as_str()
        .into()
}
fn strings(value: &JsValue, key: &str) -> Vec<String> {
    match value.get(key) {
        Some(JsValue::Array(values)) => values
            .iter()
            .filter_map(|v| v.as_string()?.to_utf8().ok())
            .collect(),
        _ => vec![],
    }
}
fn json(value: &JsValue) -> String {
    pilotdeck_protocol::stringify_json(value)
        .ok()
        .flatten()
        .unwrap_or_else(|| "null".into())
}
impl From<SopError> for crate::host::ModuleError {
    fn from(error: SopError) -> Self {
        let mut result = Self::new(error.message);
        result.code = Some(error.code.into());
        result.retryable = error.retryability.map(|v| v == "safe");
        result
    }
}
