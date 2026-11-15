//! Original product Handler bound to one Host-admitted batch. This module
//! does not mint Host preparation or final permits; its only caller must be
//! the trusted, source-verified tool.prepare_batch adapter.
use super::project_model_owner::ProjectTurnBorrow;
#[cfg(test)]
use crate::host::HostErrorKind;
use crate::{
    host::modules::capability::Handler,
    host::{HostError, HostFuture},
    runtime::tools::scheduler::FinalToolAdmission,
};
use pilotdeck_protocol::{JsObject, JsValue};
use std::sync::Arc;

/// Decode the already Host-frozen value with the existing detached-value
/// profile. This checks data representation, not source or authority.
#[cfg(test)]
fn decode_original_envelope(profile: &str, bytes: &[u8]) -> Result<JsValue, HostError> {
    if profile != pilotdeck_protocol::boundary_codec::PROFILE {
        return Err(HostError::new(
            HostErrorKind::Protocol,
            "Unsupported tool boundary profile.",
        ));
    }
    pilotdeck_protocol::boundary_codec::decode(bytes)
        .map_err(|_| HostError::new(HostErrorKind::Protocol, "Invalid tool boundary value."))
}

/// An owned, non-clone batch. The original turn handles and checkpoint are
/// not reconstructed from a session ID or from the tool arguments.
pub(crate) struct OriginalToolBatch {
    handler: Handler,
    envelope: JsValue,
    turn: ProjectTurnBorrow,
}

impl OriginalToolBatch {
    /// `gate` and `envelope` must come from the same Host-validated batch
    /// preparation and `turn` from that preparation's checked owner/source.
    /// This helper is not itself an authority, serializer or release receipt.
    pub(crate) fn bind(
        turn: ProjectTurnBorrow,
        gate: Arc<dyn FinalToolAdmission>,
        envelope: JsValue,
    ) -> Result<Self, HostError> {
        let facts = &turn.facts().tools;
        if let Some(error) = &facts.selection_error {
            return Err(error.clone());
        }
        // An unsupported wrapper must fail closed. The implementation returns
        // a per-batch clone and must not modify the original shared runtime.
        let execution = facts.execution.for_managed_batch(gate)?;
        let handler = Handler::new(execution, facts.context.clone(), facts.observer.clone());
        Ok(Self {
            handler,
            envelope,
            turn,
        })
    }

    /// Consumes the original batch exactly once. The first returned owner is
    /// transferred into Host control, independently of the future: original
    /// ToolRuntime may detach concurrent siblings after returning an error.
    /// Host must also track/drain those siblings, hooks and the real project
    /// fork/reservation; the future or this owner alone proves no release.
    pub(crate) fn into_run(self) -> (ProjectTurnBorrow, HostFuture<JsObject>) {
        let control_owner = self.turn.clone();
        let future: HostFuture<JsObject> = Box::pin(async move {
            let Self {
                handler,
                envelope,
                turn,
            } = self;
            let result = handler.call(envelope).await;
            drop(turn);
            result
        });
        (control_owner, future)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opaque_profile_keeps_utf16_undefined_and_rejects_wrong_profile() {
        let mut object = JsObject::new();
        object.insert(
            "id".into(),
            JsValue::String(pilotdeck_protocol::JsString::from_utf16(vec![0xd800])),
        );
        object.insert("arg".into(), JsValue::Undefined);
        let value = JsValue::Object(object);
        let bytes = pilotdeck_protocol::boundary_codec::encode(&value).unwrap();
        let decoded =
            decode_original_envelope(pilotdeck_protocol::boundary_codec::PROFILE, &bytes).unwrap();
        assert_eq!(decoded, value);
        assert!(decode_original_envelope("wrong-profile", &bytes).is_err());
    }
}
