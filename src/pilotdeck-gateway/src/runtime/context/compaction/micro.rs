//! Gateway compatibility adapter; compaction uses the shared pure implementation.
pub use pilotdeck_context_core::{
    COMPACTABLE_TOOL_NAMES, MICROCOMPACT_CLEARED, MICROCOMPACT_FAILURES_FOLDED,
    MICROCOMPACT_RECOVERED_FAILURE_PREFIX, MicroCompactionEngineOptions, MicroCompactionInput,
    MicroCompactionResult,
};
use pilotdeck_tokenizer::TokenizeError;

pub struct MicroCompactionEngine(pilotdeck_context_core::MicroCompactionEngine);
impl MicroCompactionEngine {
    pub fn new(options: MicroCompactionEngineOptions) -> Self {
        Self(pilotdeck_context_core::MicroCompactionEngine::new(options))
    }
    pub fn apply(
        &self,
        input: &MicroCompactionInput<'_>,
    ) -> Result<MicroCompactionResult, TokenizeError> {
        self.0.apply(
            input,
            &super::count_tokens,
            &super::flatten_tool_result_block_text,
        )
    }
}
