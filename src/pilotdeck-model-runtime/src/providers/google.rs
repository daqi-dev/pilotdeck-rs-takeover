//! Error adapter around the unique pure Google codec.
use crate::ModelStreamError;
pub(crate) use codec::{
    api_error_raw, final_body, normalize_model_id, sdk_response, synthesized_error_body,
};
use pilotdeck_model_core::codec::providers::google as codec;
pub(crate) use pilotdeck_model_core::google::{normalize_finish_reason, normalize_usage};
use pilotdeck_protocol::{JsString, JsValue};
#[derive(Default)]
pub(crate) struct StreamState(codec::StreamState);
impl StreamState {
    pub(crate) fn ended(&self) -> bool {
        self.0.ended()
    }
    pub(crate) fn normalize(&mut self, raw: &JsValue) -> Result<Vec<JsValue>, ModelStreamError> {
        self.0.normalize(raw).map_err(super::codec_error)
    }
}
pub(crate) fn build_request(
    request: &JsValue,
    model: &JsValue,
) -> Result<JsValue, ModelStreamError> {
    codec::build_request(request, model).map_err(super::codec_error)
}
pub(crate) fn endpoint(provider: &JsValue, model: &JsValue) -> Result<String, ModelStreamError> {
    codec::endpoint(provider, model).map_err(super::codec_error)
}
pub(crate) fn endpoint_with_stream(
    provider: &JsValue,
    model: &JsValue,
    stream: bool,
) -> Result<String, ModelStreamError> {
    codec::endpoint_with_stream(provider, model, stream).map_err(super::codec_error)
}
pub(crate) fn api_error_message(body: &JsValue) -> Result<JsString, ModelStreamError> {
    codec::api_error_message(body).map_err(super::codec_error)
}
pub(crate) fn parse_error_body(text: &str) -> Result<JsValue, ModelStreamError> {
    codec::parse_error_body(text).map_err(super::codec_error)
}
