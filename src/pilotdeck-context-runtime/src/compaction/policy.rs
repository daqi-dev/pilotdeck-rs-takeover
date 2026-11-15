//! Fixed-baseline `AutoCompactionPolicy`: warning and blocking both trigger.
use super::ports::{CompactionBudgetPort, CompactionPolicyPort};
use crate::host::HostError;
use pilotdeck_protocol::JsValue;
use std::sync::Arc;

pub struct AutoCompactionPolicy {
    budget: Arc<dyn CompactionBudgetPort>,
}
impl AutoCompactionPolicy {
    pub fn new(budget: Arc<dyn CompactionBudgetPort>) -> Self {
        Self { budget }
    }
    pub fn evaluate(
        &self,
        messages: &[JsValue],
        max_context_tokens: f64,
        reserved_output_tokens: Option<f64>,
    ) -> Result<JsValue, HostError> {
        self.evaluate_snapshot(self.budget.evaluate(
            messages,
            max_context_tokens,
            reserved_output_tokens,
        )?)
    }
}
impl CompactionPolicyPort for AutoCompactionPolicy {
    fn evaluate_snapshot(&self, snapshot: JsValue) -> Result<JsValue, HostError> {
        Ok(pilotdeck_context_core::budget::evaluate_snapshot(snapshot))
    }
}
