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
        crate::driver::errors::ErrorRef::from(self).name()
    }
    pub fn message(&self) -> JsString {
        crate::driver::errors::ErrorRef::from(self).message()
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
