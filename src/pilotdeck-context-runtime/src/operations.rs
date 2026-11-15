//! Product runtime invocation boundary extracted from Gateway context dispatch.
//! Identity and capabilities are supplied by the trusted host, not fields.
use crate::host::{Cancellation, HostError, ModuleError};
use pilotdeck_protocol::{JsObject, JsValue};
use std::{future::Future, pin::Pin, sync::Arc};

pub type ContextFuture<T> = Pin<Box<dyn Future<Output = Result<T, HostError>> + Send + 'static>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    PrepareForModel,
    ApplyToolResults,
    RecoverFromModelError,
    CaptureTurn,
    TryAutoCompact,
}
impl Operation {
    pub fn name(self) -> &'static str {
        match self {
            Self::PrepareForModel => "prepare_for_model",
            Self::ApplyToolResults => "apply_tool_results",
            Self::RecoverFromModelError => "recover_from_model_error",
            Self::CaptureTurn => "capture_turn",
            Self::TryAutoCompact => "try_auto_compact",
        }
    }
}

pub struct ContextInput {
    pub fields: JsObject,
    pub abort_signal: Option<Arc<dyn Cancellation>>,
    pub budget_evaluator: Option<Arc<dyn CompactionBudgetEvaluator>>,
}
pub trait CompactionBudgetEvaluator: Send + Sync {
    fn evaluate(&self, messages: JsValue) -> ContextFuture<JsValue>;
}
pub trait ContextRuntime: Send + Sync {
    fn supports(&self, operation: Operation) -> bool;
    fn prepare_for_model(&self, input: ContextInput) -> ContextFuture<JsValue>;
    fn apply_tool_results(&self, _input: ContextInput) -> ContextFuture<JsValue> {
        unsupported(Operation::ApplyToolResults)
    }
    fn recover_from_model_error(&self, _input: ContextInput) -> ContextFuture<JsValue> {
        unsupported(Operation::RecoverFromModelError)
    }
    fn capture_turn(&self, _input: ContextInput) -> ContextFuture<JsValue> {
        unsupported(Operation::CaptureTurn)
    }
    fn try_auto_compact(&self, _input: ContextInput) -> ContextFuture<JsValue> {
        unsupported(Operation::TryAutoCompact)
    }
}
fn unsupported(operation: Operation) -> ContextFuture<JsValue> {
    Box::pin(async move {
        Err(ModuleError::new(format!(
            "Host context capability does not support {}.",
            operation.name()
        ))
        .into())
    })
}
