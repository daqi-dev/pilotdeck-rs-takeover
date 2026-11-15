//! Pure validation of the Host-frozen original capability envelope. This
//! value is business data; the Host preparation carries all authority.
use agentcpm_iface::types::{Error, ErrorCategory};
use agentcpm_scheduler_protocol::context_history::OpaquePayload;
use pilotdeck_protocol::{JsValue, boundary_codec};

pub(crate) struct DecodedToolEnvelope {
    value: JsValue,
}
impl DecodedToolEnvelope {
    pub(crate) fn value(&self) -> &JsValue { &self.value }
}

/// Check the lossless profile and the two permitted routes only. Do not parse
/// `calls`, coerce arguments or run product context creation here: the
/// original Handler must still prepare its ToolInvocationContext first.
pub(crate) fn validate_envelope(payload: &OpaquePayload) -> Result<DecodedToolEnvelope, Error> {
    if payload.profile != boundary_codec::PROFILE { return Err(invalid()); }
    let value = boundary_codec::decode(&payload.bytes).map_err(|_| invalid())?;
    let outer = value.get("operation").and_then(JsValue::as_string).ok_or_else(invalid)?;
    let inner = value.get("payload").and_then(|v| v.get("operation"))
        .and_then(JsValue::as_string).ok_or_else(invalid)?;
    let module = value.get("module").and_then(JsValue::as_string).ok_or_else(invalid)?;
    if !module.equals_str("capability") || outer != inner
        || !(outer.equals_str("execute") || outer.equals_str("execute_batch")) {
        return Err(invalid());
    }
    Ok(DecodedToolEnvelope { value })
}
fn invalid() -> Error { Error::new(ErrorCategory::Protocol, -32602, "Invalid managed tool envelope.") }

#[cfg(test)]
mod tests {
    use super::*;
    use pilotdeck_protocol::{JsObject, JsString};
    fn payload(value: JsValue) -> OpaquePayload {
        OpaquePayload { profile: boundary_codec::PROFILE.into(), bytes: boundary_codec::encode(&value).unwrap() }
    }
    #[test]
    fn rejects_wrong_route_without_parsing_calls_and_keeps_original_units() {
        let id = JsString::from_utf16(vec![0xd800]);
        let mut inner = JsObject::new();
        inner.insert("operation".into(), "execute_batch".into());
        // Malformed calls intentionally survive this stage: Handler decides
        // the original error after its context preparation.
        inner.insert("calls".into(), JsValue::Undefined);
        inner.insert("marker".into(), JsValue::String(id.clone()));
        let mut outer = JsObject::new();
        outer.insert("module".into(), "capability".into());
        outer.insert("operation".into(), "execute_batch".into());
        outer.insert("payload".into(), JsValue::Object(inner));
        let encoded = payload(JsValue::Object(outer.clone()));
        let decoded = validate_envelope(&encoded).unwrap();
        assert!(decoded.value().get("payload").unwrap().get("calls").unwrap().is_undefined());
        assert_eq!(decoded.value().get("payload").unwrap().get("marker").unwrap().as_string(), Some(&id));
        outer.insert("operation".into(), "plan_todo".into());
        assert!(validate_envelope(&payload(JsValue::Object(outer))).is_err());
        let mut wrong = encoded;
        wrong.profile = "wrong".into();
        assert!(validate_envelope(&wrong).is_err());
    }
}
