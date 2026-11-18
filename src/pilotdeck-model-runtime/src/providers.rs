//! Error adapter around the unique pure provider codecs.
pub(crate) mod google;
use crate::ModelStreamError;
pub(crate) use codec::chat_endpoint_candidates;
pub(crate) use pilotdeck_model_core::arguments::parse_tool_arguments;
use pilotdeck_model_core::codec::providers as codec;
pub(crate) use pilotdeck_model_core::openai;
pub(crate) use pilotdeck_model_core::request::{expected_complete_response_shape, final_body};
use pilotdeck_protocol::JsValue;
pub(crate) fn codec_error(error: pilotdeck_model_core::codec::CodecError) -> ModelStreamError {
    match error {
        pilotdeck_model_core::codec::CodecError::Request {
            code,
            message,
            details,
        } => ModelStreamError::Request {
            code,
            message,
            details,
        },
        pilotdeck_model_core::codec::CodecError::Provider(raw) => ModelStreamError::Provider(raw),
        pilotdeck_model_core::codec::CodecError::Thrown { name, message } => {
            ModelStreamError::Thrown { name, message }
        }
    }
}
pub(crate) fn validate_model_request(
    request: &JsValue,
    config: &JsValue,
) -> Result<(JsValue, JsValue), ModelStreamError> {
    codec::validate_model_request(request, config).map_err(codec_error)
}
pub(crate) fn build_model_request(
    request: &JsValue,
    provider: &JsValue,
    model: &JsValue,
) -> Result<JsValue, ModelStreamError> {
    codec::build_model_request(request, provider, model).map_err(codec_error)
}
pub(crate) struct StreamState(codec::StreamState);
impl StreamState {
    pub(crate) fn new(provider: &JsValue) -> Self {
        Self(codec::StreamState::new(provider))
    }
    pub(crate) fn normalize(&mut self, raw: &JsValue) -> Result<Vec<JsValue>, ModelStreamError> {
        self.0.normalize(raw).map_err(codec_error)
    }
}
