use crate::driver_transport::HostOperationError;
use bytes::Bytes;
use hyper::Response;
use pilotdeck_protocol::{JsObject, JsValue};

use super::{
    AbortFlag, Checkpoint, DriverFailure, ErrorInput, HeaderProjection, InvocationResponse,
    JsError, ModelBody, ModelHostServices, ModelHttpTransport, ModelStreamError, RequestMode,
    StreamGuard, StreamOptions, StreamOutput, aborted, begin_invocation_body, continuation_request,
    delay, duration, error_event, field, finish_invocation, idle_timeout_ms,
    is_retryable_stream_error, js_round, normalize_model_error, object_entries, protocol,
    provider_exchange, provider_id, providers, read_frame, request_started_event, response_text,
    retry_decide, retryable_message, sse, sse_error, sse_limit, throw_if_aborted, wait_abort,
};

/// `toProviderError(provider, <fs error>)`: the canonical error the TS Google
/// catch yields for a failed dump `writeFileSync` — no SDK send, no retry, one
/// non-retryable `error` event (`streamModel.ts:395-401` → `:461-495`).
pub fn dump_failure_error(provider: &JsValue, failure: &JsError) -> ModelStreamError {
    let mut canonical = normalize_model_error(
        &provider_id(provider),
        &protocol(provider),
        ErrorInput::Error(failure),
        None,
    );
    // A dump write failure never carries a retryable code or status in TS
    // (`is_retryable(None, "ENOENT")` is false); pin the same for the
    // unmapped-errno and candidate-exhaustion fallbacks, whose
    // message-derived codes could otherwise look retryable.
    canonical.insert("retryable".into(), JsValue::Bool(false));
    ModelStreamError::Provider(canonical)
}

// ---------------------------------------------------------------------------
// Google protocol (`streamGoogleProviderRequest` in streamModel.ts)

/// `sendGoogleCompleteRequest`/`generateContentStream` transport for the SDK:
/// one URL, SDK headers, SDK `extraBody` deep merge. Transport failures are
/// plain `TypeError: fetch failed` (the SDK uses bare fetch, not networkFetch).
pub(super) async fn send_google_request<S: ModelHostServices + ?Sized>(
    transport: &dyn ModelHttpTransport<Rejection = S::Rejection, Preparation = S::Preparation>,
    host: &S,
    provider: &JsValue,
    model: &JsValue,
    body: &JsValue,
    options: &StreamOptions,
) -> Result<Response<(ModelBody<S::Rejection>, bool)>, (DriverFailure<S::Rejection>, bool)> {
    send_google_request_mode(
        transport,
        host,
        provider,
        model,
        body,
        options,
        GoogleSendMode::Stream,
    )
    .await
}

pub(super) enum GoogleSendMode {
    Stream,
    Complete {
        deadline: Option<tokio::time::Instant>,
    },
}

