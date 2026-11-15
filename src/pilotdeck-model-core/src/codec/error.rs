//! Runtime-independent model errors and projection failures.
use pilotdeck_protocol::{JsObject, JsString, JsValue};
#[derive(Clone, Debug)]
pub enum CodecError {
    Request {
        code: JsString,
        message: JsString,
        details: Option<Box<JsValue>>,
    },
    Provider(JsObject),
    Thrown {
        name: JsString,
        message: JsString,
    },
}
impl CodecError {
    pub fn name(&self) -> JsString {
        match self {
            Self::Request { .. } => "ModelRequestError".into(),
            Self::Provider(_) => "ModelProviderError".into(),
            Self::Thrown { name, .. } => name.clone(),
        }
    }
    pub fn message(&self) -> JsString {
        match self {
            Self::Request { message, .. } | Self::Thrown { message, .. } => message.clone(),
            Self::Provider(error) => error
                .get("message")
                .and_then(JsValue::as_string)
                .cloned()
                .unwrap_or_default(),
        }
    }
}
#[derive(Clone, Debug)]
pub struct BodyError {
    pub message: JsString,
}
impl BodyError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into().as_str().into(),
        }
    }
}

/// The same retry hint parser used by generic provider error normalization.
pub use super::normalize_error::retry_after_from_message;
