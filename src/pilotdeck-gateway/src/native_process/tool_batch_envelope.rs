//! Pure validation of the Host-frozen original capability envelope. This
//! value is business data; the Host preparation carries all authority.
use agentcpm_iface::types::{Error, ErrorCategory};
use agentcpm_scheduler_protocol::context_history::OpaquePayload;
use pilotdeck_protocol::{JsValue, boundary_codec};

pub(crate) struct DecodedToolEnvelope {
    value: JsValue,
    full_response: bool,
}
impl DecodedToolEnvelope {
    pub(crate) fn value(&self) -> &JsValue {
        &self.value
    }
    pub(crate) fn full_response(&self) -> bool {
        self.full_response
    }
}

/// Check the lossless profile and the two permitted routes only. Do not parse
/// `calls`, coerce arguments or run product context creation here: the
/// original Handler must still prepare its ToolInvocationContext first.
pub(crate) fn validate_envelope(payload: &OpaquePayload) -> Result<DecodedToolEnvelope, Error> {
    let full_response =
        payload.profile == pilotdeck_protocol::module_response::TOOL_MODULE_REQUEST_V1;
    if payload.profile != boundary_codec::PROFILE && !full_response {
        return Err(invalid());
    }
    let value = boundary_codec::decode(&payload.bytes).map_err(|_| invalid())?;
    if full_response {
        let envelope =
            pilotdeck_protocol::wire::EnvelopeRef::decode(&value).map_err(|_| invalid())?;
        if !matches!(
            envelope.message,
            pilotdeck_protocol::wire::MessageRef::Request(pilotdeck_protocol::wire::RequestRef {
                body: pilotdeck_protocol::wire::RequestBodyRef::ModuleCall { .. },
                ..
            })
        ) {
            return Err(invalid());
        }
    }
    let outer = value
        .get("operation")
        .and_then(JsValue::as_string)
        .ok_or_else(invalid)?;
    let inner = value
        .get("payload")
        .and_then(|v| v.get("operation"))
        .and_then(JsValue::as_string)
        .ok_or_else(invalid)?;
    let module = value
        .get("module")
        .and_then(JsValue::as_string)
        .ok_or_else(invalid)?;
    if !module.equals_str("capability")
        || outer != inner
        || !(outer.equals_str("execute") || outer.equals_str("execute_batch"))
    {
        return Err(invalid());
    }
    Ok(DecodedToolEnvelope {
        value,
        full_response,
    })
}
fn invalid() -> Error {
    Error::new(
        ErrorCategory::Protocol,
        -32602,
        "Invalid managed tool envelope.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilotdeck_protocol::{JsObject, JsString};
    fn payload(value: JsValue) -> OpaquePayload {
        OpaquePayload {
            profile: boundary_codec::PROFILE.into(),
            bytes: boundary_codec::encode(&value).unwrap(),
        }
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
        assert!(
            decoded
                .value()
                .get("payload")
                .unwrap()
                .get("calls")
                .unwrap()
                .is_undefined()
        );
        assert_eq!(
            decoded
                .value()
                .get("payload")
                .unwrap()
                .get("marker")
                .unwrap()
                .as_string(),
            Some(&id)
        );
        outer.insert("operation".into(), "plan_todo".into());
        assert!(validate_envelope(&payload(JsValue::Object(outer))).is_err());
        let mut wrong = encoded;
        wrong.profile = "wrong".into();
        assert!(validate_envelope(&wrong).is_err());
    }
    #[test]
    fn complete_module_request_profile_requires_real_request_envelope() {
        let value = pilotdeck_protocol::parse_json(r#"{"kind":"request","messageId":"m","method":"module_call","module":"capability","operation":"execute","runId":"r","operationId":"o","requestId":"q","payload":{"operation":"execute","name":"read_file","toolCallId":"t","arguments":{}}}"#).unwrap();
        let payload = OpaquePayload {
            profile: pilotdeck_protocol::module_response::TOOL_MODULE_REQUEST_V1.into(),
            bytes: boundary_codec::encode(&value).unwrap(),
        };
        assert!(validate_envelope(&payload).unwrap().full_response());
        let bad = OpaquePayload {
            profile: pilotdeck_protocol::module_response::TOOL_MODULE_REQUEST_V1.into(),
            bytes: boundary_codec::encode(&JsValue::Object(JsObject::new())).unwrap(),
        };
        assert!(validate_envelope(&bad).is_err());
    }
}