/// The complete caller owns the SDK's provider timeout across both headers and
/// body. Streaming retains its existing idle deadline and request dump behavior.
pub(super) async fn send_google_request_mode<S: ModelHostServices + ?Sized>(
    transport: &dyn ModelHttpTransport<Rejection = S::Rejection, Preparation = S::Preparation>,
    host: &S,
    provider: &JsValue,
    model: &JsValue,
    body: &JsValue,
    options: &StreamOptions,
    mode: GoogleSendMode,
) -> Result<Response<(ModelBody<S::Rejection>, bool)>, (DriverFailure<S::Rejection>, bool)> {
    let (stream, complete_deadline) = match mode {
        GoogleSendMode::Stream => (true, None),
        GoogleSendMode::Complete { deadline } => (false, deadline),
    };
    let timeout_ms = idle_timeout_ms(provider, options);
    let final_body = providers::google::final_body(body, provider);
    let headers = host
        .materialize(
            provider,
            &final_body,
            if stream {
                RequestMode::Stream
            } else {
                RequestMode::Complete
            },
        )
        .map_err(|error| (DriverFailure::from(error), false))?;
    let encoded = pilotdeck_model_core::codec::js::json_stringify(&final_body)
        .map_err(|_| {
            (
                ModelStreamError::thrown(
                    "RangeError",
                    pilotdeck_model_core::codec::js::StackOverflow::MESSAGE,
                )
                .into(),
                false,
            )
        })?
        .unwrap_or_default();
    // PILOTDECK_DUMP_REQUEST: debug aid that writes the provider request body
    // to a file under the OS temp directory before the SDK send
    // (TS `streamModel.ts:395-401`). Google carries its model in the URL, so
    // the log line's model comes from the canonical request, not the body.
    if stream {
        // A failed write surfaces like TS: the throw lands in the Google
        // attempt catch → `toProviderError` → one non-retryable error event,
        // and the SDK send never happens (probe: 0 server hits with a
        // dead TMPDIR, `streamModel.ts:395-401` → `:461-495`).
        match host.dump_request_if_enabled(&final_body, model) {
            Ok(()) => {}
            Err(HostOperationError::Original(failure)) => {
                return Err((dump_failure_error(provider, &failure).into(), false));
            }
            Err(HostOperationError::Rejected(error)) => {
                return Err((DriverFailure::Host(error), false));
            }
        }
    }
    let endpoint = if stream {
        providers::google::endpoint(provider, model)
    } else {
        providers::google::endpoint_with_stream(provider, model, false)
    }
    .map_err(|error| (DriverFailure::Model(error), false))?;
    let HeaderProjection {
        preparation,
        url,
        headers: header_map,
    } = headers.validate_url(&endpoint).map_err(|error| {
        (
            error.map_original(|error| ModelStreamError::thrown("TypeError", error.message)),
            false,
        )
    })?;
    let deadline = if stream {
        Some(tokio::time::Instant::now() + duration(timeout_ms))
    } else {
        complete_deadline
    };
    crate::fetch_redirect::follow_with_context(
        url,
        hyper::Method::POST,
        header_map,
        Bytes::from(encoded),
        preparation,
        || {
            (
                DriverFailure::Model(ModelStreamError::thrown("TypeError", "fetch failed")),
                false,
            )
        },
        |request, preparation| async move {
            provider_exchange(
                transport,
                request,
                preparation,
                options,
                deadline,
                |failure| ModelStreamError::thrown("TypeError", failure.message),
                || {
                    if !stream {
                        return ModelStreamError::abort();
                    }
                    ModelStreamError::thrown(
                        "StreamIdleTimeoutError",
                        format!(
                            "Stream idle timeout: no data received for {}ms",
                            pilotdeck_model_core::codec::js::number_string(timeout_ms)
                        ),
                    )
                },
            )
            .await
        },
    )
    .await
}

/// `throwErrorIfNotOK` + `toProviderError` for the SDK. A JSON content type is
/// parsed like `Response.json()`; any other content type is synthesized into
/// `{error:{message,code,status}}` before `JSON.stringify`.
pub(super) async fn google_response_error<R: Send + 'static>(
    response: Response<(ModelBody<R>, bool)>,
    options: &StreamOptions,
    provider: &JsValue,
) -> Result<ModelStreamError, DriverFailure<R>> {
    let status = response.status().as_u16();
    // Hyper retains non-canonical and explicitly empty statusText in this
    // extension. An absent extension means the canonical reason phrase.
    let status_text = response
        .extensions()
        .get::<hyper::ext::ReasonPhrase>()
        .map(|reason| String::from_utf8_lossy(reason.as_bytes()).into_owned())
        .unwrap_or_else(|| {
            response
                .status()
                .canonical_reason()
                .unwrap_or_default()
                .to_string()
        });
    // The SDK uses case-sensitive `includes("application/json")` on the value.
    let content_type = response
        .headers()
        .get("content-type")
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
        .unwrap_or_default();
    let (mut body, before_abort) = response.into_body();
    let text = response_text(&mut body, options, before_abort).await?;
    let error_body = if content_type.contains("application/json") {
        providers::google::parse_error_body(&text)?
    } else {
        providers::google::synthesized_error_body(&text, status, &status_text)
    };
    let message = providers::google::api_error_message(&error_body)?;
    let js_error = JsError {
        message,
        code: None,
        network_code: None,
        raw: JsValue::Object(providers::google::api_error_raw(status)),
    };
    let canonical = normalize_model_error(
        &provider_id(provider),
        &"google".into(),
        ErrorInput::Error(&js_error),
        Some(f64::from(status)),
    );
    Ok(ModelStreamError::Provider(canonical))
}

