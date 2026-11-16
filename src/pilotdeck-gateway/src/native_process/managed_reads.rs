//! Product snapshots, bound to the original Host reservation and turn owner.
use super::ProjectModelOwner;
use agentcpm_host::{TrustedReadCall, TrustedReadProvider};
use agentcpm_scheduler_protocol::{self as p, context_history::OpaquePayload, product_reads};
use pilotdeck_protocol::{JsObject, JsValue, boundary_codec};

pub struct NativeProductReads;
fn denied() -> p::Failure {
    p::Failure::new("PRODUCT_READ_DENIED", "original product turn unavailable")
}
fn protocol() -> p::Failure {
    p::Failure::new("PRODUCT_READ_PROTOCOL", "invalid product snapshot payload")
}
fn object(fields: impl IntoIterator<Item = (&'static str, JsValue)>) -> JsObject {
    let mut result = JsObject::new();
    for (key, value) in fields {
        result.insert(key.into(), value);
    }
    result
}
fn validate_plan(
    payload: &JsObject,
    identity: &pilotdeck_protocol::admission::ExecutionIdentity,
) -> p::Result<()> {
    if !payload
        .get("operation")
        .and_then(JsValue::as_string)
        .is_some_and(|v| v.equals_str("plan_todo"))
        || !payload
            .get("method")
            .and_then(JsValue::as_string)
            .is_some_and(|v| v.equals_str("read"))
    {
        return Err(protocol());
    }
    let session = payload
        .get("sessionId")
        .and_then(JsValue::as_string)
        .ok_or_else(protocol)?;
    let turn = payload
        .get("turnId")
        .and_then(JsValue::as_string)
        .ok_or_else(protocol)?;
    if session != &identity.session_id || turn != &identity.turn_id {
        return Err(denied());
    }
    Ok(())
}
impl TrustedReadProvider for NativeProductReads {
    fn read(
        &self,
        call: TrustedReadCall,
        input: OpaquePayload,
    ) -> agentcpm_iface::BoxFuture<'static, p::Result<OpaquePayload>> {
        Box::pin(async move {
            if input.profile != boundary_codec::PROFILE {
                return Err(protocol());
            }
            let request = boundary_codec::decode(&input.bytes).map_err(|_| protocol())?;
            let payload = request.as_object().ok_or_else(protocol)?;
            let reservation = call.project();
            reservation.revalidate()?;
            let owner = reservation
                .owner::<ProjectModelOwner>()
                .ok_or_else(denied)?;
            let turn = owner.borrow_read_turn(reservation).map_err(|_| denied())?;
            let identity = &turn.facts().identity;
            let operation = payload
                .get("operation")
                .and_then(JsValue::as_string)
                .ok_or_else(protocol)?;
            let result = match call.method() {
                product_reads::PLAN_TODO => {
                    validate_plan(payload, identity)?;
                    let plan = turn
                        .facts()
                        .native_plan_snapshot
                        .as_ref()
                        .ok_or_else(denied)?;
                    Ok(object([("snapshot", plan.snapshot().to_value())]))
                }
                product_reads::MODEL_METADATA => {
                    if !operation.equals_str("get_metadata") {
                        return Err(protocol());
                    }
                    if turn.facts().external_model_selected {
                        return Err(p::Failure::new(
                            "UNSUPPORTED",
                            "external model metadata is not installed for product reads",
                        ));
                    }
                    let provider =
                        crate::host::modules::wire::string_field(Some(payload), "provider");
                    let model = crate::host::modules::wire::string_field(Some(payload), "model");
                    match (provider, model) {
                        (Ok(provider), Ok(model)) => {
                            let metadata = turn.metadata_port().map_err(|_| denied())?;
                            crate::host::modules::model::get_metadata(
                                metadata.as_ref(),
                                provider,
                                model,
                            )
                        }
                        (Err(error), _) | (_, Err(error)) => Err(error),
                    }
                }
                _ => return Err(protocol()),
            };
            reservation.revalidate()?;
            let call_envelope = JsValue::Object(object([
                ("messageId", "product-read".into()),
                ("requestId", "product-read".into()),
            ]));
            let reply = crate::host::original_module_response(&call_envelope, result);
            let bytes = boundary_codec::encode(&reply).map_err(|_| protocol())?;
            Ok(OpaquePayload {
                profile: input.profile,
                bytes,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilotdeck_protocol::admission::ExecutionIdentity;
    #[test]
    fn only_same_turn_read_is_admitted() {
        let identity = ExecutionIdentity {
            session_id: "s".into(),
            turn_id: "t".into(),
            run_id: "r".into(),
            operation_id: "o".into(),
            request_id: "q".into(),
        };
        let payload = |session: &str, turn: &str, method: &str| {
            object([
                ("operation", "plan_todo".into()),
                ("method", method.into()),
                ("sessionId", session.into()),
                ("turnId", turn.into()),
            ])
        };
        assert!(validate_plan(&payload("s", "t", "read"), &identity).is_ok());
        assert!(validate_plan(&payload("other", "t", "read"), &identity).is_err());
        assert!(validate_plan(&payload("s", "other", "read"), &identity).is_err());
        assert!(validate_plan(&payload("s", "t", "write_todos"), &identity).is_err());
    }
}
