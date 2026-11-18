//! Google non-streaming SDK exchange and canonical response projection.
//! Request encoding, SDK field projection, usage and error normalization are
//! shared with the existing streaming implementation.
use pilotdeck_protocol::{JsObject, JsString, JsValue};

use super::super::{
    AbortFlag, DriverFailure, ErrorInput, ModelHostServices, ModelHttpTransport, ModelStreamError,
    StreamOptions, duration,
    google::{
        GoogleSendMode, google_response_error, send_google_request_mode, throw_if_google_abort,
    },
    normalize_model_error, provider_id, providers, response_text, wait_abort,
};
use super::{as_record, field, non_empty_string, object, string_or_empty};
use crate::network::{Exchange, ExchangeFuture, exchange_prearmed_continuing};

pub(super) async fn complete_attempt<S: ModelHostServices + ?Sized>(
    transport: &dyn ModelHttpTransport<Rejection = S::Rejection, Preparation = S::Preparation>,
    host: &S,
    provider: &JsValue,
    request: &JsValue,
    body: JsValue,
    options: &StreamOptions,
) -> Result<JsValue, DriverFailure<S::Rejection>> {
    // createGoogleClient supplies provider.timeoutMs to the SDK. The SDK arms
    // one abort timer for headers AND response.json(); stream idle settings are
    // deliberately not consulted by generateContent.
    let deadline = field(provider, "timeoutMs")
        .as_number()
        .filter(|ms| *ms > 0.0)
        .map(|ms| tokio::time::Instant::now() + duration(ms));
    let mut before_abort = false;
    let outcome = async {
        let response = send_google_request_mode(
            transport,
            host,
            provider,
            field(request, "model"),
            &body,
            options,
            GoogleSendMode::Complete { deadline },
        )
        .await
        .map_err(|(error, before)| {
            before_abort = before;
            error
        })?;
        before_abort = response.body().1;
        let read_options = options.clone();
        let provider = provider.clone();
        let read = ExchangeFuture::io(async move {
            if !response.status().is_success() {
                return Err(DriverFailure::Model(
                    google_response_error(response, &read_options, &provider).await?,
                ));
            }
            let (mut body, before) = response.into_body();
            let text = response_text(&mut body, &read_options, before).await?;
            let wire = providers::google::parse_error_body(&text)?;
            // The SDK assigns sdkHttpResponse before mapping the JSON. Arrays
            // accept the assignment; null and primitives throw before mapping.
            sdk_response_object(&wire)?;
            Ok(providers::google::sdk_response(&wire))
        });
        match exchange_prearmed_continuing(
            read,
            wait_abort(&options.abort),
            || options.abort.as_ref().and_then(AbortFlag::abort_event_at),
            deadline,
            before_abort,
        )
        .await
        {
            Exchange::Response(result, before) => {
                before_abort = before;
                result
            }
            Exchange::Aborted | Exchange::Timeout { .. } => Err(ModelStreamError::abort().into()),
        }
    }
    .await;
    match outcome {
        Ok(raw) => Ok(parse_response(raw, field(provider, "id"))),
        Err(DriverFailure::Host(error)) => Err(DriverFailure::Host(error)),
        Err(DriverFailure::Model(error)) => {
            let error = throw_if_google_abort(error, options, before_abort)?;
            match error {
                ModelStreamError::Provider(_) => Err(error.into()),
                other => Err(ModelStreamError::Provider(normalize_model_error(
                    &provider_id(provider),
                    &"google".into(),
                    ErrorInput::Error(&other.as_js_error()),
                    None,
                ))
                .into()),
            }
        }
    }
}

