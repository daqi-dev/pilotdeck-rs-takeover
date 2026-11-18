//! Pure PilotDeck context compaction algorithms.
//!
//! Source boundary: fixed PilotDeck `cc7c4e6` model-free Snip/Micro engines,
//! tool-pair integrity and budget decisions. The caller supplies token counting
//! and tool-result text projection; this crate owns no tokenizer, model, host,
//! filesystem, session or permission state.

pub mod budget;
mod micro;
mod snip;
pub mod tool_pair;
pub mod wire;

pub use micro::{
    COMPACTABLE_TOOL_NAMES, MICROCOMPACT_CLEARED, MICROCOMPACT_FAILURES_FOLDED,
    MICROCOMPACT_RECOVERED_FAILURE_PREFIX, MicroCompactionEngine, MicroCompactionEngineOptions,
    MicroCompactionInput, MicroCompactionResult,
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
pub use snip::{
    SnipCallOptions, SnipEngine, SnipEngineOptions, SnipResult, create_snip_boundary,
    is_snip_boundary_message, project_snipped_view,
};
use tool_pair::*;
use wire::*;
