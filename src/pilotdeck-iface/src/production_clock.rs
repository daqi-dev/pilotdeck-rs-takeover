//! Real wall clock and UUID policy used by the original Gateway turn factory.
use pilotdeck_agent::turn::TurnClock;
use pilotdeck_protocol::JsString;
use pilotdeck_transport::{server::ExecutionClock, system_clock::SystemExecutionClock};

pub struct ProductionTurnClock;
impl TurnClock for ProductionTurnClock {
    fn now_iso(&self) -> JsString {
        SystemExecutionClock.now_iso()
    }
    fn uuid(&self) -> JsString {
        uuid::Uuid::new_v4().to_string().into()
    }
    fn offers_uuid(&self) -> bool {
        false
    }
}
