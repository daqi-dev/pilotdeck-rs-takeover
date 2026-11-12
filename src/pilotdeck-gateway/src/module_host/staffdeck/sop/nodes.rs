//! Internal node sessions are supplied by the host composition. The SOP
//! decorator may select a pinned node; it cannot construct or adopt a writer.
use crate::{
    host::HostFuture,
    runtime::{network::Cancellation, turn::host_factory::HostPortDecorators},
};
use pilotdeck_protocol::{JsString, JsValue};
use pilotdeck_transport::server::ExecutionFactory;
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct NodeSessionSpec {
    pub parent_session_id: JsString,
    pub session_id: JsString,
    pub sidechain_id: JsString,
    pub skill_id: JsString,
    pub node_id: JsString,
}
pub(crate) struct NodeTurnInput {
    pub spec: NodeSessionSpec,
    /// Only this turn's latest non-tool user blocks. The node owns its history.
    pub content: JsValue,
    pub options: JsValue,
    pub decorators: HostPortDecorators,
    /// A single-node execution wrapper, never the outer SOP dispatcher.
    pub factory: Arc<dyn ExecutionFactory>,
    pub signal: Cancellation,
}
pub(crate) struct NodeTurnResult {
    pub result: JsValue,
    pub messages: JsValue,
}
pub(crate) trait NodeSessionProvider: Send + Sync {
    /// Restore/cache the exact node, validate its parent, run one admitted
    /// input and forward committed node events through the parent's host buffer.
    fn run(&self, input: NodeTurnInput) -> HostFuture<NodeTurnResult>;
    /// Stop active work and drain every owned node writer before returning.
    fn close(&self) -> HostFuture<()>;
}