/// The audit records the public SDK parameters, rather than its HTTP wire body.
fn invocation_request(request: &JsValue, wire: &JsValue) -> JsValue {
    let mut config = JsObject::new();
    let generation = field(wire, "generationConfig");
    config.insert(
        "maxOutputTokens".into(),
        field(generation, "maxOutputTokens").clone(),
    );
    let prompt = field(request, "systemPrompt");
    if pilotdeck_model_core::codec::projection::truthy(prompt) {
        let mut instruction = JsObject::new();
        instruction.insert("text".into(), prompt.clone());
        config.insert("systemInstruction".into(), JsValue::Object(instruction));
    }
    let mut automatic = JsObject::new();
    automatic.insert("disable".into(), JsValue::Bool(true));
    config.insert(
        "automaticFunctionCalling".into(),
        JsValue::Object(automatic),
    );
    for key in ["tools", "toolConfig"] {
        if !field(wire, key).is_undefined() {
            config.insert(key.into(), field(wire, key).clone());
        }
    }
    for key in ["thinkingConfig", "responseMimeType", "responseJsonSchema"] {
        if !field(generation, key).is_undefined() {
            config.insert(key.into(), field(generation, key).clone());
        }
    }
    // JSON.stringify(AbortSignal) is {}. Every Google stream owns a signal.
    config.insert("abortSignal".into(), JsValue::Object(JsObject::new()));
    let mut body = JsObject::new();
    let model = field(request, "model")
        .as_string()
        .map(|s| String::from_utf16_lossy(s.units()))
        .unwrap_or_default();
    body.insert(
        "model".into(),
        JsValue::String(providers::google::normalize_model_id(&model).into()),
    );
    body.insert("contents".into(), field(wire, "contents").clone());
    body.insert("config".into(), JsValue::Object(config));
    JsValue::Object(body)
}

struct GoogleAttempt<'a> {
    model: &'a JsValue,
    wire: &'a JsValue,
    audit: JsValue,
    number: f64,
}

