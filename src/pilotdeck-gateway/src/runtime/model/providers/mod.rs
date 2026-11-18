//! Trusted credential/error adapter around the shared pure provider codec.
#[cfg(test)]
pub(crate) mod anthropic;
pub(crate) mod thinking;
/// Existing EdgeClaw consumer keeps the same Google model-id projection.
pub(crate) mod google {
    pub(crate) use pilotdeck_model_core::codec::providers::google::normalize_model_id;
}
use super::http::ModelStreamError;
pub(crate) use codec::normalize_provider_base_url;
use pilotdeck_model_core::codec::providers as codec;
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
