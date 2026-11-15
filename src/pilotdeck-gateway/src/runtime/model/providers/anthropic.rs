//! Keeps the existing jsonrepair/Anthropic oracle on the production pure codec.
use super::super::http::ModelStreamError;
pub(crate) use codec::StreamState;
use pilotdeck_model_core::codec::providers::anthropic as codec;
use pilotdeck_protocol::JsValue;
pub(crate) fn normalize_stream_event(
    raw: &JsValue,
    state: &mut StreamState,
) -> Result<Vec<JsValue>, ModelStreamError> {
    codec::normalize_stream_event(raw, state).map_err(super::codec_error)
}
