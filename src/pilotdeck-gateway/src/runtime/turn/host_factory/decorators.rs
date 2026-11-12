//! Session-selected wrappers around freshly composed, turn-owned host ports.
use crate::host::modules::{
    capability::ToolExecution, context::ContextRuntime, model::ModelExecution,
};
use pilotdeck_protocol::admission::ExecutionIdentity;
use std::sync::Arc;

pub type ModelPortDecorator = Arc<
    dyn Fn(&ExecutionIdentity, Arc<dyn ModelExecution>) -> Arc<dyn ModelExecution> + Send + Sync,
>;
pub type ContextPortDecorator = Arc<
    dyn Fn(&ExecutionIdentity, Arc<dyn ContextRuntime>) -> Arc<dyn ContextRuntime> + Send + Sync,
>;
pub type ToolPortDecorator =
    Arc<dyn Fn(&ExecutionIdentity, Arc<dyn ToolExecution>) -> Arc<dyn ToolExecution> + Send + Sync>;

/// Wrappers retain the original port and delegate operations they do not
/// customize. Every invocation receives the actual turn identity, never a
/// shared mutable "current turn". Underlying routing, recording, metadata,
/// context state and capability ownership are composed as usual first.
#[derive(Clone, Default)]
pub struct HostPortDecorators {
    pub model: Option<ModelPortDecorator>,
    pub context: Option<ContextPortDecorator>,
    pub tool_execution: Option<ToolPortDecorator>,
}
