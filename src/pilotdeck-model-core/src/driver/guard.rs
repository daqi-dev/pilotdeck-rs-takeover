//! Duration and repeated-chunk guards; all time/environment inputs are explicit.
use super::retry::retry_number;
use crate::codec::{CodecError, projection::field};
use pilotdeck_protocol::{JsString, JsValue};
const REPEATED_CHUNK_LIMIT: f64 = 100.0;
fn thrown(name: &str, message: impl Into<JsString>) -> CodecError {
    CodecError::Thrown {
        name: name.into(),
        message: message.into(),
    }
}

pub struct StreamGuard {
    started: f64,
    max_duration: Option<f64>,
    limit: f64,
    last: Option<JsString>,
    repeated: f64,
}
impl StreamGuard {
    /// The host reads LITELLM_MAX_STREAMING_DURATION_SECONDS and supplies its raw value.
    pub fn new(provider: &JsValue, now: f64, duration_env: Option<&str>) -> Self {
        let env = duration_env
            .filter(|raw| !crate::codec::metadata::js_trim(raw).is_empty())
            .map(crate::codec::js::string_to_number)
            .filter(|v| v.is_finite() && *v > 0.0)
            .map(|v| v * 1000.0);
        Self {
            started: now,
            max_duration: retry_number(provider, "maxStreamingDurationMs").or(env),
            limit: retry_number(provider, "repeatedChunkLimit").unwrap_or(REPEATED_CHUNK_LIMIT),
            last: None,
            repeated: 1.0,
        }
    }
    pub fn check_duration(&self, now: f64) -> Result<(), CodecError> {
        match self.max_duration {
            Some(max) if now - self.started > max => Err(thrown(
                "MaxStreamingDurationError",
                format!(
                    "Stream exceeded max streaming duration of {}ms",
                    crate::codec::js::number_string(max)
                ),
            )),
            _ => Ok(()),
        }
    }
    pub fn observe(&mut self, event: &JsValue, now: f64) -> Result<(), CodecError> {
        self.check_duration(now)?;
        let text = match (field(event, "type"), field(event, "text")) {
            (JsValue::String(t), JsValue::String(text))
                if t.equals_str("text_delta") && text.len_utf16() > 2 =>
            {
                text
            }
            _ => {
                self.repeated = 1.0;
                self.last = None;
                return Ok(());
            }
        };
        if self.last.as_ref() == Some(text) {
            self.repeated += 1.0;
        } else {
            self.last = Some(text.clone());
            self.repeated = 1.0;
        }
        if self.repeated >= self.limit {
            let mut message: Vec<u16> = "The model is repeating the same chunk = "
                .encode_utf16()
                .collect();
            message.extend(text.units());
            message.push(u16::from(b'.'));
            return Err(thrown(
                "RepeatedStreamingChunkError",
                JsString::from_utf16(message),
            ));
        }
        Ok(())
    }
}
