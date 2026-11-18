//! Original initialization reads over the scoped public product-read resource.
//! The Kernel, never request payload, authenticates the parent and read source.
use crate::port_helpers::invalid;
use crate::resources::{ResourceCodec, ResourceRequest, ResourceStep};
use agentcpm_scheduler_protocol::{Client, context_history::OpaquePayload, product_reads};
use pilotdeck_agent::ports::{HostCall, PlanTodoMutation, PlanTodoRequest, PortError, PortResult};
use pilotdeck_protocol::{JsValue, boundary_codec};
use serde_json::Value;

/// Availability comes only from the actual call-bound SDK feature handshake.
pub struct InitialReadCodec {
    plan_todo: bool,
    model_metadata: bool,
    pending: Option<(&'static str, Vec<u8>)>,
}
impl InitialReadCodec {
    pub fn new(client: &dyn Client) -> Self {
        let features = client.host_features();
        Self {
            plan_todo: features
                .iter()
                .any(|value| value == product_reads::PLAN_TODO_FEATURE),
            model_metadata: features
                .iter()
                .any(|value| value == product_reads::MODEL_METADATA_FEATURE),
            pending: None,
        }
    }
    fn method(call: &HostCall) -> Option<&'static str> {
        match call {
            HostCall::PlanTodo(PlanTodoRequest {
                mutation: PlanTodoMutation::Read,
                ..
            }) => Some(product_reads::PLAN_TODO),
            HostCall::ModelMetadata { .. } => Some(product_reads::MODEL_METADATA),
            _ => None,
        }
    }
    fn encode(call: &HostCall) -> PortResult<Vec<u8>> {
        let payload = pilotdeck_transport::initialization_read_payload(call)?;
        boundary_codec::encode(&JsValue::Object(payload))
            .map_err(|_| invalid("initialization read input exceeds boundary profile"))
    }
}
impl ResourceCodec for InitialReadCodec {
    fn advertised(&self, module: &str, operation: &str) -> bool {
        (self.plan_todo && module == "capability" && operation == "plan_todo")
            || (self.model_metadata && module == "model" && operation == "get_metadata")
    }
    fn accepts_call(&self, call: &HostCall) -> bool {
        Self::method(call).is_some() && {
            let (module, operation) = call.method();
            self.advertised(module, operation)
        }
    }
    fn begin(&mut self, call: &HostCall) -> PortResult<ResourceStep> {
        if self.pending.is_some() {
            return Err(invalid("initialization read already pending"));
        }
        let method = Self::method(call).ok_or_else(|| invalid("not an initialization read"))?;
        let (module, operation) = call.method();
        if !self.advertised(module, operation) {
            return Err(PortError::NotAdvertised { module, operation });
        }
        let bytes = Self::encode(call)?;
        let request = OpaquePayload {
            profile: boundary_codec::PROFILE.into(),
            bytes: bytes.clone(),
        };
        let payload = serde_json::to_value(request)
            .map_err(|_| invalid("initialization read request encoding failed"))?;
        self.pending = Some((method, bytes));
        Ok(ResourceStep::Request(ResourceRequest { method, payload }))
    }
    fn resume(&mut self, call: &HostCall, response: Value) -> PortResult<ResourceStep> {
        let (method, original) = self
            .pending
            .take()
            .ok_or_else(|| invalid("initialization read response without request"))?;
        if Self::method(call) != Some(method) || Self::encode(call)? != original {
            return Err(invalid("initialization read request changed"));
        }
        let output: OpaquePayload = serde_json::from_value(response)
            .map_err(|_| invalid("invalid initialization read envelope"))?;
        if output.profile != boundary_codec::PROFILE {
            return Err(invalid("invalid initialization read response profile"));
        }
        let response = boundary_codec::decode(&output.bytes)
            .map_err(|_| invalid("invalid initialization read response encoding"))?;
        Ok(ResourceStep::Reply(Box::new(
            pilotdeck_transport::initialization_read_reply(call, response)?,
        )))
    }
    fn cleanup_calls(&self) -> Vec<HostCall> {
        vec![]
    }
    fn resource_failed(&mut self, _: &HostCall, _: &PortError) {
        self.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilotdeck_agent::ports::{HostReply, Route};
    use pilotdeck_protocol::{JsString, parse_json};
    fn codec() -> InitialReadCodec {
        InitialReadCodec {
            plan_todo: true,
            model_metadata: true,
            pending: None,
        }
    }
    fn output(json: &str) -> Value {
        let value = parse_json(json).unwrap();
        serde_json::to_value(OpaquePayload {
            profile: boundary_codec::PROFILE.into(),
            bytes: boundary_codec::encode(&value).unwrap(),
        })
        .unwrap()
    }
    fn metadata() -> HostCall {
        HostCall::ModelMetadata {
            route: Route {
                provider: JsString::from_utf16(vec![0x70, 0xd800]),
                model: "m".into(),
            },
        }
    }
    #[test]
    fn metadata_read_retains_original_utf16_and_projection() {
        let call = metadata();
        let mut codec = codec();
        let ResourceStep::Request(request) = codec.begin(&call).unwrap() else {
            panic!("request")
        };
        assert_eq!(request.method, product_reads::MODEL_METADATA);
        let input: OpaquePayload = serde_json::from_value(request.payload).unwrap();
        let input = boundary_codec::decode(&input.bytes).unwrap();
        assert_eq!(
            input.get("provider").unwrap().as_string().unwrap().units(),
            [0x70, 0xd800]
        );
        let ResourceStep::Reply(reply) = codec.resume(&call, output(r#"{"ok":true,"payload":{"metadata":{"provider":"p\ud800","model":"m","maxContextTokens":4096}}}"#)).unwrap() else { panic!("reply") };
        let HostReply::ModelMetadata(value) = *reply else {
            panic!("metadata")
        };
        assert_eq!(value.view.route.provider.units(), [0x70, 0xd800]);
        assert_eq!(value.view.max_context_tokens, Some(4096.0));
    }
    #[test]
    fn original_failure_preserves_utf16_and_writes_never_route_here() {
        let call = metadata();
        let mut codec = codec();
        codec.begin(&call).unwrap();
        let error = codec
            .resume(
                &call,
                output(r#"{"ok":false,"error":{"message":"bad\ud800"},"code":"E"}"#),
            )
            .err()
            .unwrap();
        let PortError::HostFailure { message, .. } = error else {
            panic!("known source failure")
        };
        assert_eq!(message.units(), [0x62, 0x61, 0x64, 0xd800]);
        let write = HostCall::PlanTodo(PlanTodoRequest {
            session_id: "s".into(),
            turn_id: "t".into(),
            mutation: PlanTodoMutation::MarkPlanApproved { plan: "p".into() },
        });
        assert!(!codec.accepts_call(&write));
    }
}