/// One `streamGoogleProviderRequest` attempt: send, then consume the SDK SSE
/// stream. Returns as soon as a `message_end` event is emitted, like the TS
/// loop's `stream.return(...)`.
async fn google_attempt<S: ModelHostServices + ?Sized>(
    transport: &dyn ModelHttpTransport<Rejection = S::Rejection, Preparation = S::Preparation>,
    host: &S,
    provider: &JsValue,
    request: GoogleAttempt<'_>,
    options: &StreamOptions,
    sink: &mut (dyn FnMut(StreamOutput) + Send),
    checkpoint: &mut Checkpoint,
) -> (Result<(), DriverFailure<S::Rejection>>, bool) {
    let model = request.model;
    // Error normalization is also a queued continuation: a later parent timer
    // must not replace an already-read SDK error with AbortError.
    let mut audit_body = object_entries(&request.audit);
    if let Some(extra) = field(provider, "extraBody").as_object() {
        for (key, value) in extra.own_entries() {
            audit_body.insert(key.clone(), value.clone());
        }
    }
    let mut invocation = match begin_invocation_body(
        host,
        options,
        provider,
        model,
        true,
        &JsValue::Object(audit_body),
        request.number,
    ) {
        Ok(state) => state,
        Err(error) => return (Err(error.into()), false),
    };
    let mut raw_response = String::new();
    let mut body_before_abort = false;
    let sdk_deadline = tokio::time::Instant::now() + duration(idle_timeout_ms(provider, options));
    let outcome = async {
        let response = send_google_request(transport, host, provider, model, request.wire, options)
            .await
            .map_err(|(error, before_abort)| {
                body_before_abort = before_abort;
                error
            })?;
        body_before_abort = response.body().1;
        let status = response.status().as_u16();
        if matches!(status, 204 | 205) {
            // fetch exposes a null body for null-body statuses; the SDK's stream
            // reader throws before yielding any chunk.
            return Err(ModelStreamError::thrown("Error", "Response body is empty").into());
        }
        if !(200..300).contains(&status) {
            use crate::network::{Exchange, exchange_prearmed_continuing};
            let idle_ms = idle_timeout_ms(provider, options);
            let provider = provider.clone();
            let read_options = options.clone();
            let read = crate::network::ExchangeFuture::io(async move {
                google_response_error(response, &read_options, &provider).await
            });
            // Preserve the SDK error-body promise's catch continuation.
            return match exchange_prearmed_continuing(
                read,
                wait_abort(&options.abort),
                || options.abort.as_ref().and_then(AbortFlag::abort_event_at),
                Some(sdk_deadline),
                body_before_abort,
            )
            .await
            {
                Exchange::Response(result, before_abort) => {
                    body_before_abort = before_abort;
                    Err(result?.into())
                }
                Exchange::Aborted => Err(ModelStreamError::abort().into()),
                Exchange::Timeout { before_abort } => {
                    body_before_abort = before_abort;
                    Err(ModelStreamError::thrown(
                        "StreamIdleTimeoutError",
                        format!(
                            "Stream idle timeout: no data received for {}ms",
                            pilotdeck_model_core::codec::js::number_string(idle_ms)
                        ),
                    )
                    .into())
                }
            };
        }
        // Fetch Headers iteration is lexically ordered and lower-cased.
        let mut sorted = std::collections::BTreeMap::<String, String>::new();
        for (name, value) in response.headers() {
            let value = String::from_utf8_lossy(value.as_bytes());
            sorted
                .entry(name.as_str().to_owned())
                .and_modify(|old| {
                    old.push_str(", ");
                    old.push_str(&value);
                })
                .or_insert_with(|| value.into_owned());
        }
        let mut headers = JsObject::new();
        for (name, value) in sorted {
            headers.insert(name.into(), JsValue::String(value.into()));
        }
        let mut http_response = JsObject::new();
        http_response.insert("headers".into(), JsValue::Object(headers));
        let (mut body, _) = response.into_body();
        let mut state = providers::google::StreamState::default();
        let idle_ms = idle_timeout_ms(provider, options);
        let mut guard = StreamGuard::new(
            provider,
            (options.clock)(),
            host.streaming_duration_limit().as_deref(),
        );
        let mut buffer = sse::SseBuffer::with_limit(sse_limit());
        loop {
            if !body_before_abort {
                throw_if_aborted(options)?;
            }
            let chunk = read_frame(&mut body, idle_ms, options, &mut body_before_abort).await?;
            let parsed = match chunk {
                Some(ref bytes) => buffer.push(bytes),
                None => buffer.finish(),
            };
            for item in parsed {
                let item = item.map_err(sse_error)?;
                let raw = match item {
                    sse::SseEvent::Done => continue,
                    sse::SseEvent::Data(raw) => raw,
                };
                if invocation.is_some() {
                    let mut chunk = object_entries(&providers::google::sdk_response(&raw));
                    chunk.insert(
                        "sdkHttpResponse".into(),
                        JsValue::Object(http_response.clone()),
                    );
                    let encoded =
                        pilotdeck_model_core::codec::js::json_stringify(&JsValue::Object(chunk))
                            .map_err(|_| {
                                ModelStreamError::thrown(
                                    "RangeError",
                                    pilotdeck_model_core::codec::js::StackOverflow::MESSAGE,
                                )
                            })?
                            .unwrap_or_default();
                    raw_response.push_str(&encoded);
                }
                if !body_before_abort {
                    throw_if_aborted(options)?;
                }
                guard.check_duration((options.clock)())?;
                for event in state.normalize(&raw)? {
                    let tag = field(&event, "type");
                    if tag.as_string().is_some_and(|tag| tag.equals_str("error")) {
                        return Err(ModelStreamError::Provider(object_entries(field(
                            &event, "error",
                        )))
                        .into());
                    }
                    let message_end = tag
                        .as_string()
                        .is_some_and(|tag| tag.equals_str("message_end"));
                    guard.observe(&event, (options.clock)())?;
                    checkpoint.on_event(&event)?;
                    sink(StreamOutput::Event(event));
                    options.yield_boundary().await;
                    if message_end {
                        return Ok(());
                    }
                }
            }
            if chunk.is_none() {
                break;
            }
        }
        guard.check_duration((options.clock)())?;
        if !state.ended() {
            return Err(ModelStreamError::thrown(
                "IncompleteStreamError",
                "Network stream ended before provider completion sentinel.",
            )
            .into());
        }
        Ok(())
    }
    .await;
    // Trusted rejection is not a provider failure. Record the actual incomplete
    // attempt through the owner's cleanup authority, then preserve the typed
    // rejection; resource release still requires the managed owner's receipt.
    if matches!(&outcome, Err(DriverFailure::Host(_))) {
        finish_invocation(
            host,
            &mut invocation,
            options,
            "incomplete",
            InvocationResponse {
                body: Some(raw_response.as_bytes()),
                status: None,
                complete: false,
            },
            None,
        );
        return (outcome, body_before_abort);
    }
    // TS throwIfGoogleAbort runs before finishInvocation. Preserve that
    // boundary: cancellation can leave the staged record pending; this change
    // only fills the missing normal/incomplete Google stream audit path.
    let abort_before_finish = outcome.as_ref().err().is_some_and(|error| {
        error.model().is_some_and(|error| error.is_abort())
            || (!body_before_abort && aborted(options))
    });
    if !abort_before_finish {
        finish_invocation(
            host,
            &mut invocation,
            options,
            if outcome.is_ok() {
                "success"
            } else {
                "incomplete"
            },
            InvocationResponse {
                body: Some(raw_response.as_bytes()),
                status: None,
                complete: outcome.is_ok(),
            },
            outcome.as_ref().err().and_then(DriverFailure::model),
        );
    }
    (outcome, body_before_abort)
}

