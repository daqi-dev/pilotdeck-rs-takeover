//! Stream error classification shared by the host and capability adapters.
//! ErrorRef borrows provider payloads so classification does not copy raw responses.
use super::Checkpoint;
use crate::codec::{
    CodecError,
    normalize_error::{
        ErrorInput, JsError, is_premature_stream_close, is_stream_guard_error,
        normalize_model_error,
    },
    projection::field,
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};

#[derive(Clone, Copy, Debug)]
pub enum ErrorRef<'a> {
    Request {
        message: &'a JsString,
    },
    Provider(&'a JsObject),
    Thrown {
        name: &'a JsString,
        message: &'a JsString,
    },
}
impl<'a> From<&'a CodecError> for ErrorRef<'a> {
    fn from(error: &'a CodecError) -> Self {
        match error {
            CodecError::Request { message, .. } => Self::Request { message },
            CodecError::Provider(error) => Self::Provider(error),
            CodecError::Thrown { name, message } => Self::Thrown { name, message },
        }
    }
}
impl ErrorRef<'_> {
    pub fn name(self) -> JsString {
        match self {
            Self::Request { .. } => "ModelRequestError".into(),
            Self::Provider(_) => "ModelProviderError".into(),
            Self::Thrown { name, .. } => name.clone(),
        }
    }
    pub fn message(self) -> JsString {
        match self {
            Self::Request { message, .. } | Self::Thrown { message, .. } => message.clone(),
            Self::Provider(error) => error
                .get("message")
                .and_then(JsValue::as_string)
                .cloned()
                .unwrap_or_default(),
        }
    }
    pub fn message_text(self) -> String {
        String::from_utf16_lossy(self.message().units())
    }
    /// All variants are JavaScript Errors; an aborted message wins over retryability.
    pub fn is_abort(self) -> bool {
        self.name().equals_str("AbortError")
            || self.message_text().to_lowercase().contains("aborted")
    }
    pub fn as_js_error(self) -> JsError {
        let name = self.name();
        let mut raw = JsObject::new();
        if is_stream_guard_error(&name) {
            raw.insert("name".into(), JsValue::String(name.clone()));
        }
        JsError {
            message: self.message(),
            code: None,
            network_code: None,
            raw: JsValue::Object(raw),
        }
    }
}
fn provider_id(provider: &JsValue) -> JsString {
    field(provider, "id")
        .as_string()
        .cloned()
        .unwrap_or_default()
}
fn protocol(provider: &JsValue) -> JsString {
    field(provider, "protocol")
        .as_string()
        .cloned()
        .unwrap_or_default()
}

/// `isRetryableStreamError`.
pub fn is_retryable_stream_error(error: ErrorRef<'_>) -> bool {
    if error.is_abort() {
        return false;
    }
    match error {
        ErrorRef::Provider(canonical) => {
            canonical.get("retryable") == Some(&JsValue::Bool(true))
                || is_premature_stream_close(&error.message_text())
        }
        ErrorRef::Request { .. } => {
            let message = error.message_text().to_lowercase();
            retryable_message(&message) || is_premature_stream_close(&error.message_text())
        }
        ErrorRef::Thrown { name, .. } => {
            if is_stream_guard_error(name) {
                return true;
            }
            let message = error.message_text().to_lowercase();
            retryable_message(&message) || is_premature_stream_close(&error.message_text())
        }
    }
}
/// `isRetryableStreamError`'s raw-`Error` branch (`streamModel.ts:587-618`).
/// This is the stream layer's list; `fallback::canonical_error` ports the
/// different router-layer `RouterRuntime.isNetworkTransient`
/// (`RouterRuntime.ts:1280-1304`), so dns/etimedout/abort belong there only.
pub fn retryable_message(message: &str) -> bool {
    [
        "network",
        "econnreset",
        "socket hang up",
        "fetch failed",
        "aborted",
        "timeout",
        "epipe",
        "econnrefused",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

/// `toProviderError(provider, error).error`.
pub fn provider_error(provider: &JsValue, error: ErrorRef<'_>) -> JsObject {
    match error {
        ErrorRef::Provider(canonical) => canonical.clone(),
        other => {
            let js = other.as_js_error();
            normalize_model_error(
                &provider_id(provider),
                &protocol(provider),
                ErrorInput::Error(&js),
                None,
            )
        }
    }
}
/// `streamInterruptionError`.
pub fn interruption_error(
    provider: &JsValue,
    error: ErrorRef<'_>,
    checkpoint: &Checkpoint,
) -> JsValue {
    let mut canonical = provider_error(provider, error);
    canonical.insert("streamInterruption".into(), checkpoint.interruption());
    JsValue::Object(canonical)
}
pub fn error_event(error: JsValue) -> JsValue {
    let mut event = JsObject::new();
    event.insert("type".into(), "error".into());
    event.insert("error".into(), error);
    JsValue::Object(event)
}
pub fn retry_reason(error: ErrorRef<'_>) -> &'static str {
    match error {
        ErrorRef::Provider(canonical)
            if canonical
                .get("code")
                .and_then(JsValue::as_string)
                .is_some_and(|c| c.equals_str("server_error")) =>
        {
            "server_error"
        }
        _ => "network_error",
    }
}
pub fn retry_after(error: ErrorRef<'_>) -> Option<f64> {
    match error {
        ErrorRef::Provider(canonical) => canonical.get("retryAfterMs").and_then(JsValue::as_number),
        _ => None,
    }
}
