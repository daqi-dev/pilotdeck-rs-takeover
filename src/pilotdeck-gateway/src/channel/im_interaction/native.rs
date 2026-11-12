//! Responses enter the exact coordinator used by the native turn host. This
//! module creates no coordinator, session owner, permission registry or turn.
use std::sync::Arc;

use pilotdeck_protocol::JsValue;

use super::InteractionResponder;
use crate::channel::{protocol, sms};
use crate::runtime::interaction::{GatewayInteractionCoordinator, PermissionDecision};

pub struct NativeInteractionResponder {
    coordinator: Arc<GatewayInteractionCoordinator>,
}

impl NativeInteractionResponder {
    pub fn new(coordinator: Arc<GatewayInteractionCoordinator>) -> Self {
        Self { coordinator }
    }
}

impl InteractionResponder for NativeInteractionResponder {
    fn respond_elicitation(
        &self,
        response: sms::ElicitationResponse,
    ) -> protocol::BoxFuture<Result<(), protocol::ChannelError>> {
        let coordinator = self.coordinator.clone();
        Box::pin(async move {
            if let Some(request_id) = response.request_id {
                // In-process IM answers have no WebSocket connection binding.
                // The helper deliberately ignores delivered=false, as TS
                // ImElicitationHelper.answer does (lines 72-77 and 99-104).
                coordinator.respond_elicitation(
                    &response.session_key,
                    &request_id,
                    response.answer,
                    None,
                );
            }
            // TS's async gateway method settles the bus synchronously, then
            // the helper's `await` yields before its continuation runs.
            tokio::task::yield_now().await;
            Ok(())
        })
    }

    fn permission_decide(
        &self,
        decision: sms::PermissionDecision,
    ) -> protocol::BoxFuture<Result<sms::PermissionDecisionOutcome, protocol::ChannelError>> {
        let coordinator = self.coordinator.clone();
        Box::pin(async move {
            let delivered = match decision.request_id {
                None => false,
                Some(request_id) => coordinator.decide_permission(
                    &decision.session_key,
                    PermissionDecision {
                        request_id,
                        decision: if decision.deny { "deny" } else { "allow" }.into(),
                        remember: (!decision.deny).then_some(JsValue::Bool(decision.remember)),
                        reason: decision
                            .deny
                            .then(|| "User denied permission from IM channel.".into()),
                    },
                    None,
                ),
            };
            tokio::task::yield_now().await;
            Ok(sms::PermissionDecisionOutcome { delivered })
        })
    }
}
