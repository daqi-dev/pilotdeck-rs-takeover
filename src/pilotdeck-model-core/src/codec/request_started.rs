//! Exact request_started event and fingerprint over the canonical request.
use super::{CodecError, projection::field};
use pilotdeck_protocol::{JsObject, JsValue};
use sha2::{Digest, Sha256};
pub fn request_started_event(provider: &JsValue, current: &JsValue) -> Result<JsValue, CodecError> {
    let fingerprint = super::js::json_stringify(current)
        .map_err(|_| CodecError::Thrown {
            name: "RangeError".into(),
            message: super::js::StackOverflow::MESSAGE.into(),
        })?
        .unwrap_or_default();
    let mut started = JsObject::new();
    started.insert("type".into(), "request_started".into());
    started.insert("provider".into(), field(provider, "id").clone());
    started.insert("model".into(), field(current, "model").clone());
    started.insert(
        "providerBaseUrl".into(),
        super::providers::normalize_provider_base_url(field(provider, "url")),
    );
    started.insert(
        "requestFingerprint".into(),
        format!("{:x}", Sha256::digest(fingerprint.as_bytes()))
            .as_str()
            .into(),
    );
    started.insert("metadata".into(), field(current, "metadata").clone());
    Ok(JsValue::Object(started))
}
