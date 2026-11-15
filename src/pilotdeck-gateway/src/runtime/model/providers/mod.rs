//! Trusted credential/error adapter around the shared pure provider codec.
#[cfg(test)]
pub(crate) mod anthropic;
pub(crate) mod google;
pub(crate) mod openai;
pub(crate) mod thinking;
use super::http::ModelStreamError;
pub(crate) use codec::{chat_endpoint_candidates, normalize_provider_base_url};
pub(crate) use pilotdeck_model_core::arguments::parse_tool_arguments;
use pilotdeck_model_core::codec::providers as codec;
pub(crate) use pilotdeck_model_core::request::{expected_complete_response_shape, final_body};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
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
pub(crate) fn provider_headers(provider: &JsValue, body: &JsValue) -> Vec<(JsString, JsValue)> {
    codec::provider_headers_with_auth(provider, body, |headers: &mut JsObject| {
        let units =
            crate::js_value::to_string_units(provider.get("apiKey").unwrap_or(&JsValue::Undefined))
                .unwrap_or_default();
        let key = String::from_utf16_lossy(&units);
        let key = crate::metadata::js_trim(&key);
        let anthropic = provider
            .get("protocol")
            .and_then(JsValue::as_string)
            .is_some_and(|protocol| protocol.equals_str("anthropic"));
        if anthropic {
            headers.insert("x-api-key".into(), key.into());
        } else if matches!(
            headers.get("authorization"),
            None | Some(JsValue::Undefined | JsValue::Null)
        ) {
            headers.insert(
                "authorization".into(),
                format!("Bearer {key}").as_str().into(),
            );
        }
    })
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