fn sdk_response_object(wire: &JsValue) -> Result<(), ModelStreamError> {
    let kind = match wire {
        JsValue::Object(_) | JsValue::Array(_) => return Ok(()),
        JsValue::Null => {
            return Err(ModelStreamError::thrown(
                "TypeError",
                "Cannot set properties of null (setting 'sdkHttpResponse')",
            ));
        }
        JsValue::String(_) => "string",
        JsValue::Number(_) => "number",
        JsValue::Bool(_) => "boolean",
        JsValue::Undefined => "undefined",
    };
    let value =
        pilotdeck_model_core::codec::js::to_string_units(wire).expect("scalar string coercion");
    let mut message =
        JsString::from(format!("Cannot create property 'sdkHttpResponse' on {kind} '").as_str())
            .units()
            .to_vec();
    message.extend_from_slice(&value);
    message.push(u16::from(b'\''));
    Err(ModelStreamError::thrown(
        "TypeError",
        JsString::from_utf16(message),
    ))
}

fn safe_id(value: &JsString) -> String {
    let raw = String::from_utf16_lossy(value.units());
    let mut id = String::new();
    let mut replaced = false;
    for ch in pilotdeck_model_core::codec::metadata::js_trim(&raw).chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            id.push(ch);
            replaced = false;
        } else if !replaced {
            id.push('_');
            replaced = true;
        }
    }
    let id = id.trim_matches('_');
    if id.is_empty() {
        "google".into()
    } else {
        id.into()
    }
}

fn parse_response(raw: JsValue, provider: &JsValue) -> JsValue {
    let undefined = JsValue::Undefined;
    let candidate = match field(&raw, "candidates") {
        JsValue::Array(candidates) => candidates.first().unwrap_or(&undefined),
        _ => &undefined,
    };
    let parts = match field(field(candidate, "content"), "parts") {
        JsValue::Array(parts) => parts.as_slice(),
        _ => &[],
    };
    let base = field(&raw, "responseId")
        .as_string()
        .cloned()
        .unwrap_or_else(|| "google_response".into());
    let base = safe_id(&base);
    let mut used = Vec::new();
    let mut content = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        if let Some(text) = field(part, "text").as_string() {
            if field(part, "thought") == &JsValue::Bool(true) {
                let mut thinking = object([
                    ("type", "thinking".into()),
                    ("text", JsValue::String(text.clone())),
                ]);
                if let Some(signature) = non_empty_string(field(part, "thoughtSignature"))
                    && let JsValue::Object(thinking) = &mut thinking
                {
                    thinking.insert("signature".into(), JsValue::String(signature.clone()));
                }
                content.push(thinking);
            } else if !text.units().is_empty() {
                content.push(object([
                    ("type", "text".into()),
                    ("text", JsValue::String(text.clone())),
                ]));
            }
            continue;
        }
        let function = as_record(field(part, "functionCall"));
        if !function.as_object().is_some_and(|value| !value.is_empty()) {
            continue;
        }
        let incoming = non_empty_string(field(function, "id"));
        let id = incoming
            .map(safe_id)
            .unwrap_or_else(|| format!("call_{base}_{index}"));
        let mut unique = id.clone();
        let mut suffix = 2;
        while used.contains(&unique) {
            unique = format!("{id}_{suffix}");
            suffix += 1;
        }
        used.push(unique.clone());
        content.push(object([
            ("type", "tool_call".into()),
            ("id", unique.as_str().into()),
            ("name", string_or_empty(field(function, "name"))),
            ("input", as_record(field(function, "args")).clone()),
            (
                "raw",
                object([
                    ("provider", provider.clone()),
                    ("functionCall", function.clone()),
                ]),
            ),
        ]));
    }
    let mut response = JsObject::new();
    response.insert("role".into(), "assistant".into());
    response.insert("content".into(), JsValue::Array(content));
    response.insert(
        "usage".into(),
        providers::google::normalize_usage(field(&raw, "usageMetadata"))
            .unwrap_or(JsValue::Undefined),
    );
    response.insert(
        "finishReason".into(),
        providers::google::normalize_finish_reason(field(candidate, "finishReason")).into(),
    );
    response.insert("raw".into(), raw);
    JsValue::Object(response)
}
