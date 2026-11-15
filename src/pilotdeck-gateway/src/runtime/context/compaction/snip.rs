//! Gateway compatibility adapter; all pruning runs in pilotdeck-context-core.
pub use pilotdeck_context_core::{
    SnipCallOptions, SnipEngineOptions, SnipResult, create_snip_boundary, is_snip_boundary_message,
};
use pilotdeck_protocol::JsValue;
use pilotdeck_tokenizer::TokenizeError;

pub struct SnipEngine(pilotdeck_context_core::SnipEngine);
impl SnipEngine {
    pub fn new(options: SnipEngineOptions) -> Self {
        Self(pilotdeck_context_core::SnipEngine::new(options))
    }
    pub fn snip(
        &self,
        messages: &[JsValue],
        options: SnipCallOptions,
    ) -> Result<SnipResult, TokenizeError> {
        self.0
            .snip(messages, options, &super::estimate_messages_tokens)
    }
}
pub fn project_snipped_view(
    messages: &[JsValue],
    options: SnipEngineOptions,
) -> Result<Vec<JsValue>, TokenizeError> {
    pilotdeck_context_core::project_snipped_view(
        messages,
        options,
        &super::estimate_messages_tokens,
    )
}
