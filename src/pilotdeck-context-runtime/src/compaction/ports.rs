//! Stage interfaces from Gateway compaction engine/port.rs. The implementations
//! and orchestrator are still being extracted; these types perform no I/O.
use crate::{
    host::HostError,
    operations::{ContextFuture, ContextInput},
};
use pilotdeck_context_core::{
    MicroCompactionInput, MicroCompactionResult, SnipCallOptions, SnipResult,
};
use pilotdeck_protocol::JsValue;

pub trait CompactionRunner: Send + Sync {
    fn auto_compact(&self, input: ContextInput) -> ContextFuture<JsValue>;
}
pub trait CompactionBudgetPort: Send + Sync {
    /// Raw values must reach the provider intact; it owns JS coercion and
    /// errors in the original evaluation order.
    fn evaluate_raw(
        &self,
        messages: &JsValue,
        context: &JsValue,
        reserved: &JsValue,
    ) -> Result<JsValue, HostError>;
    fn evaluate(
        &self,
        messages: &[JsValue],
        max_context_tokens: f64,
        reserved: Option<f64>,
    ) -> Result<JsValue, HostError>;
    fn estimate_messages(&self, messages: &[JsValue]) -> Result<f64, HostError>;
}
pub trait CompactionPolicyPort: Send + Sync {
    fn evaluate_snapshot(&self, snapshot: JsValue) -> Result<JsValue, HostError>;
}
pub trait CompactionRecoveryPort: Send + Sync {
    fn decide(&self, input: JsValue) -> Result<JsValue, HostError>;
}
pub trait CompactionMicroPort: Send + Sync {
    fn apply(&self, input: &MicroCompactionInput<'_>) -> Result<MicroCompactionResult, HostError>;
}
pub trait CompactionSnipPort: Send + Sync {
    fn snip(&self, messages: &[JsValue], options: SnipCallOptions)
    -> Result<SnipResult, HostError>;
}
pub use pilotdeck_protocol::compaction::project_compaction_budget;

pub trait CompactionSummaryPort: Send + Sync {
    fn run(
        &self,
        input: super::types::CompactionInput,
    ) -> ContextFuture<super::types::CompactionResult>;
}

pub struct CheckpointTruncation {
    pub messages: Vec<JsValue>,
    pub changed: bool,
}
pub type PostCompactBuilder =
    std::sync::Arc<dyn Fn(&super::types::CompactionResult) -> Vec<JsValue> + Send + Sync>;
pub type CheckpointTruncator =
    std::sync::Arc<dyn Fn(&[JsValue], f64) -> CheckpointTruncation + Send + Sync>;
#[derive(Clone)]
pub struct CompactionPort {
    pub dispose: Option<std::sync::Arc<dyn Fn() -> ContextFuture<()> + Send + Sync>>,
    pub auto_compact: Option<std::sync::Arc<dyn CompactionRunner>>,
    pub budget: Option<std::sync::Arc<dyn CompactionBudgetPort>>,
    pub policy: Option<std::sync::Arc<dyn CompactionPolicyPort>>,
    pub summary: Option<std::sync::Arc<dyn CompactionSummaryPort>>,
    pub micro: Option<std::sync::Arc<dyn CompactionMicroPort>>,
    pub snip: Option<std::sync::Arc<dyn CompactionSnipPort>>,
    pub recovery: Option<std::sync::Arc<dyn CompactionRecoveryPort>>,
    pub build_post_compact_messages: PostCompactBuilder,
    pub truncate_head_preserving_checkpoint: CheckpointTruncator,
}
impl CompactionPort {
    pub fn build_post_compact_messages(
        &self,
        result: &super::types::CompactionResult,
    ) -> Vec<JsValue> {
        (self.build_post_compact_messages)(result)
    }
    pub fn truncate_head_preserving_checkpoint(
        &self,
        messages: &[JsValue],
        ratio: f64,
    ) -> Vec<JsValue> {
        (self.truncate_head_preserving_checkpoint)(messages, ratio).messages
    }
    pub fn try_auto_compact(
        &self,
        input: ContextInput,
        default_max_context_tokens: f64,
    ) -> ContextFuture<JsValue> {
        if let Some(runner) = &self.auto_compact {
            return runner.auto_compact(input);
        }
        super::orchestrator::CompactionOrchestrator::new(self.clone(), default_max_context_tokens)
            .auto_compact(input)
    }
}
