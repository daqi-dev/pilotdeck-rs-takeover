//! Compaction components extracted from the Gateway implementation.
use pilotdeck_context_core::wire::{content, is_block, is_role, replace_content, text_block};
pub mod strip_multimedia;
pub use pilotdeck_context_core::tool_pair;
pub mod policy;
pub mod ports;
pub mod recovery;
pub mod types;
fn js_truthy(value: &pilotdeck_protocol::JsValue) -> bool {
    use pilotdeck_protocol::JsValue;
    match value {
        JsValue::Undefined | JsValue::Null => false,
        JsValue::Bool(value) => *value,
        JsValue::Number(value) => *value != 0.0 && !value.is_nan(),
        JsValue::String(value) => !value.is_empty(),
        JsValue::Array(_) | JsValue::Object(_) => true,
    }
}
pub mod orchestrator;
pub(crate) use pilotdeck_context_core::tool_pair::{
    is_direct_tool_result, is_media_reference_with_id, is_real_user_request_message,
};
pub(crate) use pilotdeck_context_core::wire::is_tool_result_only;
