//! Gateway adapter for shared pure thinking-plan resolution.
use super::super::http::ModelStreamError;
pub(crate) use codec::ThinkingPlan;
use pilotdeck_model_core::codec::providers::thinking as codec;
use pilotdeck_protocol::JsValue;
pub(crate) fn resolve(
    request: &JsValue,
    provider: &JsValue,
    model: &JsValue,
) -> Result<ThinkingPlan, ModelStreamError> {
    codec::resolve(request, provider, model).map_err(super::codec_error)
}
pub(crate) fn throw_if_unsupported(
    plan: &ThinkingPlan,
    request: &JsValue,
) -> Result<(), ModelStreamError> {
    codec::throw_if_unsupported(plan, request).map_err(super::codec_error)
}
