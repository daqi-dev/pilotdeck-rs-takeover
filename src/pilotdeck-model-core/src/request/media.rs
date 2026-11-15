//! Conservative inspection of the canonical content schema, without projection.
//!
//! Content field rules follow the fixed canonical.ts definitions and the strict
//! content reader in turn-preparation/model_admission.rs. This is deliberately
//! narrower than its lax BoundaryOnly admission: an unknown content shape cannot
//! prove that an input is text-only. Opaque fields are never searched for media.
use pilotdeck_protocol::{JsObject, JsString, JsValue};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaInspectionError {
    Unsupported { path: String, reason: &'static str },
}
impl std::fmt::Display for MediaInspectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported { path, reason } => write!(f, "Cannot inspect {path}: {reason}"),
        }
    }
}
impl std::error::Error for MediaInspectionError {}
type Result<T> = std::result::Result<T, MediaInspectionError>;

fn unsupported(path: &str, reason: &'static str) -> MediaInspectionError {
    MediaInspectionError::Unsupported {
        path: path.into(),
        reason,
    }
}
fn object<'a>(value: &'a JsValue, path: &str) -> Result<&'a JsObject> {
    value
        .as_object()
        .ok_or_else(|| unsupported(path, "expected an object"))
}
fn array<'a>(value: &'a JsValue, path: &str) -> Result<&'a [JsValue]> {
    match value {
        JsValue::Array(items) => Ok(items),
        _ => Err(unsupported(path, "expected an array")),
    }
}
fn required<'a>(value: &'a JsObject, key: &str, path: &str) -> Result<&'a JsValue> {
    value
        .get(key)
        .ok_or_else(|| unsupported(&format!("{path}.{key}"), "missing required field"))
}
fn string<'a>(value: &'a JsObject, key: &str, path: &str) -> Result<&'a JsString> {
    required(value, key, path)?
        .as_string()
        .ok_or_else(|| unsupported(&format!("{path}.{key}"), "expected a UTF-16 string"))
}
fn choice(value: &JsObject, key: &str, path: &str, choices: &[&str]) -> Result<usize> {
    let text = string(value, key, path)?;
    choices
        .iter()
        .position(|choice| text.equals_str(choice))
        .ok_or_else(|| unsupported(&format!("{path}.{key}"), "unknown discriminator"))
}
#[derive(Clone, Copy)]
enum Scalar {
    String,
    Number,
    Bool,
}
fn scalar(value: &JsValue, kind: Scalar, path: &str) -> Result<()> {
    let valid = match kind {
        Scalar::String => matches!(value, JsValue::String(_)),
        Scalar::Number => matches!(value, JsValue::Number(_)),
        Scalar::Bool => matches!(value, JsValue::Bool(_)),
    };
    if valid {
        Ok(())
    } else {
        Err(unsupported(path, "invalid scalar field type"))
    }
}
fn fields(
    value: &JsObject,
    path: &str,
    required_fields: &[(&str, Scalar)],
    optional_fields: &[(&str, Scalar)],
) -> Result<()> {
    for (key, kind) in required_fields {
        scalar(required(value, key, path)?, *kind, &format!("{path}.{key}"))?;
    }
    for (key, kind) in optional_fields {
        if let Some(field) = value
            .get(key)
            .filter(|field| !matches!(field, JsValue::Undefined))
        {
            scalar(field, *kind, &format!("{path}.{key}"))?;
        }
    }
    Ok(())
}
fn timeline(value: &JsObject, path: &str) -> Result<()> {
    let Some(value) = value
        .get("timeline")
        .filter(|value| !matches!(value, JsValue::Undefined))
    else {
        return Ok(());
    };
    let path = format!("{path}.timeline");
    let value = object(value, &path)?;
    if required(value, "version", &path)?.as_number() != Some(1.0) {
        return Err(unsupported(
            &format!("{path}.version"),
            "expected timeline version 1",
        ));
    }
    use Scalar::*;
    fields(
        value,
        &path,
        &[
            ("turnId", String),
            ("id", String),
            ("order", Number),
            ("revision", Number),
        ],
        &[("previousId", String), ("offset", Number)],
    )
}
fn detail(value: &JsObject, path: &str) -> Result<()> {
    if value
        .get("detail")
        .is_some_and(|value| !matches!(value, JsValue::Undefined))
    {
        choice(value, "detail", path, &["auto", "low", "high"])?;
    }
    Ok(())
}
fn text(value: &JsObject, path: &str) -> Result<()> {
    fields(
        value,
        path,
        &[("text", Scalar::String)],
        &[("blockId", Scalar::String)],
    )
}
fn image(value: &JsObject, path: &str) -> Result<()> {
    choice(value, "source", path, &["base64", "url"])?;
    fields(
        value,
        path,
        &[("data", Scalar::String), ("mimeType", Scalar::String)],
        &[("bytes", Scalar::Number)],
    )?;
    detail(value, path)
}
fn pdf(value: &JsObject, path: &str) -> Result<()> {
    choice(value, "source", path, &["base64"])?;
    choice(value, "mimeType", path, &["application/pdf"])?;
    fields(
        value,
        path,
        &[("data", Scalar::String), ("bytes", Scalar::Number)],
        &[("pages", Scalar::Number)],
    )
}
fn inner_content(value: &JsValue, path: &str) -> Result<bool> {
    let value = object(value, path)?;
    let media = match choice(value, "type", path, &["text", "image", "pdf"])? {
        0 => {
            text(value, path)?;
            false
        }
        1 => {
            image(value, path)?;
            true
        }
        _ => {
            pdf(value, path)?;
            true
        }
    };
    timeline(value, path)?;
    Ok(media)
}
fn content(value: &JsValue, path: &str) -> Result<bool> {
    let value = object(value, path)?;
    use Scalar::*;
    let media = match choice(
        value,
        "type",
        path,
        &[
            "text",
            "thinking",
            "image",
            "pdf",
            "audio",
            "tool_call",
            "tool_result",
            "tool_result_reference",
            "media_reference",
        ],
    )? {
        0 => {
            text(value, path)?;
            false
        }
        1 => {
            fields(
                value,
                path,
                &[("text", String)],
                &[
                    ("blockId", String),
                    ("signature", String),
                    ("reasoningContent", String),
                ],
            )?;
            false
        }
        2 => {
            image(value, path)?;
            true
        }
        3 => {
            pdf(value, path)?;
            true
        }
        4 => {
            choice(value, "source", path, &["base64", "url"])?;
            fields(
                value,
                path,
                &[("data", String), ("mimeType", String)],
                &[("bytes", Number), ("durationSeconds", Number)],
            )?;
            true
        }
        5 => {
            // input/raw are opaque, including absent/undefined input admitted by the strict reader.
            fields(value, path, &[("id", String), ("name", String)], &[])?;
            false
        }
        6 => {
            fields(value, path, &[("toolCallId", String)], &[("isError", Bool)])?;
            let nested_path = format!("{path}.content");
            let nested = array(required(value, "content", path)?, &nested_path)?;
            let mut media = false;
            for (index, item) in nested.iter().enumerate() {
                // Do not short-circuit: later unknown items must still fail inspection.
                media |= inner_content(item, &format!("{nested_path}[{index}]"))?;
            }
            media
        }
        7 => {
            fields(
                value,
                path,
                &[
                    ("toolCallId", String),
                    ("path", String),
                    ("originalBytes", Number),
                    ("preview", String),
                    ("hasMore", Bool),
                ],
                &[
                    ("isError", Bool),
                    ("readFilePath", String),
                    ("mimeType", String),
                    ("reason", String),
                ],
            )?;
            // The persisted body is not read; it may contain media despite a text MIME hint.
            true
        }
        _ => {
            fields(
                value,
                path,
                &[
                    ("path", String),
                    ("originalBytes", Number),
                    ("preview", String),
                    ("hasMore", Bool),
                    ("mimeType", String),
                ],
                &[
                    ("toolCallId", String),
                    ("pages", Number),
                    ("reason", String),
                ],
            )?;
            choice(value, "mediaType", path, &["image", "pdf", "audio"])?;
            detail(value, path)?;
            true
        }
    };
    timeline(value, path)?;
    Ok(media)
}

/// Classify one fully assembled canonical request without altering or projecting it.
///
/// Only `messages[].content` and its declared content fields are inspected. Message
/// metadata, request schemas/extensions and tool input/raw remain opaque. Known
/// media and persisted-result references return true; unknown or invalid content
/// returns Unsupported, including shapes that lax BoundaryOnly admission retains.
/// This does not validate routing/provider capability, authorize media, read a
/// reference, or determine whether a model can consume the media. The caller must
/// seal and execute the same input group that was inspected.
pub fn inspect_request_media(request: &JsValue) -> Result<bool> {
    let request = object(request, "$")?;
    let messages = array(required(request, "messages", "$")?, "$.messages")?;
    let mut media = false;
    for (index, message) in messages.iter().enumerate() {
        let path = format!("$.messages[{index}]");
        let message = object(message, &path)?;
        choice(message, "role", &path, &["user", "assistant"])?;
        let items = array(
            required(message, "content", &path)?,
            &format!("{path}.content"),
        )?;
        for (index, item) in items.iter().enumerate() {
            media |= content(item, &format!("{path}.content[{index}]"))?;
        }
    }
    Ok(media)
}
