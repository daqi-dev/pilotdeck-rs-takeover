//! Pure PilotDeck input admission and owning-turn preparation.
#![forbid(unsafe_code)]
pub mod execute;
mod factory_payload;
pub mod initialization;
#[doc(hidden)]
pub mod model_admission;
pub mod preparation;
use pilotdeck_agent::ports::PortResult;
use pilotdeck_agent::{CapturedModelEvent, ProjectedMessage, ToolExecutionResult};
use pilotdeck_protocol::{JsString, JsValue};
pub use preparation::{
    OriginalTurnPreparation, PreparationEffects, permission_context, plan_addendum, run_mode,
};

#[derive(Debug, Clone)]
pub struct AdvertisedMethods {
    /// Original advertised order/duplicates retained. Methods are consumed only
    /// for exact known module/operation membership, not authority grants.
    pub modules: Vec<(JsString, Vec<JsString>)>,
}
impl AdvertisedMethods {
    pub fn supports(&self, module: &str, operation: &str) -> bool {
        self.modules.iter().any(|(m, operations)| {
            m.equals_str(module) && operations.iter().any(|op| op.equals_str(operation))
        })
    }
}
/// Consumer admission retains the full admitted value with its typed view.
/// Lax canonical messages successfully admit BoundaryOnly, not an empty message.
pub fn admit_message(value: &JsValue) -> PortResult<ProjectedMessage> {
    model_admission::message(value)
}
pub fn admit_model_event(value: &JsValue) -> PortResult<CapturedModelEvent> {
    model_admission::event(value)
}
pub fn admit_tool_result(value: &JsValue) -> PortResult<ToolExecutionResult> {
    model_admission::tool_result(value)
}
pub fn admit_model_request(value: &JsValue) -> PortResult<pilotdeck_agent::ports::CapturedRequest> {
    model_admission::request(value)
}

/// Decode the normalized normal-turn view of a source assembler result. The
/// mandatory raw assembled boundary remains the authority for open fields.
pub fn admit_assistant_assembly(
    value: &JsValue,
) -> PortResult<pilotdeck_agent::AssembledAssistantMessage> {
    model_admission::assembled(value)
}
