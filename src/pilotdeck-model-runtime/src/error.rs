//! Model execution errors, retaining the existing Gateway error contract.
use pilotdeck_model_core::codec::normalize_error::JsError;
use pilotdeck_model_core::driver::errors::ErrorRef;
use pilotdeck_protocol::{JsObject, JsString, JsValue};

/// What `streamModel` throws.
#[derive(Clone, Debug)]
pub enum ModelStreamError {
    /// `ModelRequestError(code, message)`: local validation, never retried.
    Request {
        code: JsString,
        message: JsString,
        details: Option<Box<JsValue>>,
    },
    /// `ModelProviderError(canonical)`.
    Provider(JsObject),
    /// Any other `Error` (`TypeError: terminated`, `SyntaxError`, `AbortError`
    /// and the stream guard errors).
    Thrown { name: JsString, message: JsString },
}
impl ModelStreamError {
    pub fn thrown(name: &str, message: impl Into<JsString>) -> Self {
        Self::Thrown {
            name: name.into(),
            message: message.into(),
        }
    }
    pub fn abort() -> Self {
        // `new AbortController().abort()` reason, returned by createAbortError.
        Self::thrown("AbortError", "This operation was aborted")
    }
    pub fn view(&self) -> ErrorRef<'_> {
        match self {
            Self::Request { message, .. } => ErrorRef::Request { message },
            Self::Provider(error) => ErrorRef::Provider(error),
            Self::Thrown { name, message } => ErrorRef::Thrown { name, message },
        }
    }
    pub fn name(&self) -> JsString {
        self.view().name()
    }
    pub fn message(&self) -> JsString {
        self.view().message()
    }
    pub fn message_text(&self) -> String {
        self.view().message_text()
    }
    pub fn is_abort(&self) -> bool {
        self.view().is_abort()
    }
    pub fn as_js_error(&self) -> JsError {
        self.view().as_js_error()
    }
}
