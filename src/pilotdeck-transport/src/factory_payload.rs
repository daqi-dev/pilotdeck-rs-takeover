//! Kept as a source-path compatibility seam for existing decoder behavior tests.
use pilotdeck_protocol::{JsString, JsValue};
use pilotdeck_turn_preparation::execute::{
    FactoryPayload, FactoryPayloadDecoder, FactoryPayloadError, OriginalFactoryPayloadDecoder,
};
pub fn decode(
    execute: &JsValue,
    process_cwd: &JsString,
) -> Result<FactoryPayload, FactoryPayloadError> {
    OriginalFactoryPayloadDecoder.decode(execute, process_cwd)
}
