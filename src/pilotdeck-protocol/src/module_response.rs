//! Original Module2.0 response prefix for a correlated host call.
use crate::{JsObject, JsString, JsValue};

/// Lossless complete ModuleCall/ModuleResponse values; these profiles carry
/// product data and never grant Host authority.
pub const TOOL_MODULE_REQUEST_V1: &str = "pilotdeck.tool.module-request.v1";
pub const TOOL_MODULE_RESPONSE_V1: &str = "pilotdeck.tool.module-response.v1";

/// Reuses the original Gateway projection. Call must be the original validated
/// request, including its actual messageId and requestId; values are data, not
/// authority. The caller supplies the actual result status.
pub fn original_module_response_header(call: &JsValue, ok: bool) -> JsObject {
    let mut response = JsObject::new();
    response.insert("kind".into(), "response".into());
    let id = call.get("messageId").unwrap().as_string().unwrap();
    let mut message_id: Vec<u16> = "host-module-".encode_utf16().collect();
    message_id.extend_from_slice(id.units());
    response.insert(
        "messageId".into(),
        JsValue::String(JsString::from_utf16(message_id)),
    );
    response.insert("inReplyTo".into(), JsValue::String(id.clone()));
    response.insert("requestId".into(), call.get("requestId").unwrap().clone());
    response.insert("ok".into(), JsValue::Bool(ok));
    response
}
/// Exact original successful response for an already validated request.
pub fn original_module_success_response(call: &JsValue, payload: JsObject) -> JsValue {
    let mut response = original_module_response_header(call, true);
    response.insert("payload".into(), JsValue::Object(payload));
    JsValue::Object(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn response_keeps_actual_ids_and_lone_utf16_unit() {
        let id = JsString::from_utf16(vec![0xd800, 0x78]);
        let mut call = JsObject::new();
        call.insert("messageId".into(), JsValue::String(id.clone()));
        call.insert("requestId".into(), "real-request".into());
        let response = original_module_success_response(&JsValue::Object(call), JsObject::new());
        let message = response.get("messageId").unwrap().as_string().unwrap();
        let expected: Vec<u16> = "host-module-"
            .encode_utf16()
            .chain(id.units().iter().copied())
            .collect();
        assert_eq!(message.units(), expected);
        assert_eq!(response.get("inReplyTo"), Some(&JsValue::String(id)));
        assert_eq!(
            response.get("requestId"),
            Some(&JsValue::from("real-request"))
        );
        assert_eq!(response.get("ok"), Some(&JsValue::Bool(true)));
    }
}
