//! Scoped tool-result budgeting contract; implementation migration remains.
use pilotdeck_protocol::JsValue;
use std::{future::Future, pin::Pin};
#[derive(Debug)]
pub struct ToolBudgetError {
    pub message: String,
}
pub type ToolBudgetFuture<'a> =
    Pin<Box<dyn Future<Output = Result<JsValue, ToolBudgetError>> + Send + 'a>>;
pub trait ToolResultBudget: Send + Sync {
    fn apply_to_message<'a>(
        &'a self,
        message: &'a JsValue,
        turn_id: Option<&'a JsValue>,
    ) -> ToolBudgetFuture<'a>;
    fn apply_to_supplemental_message<'a>(
        &'a self,
        message: &'a JsValue,
        tool_call_id: Option<&'a JsValue>,
        turn_id: Option<&'a JsValue>,
    ) -> ToolBudgetFuture<'a>;
}