/// `throwIfGoogleAbort`: preserve abort-like errors, while an observed parent
/// cancellation takes precedence. A non-abort error continues into retry handling.
pub(super) fn throw_if_google_abort(
    error: ModelStreamError,
    options: &StreamOptions,
    before_abort: bool,
) -> Result<ModelStreamError, ModelStreamError> {
    if !before_abort && aborted(options) {
        return Err(ModelStreamError::abort());
    }
    if error.is_abort() {
        return Err(error);
    }
    Ok(error)
}

/// `streamGoogleProviderRequest`: request_started per attempt, SDK error
/// canonicalization, continuation/network retries and error events.
pub(super) async fn stream_google_model<S: ModelHostServices + ?Sized>(
    transport: &dyn ModelHttpTransport<Rejection = S::Rejection, Preparation = S::Preparation>,
    host: &S,
    request: &JsValue,
    config: &JsValue,
    options: &StreamOptions,
    sink: &mut (dyn FnMut(StreamOutput) + Send),
    provider: &JsValue,
) -> Result<(), DriverFailure<S::Rejection>> {
    let mut current = request.clone();
    let (max_retries, _) = retry_decide(provider, 0.0, None, 0.0);
    let mut checkpoint = Checkpoint::default();
    let mut attempt = 0.0;
    let mut continuation_before_abort = false;
    while attempt <= max_retries {
        if !std::mem::take(&mut continuation_before_abort) {
            throw_if_aborted(options)?;
        }
        sink(StreamOutput::Event(request_started_event(
            provider, &current,
        )?));
        options.yield_boundary().await;

        let (_, model) = providers::validate_model_request(&current, config)?;
        let model_id = field(&current, "model").clone();
        let (outcome, before_abort) = match providers::google::build_request(&current, &model) {
            Ok(body) => {
                google_attempt(
                    transport,
                    host,
                    provider,
                    GoogleAttempt {
                        model: &model_id,
                        audit: invocation_request(&current, &body),
                        wire: &body,
                        number: attempt + 1.0,
                    },
                    options,
                    sink,
                    &mut checkpoint,
                )
                .await
            }
            Err(error) => (Err(error.into()), false),
        };
        let Err(error) = outcome else {
            return Ok(());
        };
        let error = match error {
            DriverFailure::Model(error) => error,
            DriverFailure::Host(error) => return Err(DriverFailure::Host(error)),
        };
        let error = throw_if_google_abort(error, options, before_abort)?;
        let canonical = match &error {
            ModelStreamError::Provider(canonical) => canonical.clone(),
            other => normalize_model_error(
                &provider_id(provider),
                &"google".into(),
                ErrorInput::Error(&other.as_js_error()),
                None,
            ),
        };
        let canonical_message = canonical
            .get("message")
            .and_then(JsValue::as_string)
            .map(|message| String::from_utf16_lossy(message.units()).to_lowercase())
            .unwrap_or_default();
        let retryable = canonical.get("retryable") == Some(&JsValue::Bool(true))
            || is_retryable_stream_error(&error)
            || retryable_message(&canonical_message);
        let current_model = field(&current, "model").clone();
        let progress =
            |sink: &mut (dyn FnMut(StreamOutput) + Send), reason: &str, delay_ms: f64| {
                let mut value = JsObject::new();
                value.insert("reason".into(), reason.into());
                value.insert("attempt".into(), JsValue::Number(attempt + 1.0));
                value.insert("maxAttempts".into(), JsValue::Number(max_retries));
                value.insert("delayMs".into(), JsValue::Number(js_round(delay_ms)));
                value.insert("provider".into(), field(provider, "id").clone());
                value.insert("model".into(), current_model.clone());
                sink(StreamOutput::RetryProgress(JsValue::Object(value)));
            };
        if attempt < max_retries && retryable && checkpoint.can_continue_text() {
            current = continuation_request(&current, checkpoint.partial_text());
            let (_, delay_ms) = retry_decide(provider, attempt, None, (options.random)());
            progress(sink, "continuation", delay_ms);
            continuation_before_abort = delay(delay_ms, options).await?;
            attempt += 1.0;
            continue;
        }
        let empty = checkpoint
            .interruption()
            .get("phase")
            .is_some_and(|phase| phase.as_string().is_some_and(|p| p.equals_str("empty")));
        if attempt < max_retries && retryable && empty {
            let (_, delay_ms) = retry_decide(provider, attempt, None, (options.random)());
            progress(sink, "network_error", delay_ms);
            continuation_before_abort = delay(delay_ms, options).await?;
            attempt += 1.0;
            continue;
        }
        let mut canonical = canonical;
        if retryable {
            canonical.insert("streamInterruption".into(), checkpoint.interruption());
        }
        sink(StreamOutput::Event(error_event(JsValue::Object(canonical))));
        options.yield_boundary().await;
        return Ok(());
    }
    Ok(())
}
