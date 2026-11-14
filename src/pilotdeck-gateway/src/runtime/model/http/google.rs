use bytes::Bytes;
use hyper::Response;
use pilotdeck_protocol::{JsObject, JsString, JsValue};

use super::{
    AbortFlag, Checkpoint, DumpFailure, ErrorInput, JsError, ModelBody, ModelHttpTransport,
    ModelStreamError, StreamGuard, StreamOptions, StreamOutput, aborted, build_http_request,
    continuation_request, delay, dump_request_to_file, duration, error_event, field,
    idle_timeout_ms, is_retryable_stream_error, js_round, normalize_model_error, object_entries,
    protocol, provider_exchange, provider_id, providers, read_frame, request_started_event,
    response_text, retry_decide, retryable_message, sse, sse_error, sse_limit, throw_if_aborted,
    wait_abort,
};

/// `toProviderError(provider, <fs error>)`: the canonical error the TS Google
/// catch yields for a failed dump `writeFileSync` — no SDK send, no retry, one
/// non-retryable `error` event (`streamModel.ts:395-401` → `:461-495`).
pub(super) fn dump_failure_error(provider: &JsValue, failure: &DumpFailure) -> ModelStreamError {
    // The fs Error's own fields, as Node's `JSON.stringify` projects them.
    let mut raw = JsObject::new();
    if let Some(errno) = failure.errno {
        raw.insert("errno".into(), JsValue::Number(f64::from(errno)));
    }
    if let Some(code) = failure.code {
        raw.insert("code".into(), code.into());
    }
    raw.insert("syscall".into(), failure.syscall.into());
    if let Some(path) = &failure.path {
        raw.insert(
            "path".into(),
            JsValue::String(JsString::from(path.display().to_string().as_str())),
        );
    }
    let mut canonical = normalize_model_error(
        &provider_id(provider),
        &protocol(provider),
        ErrorInput::Error(&JsError {
            message: JsString::from(failure.message().as_str()),
            code: failure.code.map(JsString::from),
            network_code: None,
            raw: JsValue::Object(raw),
        }),
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

/// The SDK's `HttpResponse.headers`: `Object.fromEntries`-style copy of
/// `Response.headers.entries()` — names lowercased and sorted, repeated
/// values joined with `", "`, and `set-cookie` listed once per value (so the
/// object keeps the last one). Values are byte strings (latin-1).
pub(super) fn sdk_response_headers(headers: &hyper::HeaderMap) -> JsValue {
    let mut names: Vec<&hyper::header::HeaderName> = headers.keys().collect();
    names.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let latin1 = |value: &hyper::header::HeaderValue| -> String {
        value.as_bytes().iter().map(|&b| char::from(b)).collect()
    };
    let mut out = JsObject::new();
    for name in names {
        let values: Vec<String> = headers.get_all(name).iter().map(latin1).collect();
        let value = if name == hyper::header::SET_COOKIE {
            values.last().cloned().unwrap_or_default()
        } else {
            values.join(", ")
        };
        out.insert(name.as_str().into(), value.as_str().into());
    }
    JsValue::Object(out)
}

/// `sendGoogleCompleteRequest`/`generateContentStream` transport for the SDK:
/// one URL, SDK headers, SDK `extraBody` deep merge. Transport failures are
/// plain `TypeError: fetch failed` (the SDK uses bare fetch, not networkFetch).
/// The SDK calls Node's global `fetch`, so the wire header block is undici's
/// (`host`, `connection`, the SDK headers, then the Fetch defaults it did not
/// set). `stream == false` is `models.generateContent`: its endpoint, and the
/// provider `timeoutMs` as the SDK's `httpOptions.timeout` (an
/// `X-Server-Timeout` header and an abort of the whole request; no timeout
/// when unset). Streaming clears the SDK timeout and keeps the idle watchdog.
pub(super) async fn send_google_request(
    transport: &dyn ModelHttpTransport,
    provider: &JsValue,
    model: &JsValue,
    body: &JsValue,
    options: &StreamOptions,
    stream: bool,
) -> Result<Response<(ModelBody, bool)>, (ModelStreamError, bool)> {
    let sdk_timeout_ms = if stream {
        None
    } else {
        field(provider, "timeoutMs")
            .as_number()
            .filter(|ms| *ms > 0.0)
    };
    let final_body = providers::google::final_body(body, provider);
    let headers =
        providers::google::headers(provider, sdk_timeout_ms).map_err(|error| (error, false))?;
    let encoded = crate::js_value::json_stringify(&final_body)
        .map_err(|_| {
            (
                ModelStreamError::thrown("RangeError", crate::js_value::StackOverflow::MESSAGE),
                false,
            )
        })?
        .unwrap_or_default();
    let endpoint =
        providers::google::endpoint(provider, model, stream).map_err(|error| (error, false))?;
    let (url, header_map) = build_http_request(&endpoint, &headers)
        .map_err(|error| (ModelStreamError::thrown("TypeError", error.message), false))?;
    let idle_ms = idle_timeout_ms(provider, options);
    let deadline = if stream {
        Some(tokio::time::Instant::now() + duration(idle_ms))
    } else {
        sdk_timeout_ms.map(|ms| tokio::time::Instant::now() + duration(ms))
    };
    crate::fetch_redirect::follow(
        url,
        hyper::Method::POST,
        header_map,
        Bytes::from(encoded),
        || (ModelStreamError::thrown("TypeError", "fetch failed"), false),
        |mut request| async move {
            let wire =
                crate::fetch_redirect::node_fetch_wire_headers(request.uri(), request.headers());
            *request.headers_mut() = wire;
            provider_exchange(
                transport,
                request,
                options,
                deadline,
                |failure| ModelStreamError::thrown("TypeError", failure.message),
                || {
                    if stream {
                        ModelStreamError::thrown(
                            "StreamIdleTimeoutError",
                            format!(
                                "Stream idle timeout: no data received for {}ms",
                                crate::js_value::number_string(idle_ms)
                            ),
                        )
                    } else {
                        // The SDK's own timer aborts its controller: fetch
                        // rejects with an AbortError.
                        ModelStreamError::abort()
                    }
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
pub(super) async fn google_response_error(
    response: Response<(ModelBody, bool)>,
    options: &StreamOptions,
    provider: &JsValue,
) -> Result<ModelStreamError, ModelStreamError> {
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

/// One `streamGoogleProviderRequest` attempt: send, then consume the SDK SSE
/// stream. Returns as soon as a `message_end` event is emitted, like the TS
/// loop's `stream.return(...)`. `dump` is the `PILOTDECK_DUMP_REQUEST` body.
#[allow(clippy::too_many_arguments)]
async fn google_attempt(
    transport: &dyn ModelHttpTransport,
    provider: &JsValue,
    model: &JsValue,
    body: &JsValue,
    dump: Option<&JsValue>,
    options: &StreamOptions,
    sink: &mut (dyn FnMut(StreamOutput) + Send),
    checkpoint: &mut Checkpoint,
) -> (Result<(), ModelStreamError>, bool) {
    // Error normalization is also a queued continuation: a later parent timer
    // must not replace an already-read SDK error with AbortError.
    let mut body_before_abort = false;
    let sdk_deadline = tokio::time::Instant::now() + duration(idle_timeout_ms(provider, options));
    let outcome = async {
        // PILOTDECK_DUMP_REQUEST (streamModel.ts:395-402): the streaming
        // attempt writes the SDK parameters (`dump`, built by the caller only
        // when the switch is on) before creating the SDK client; `complete()`
        // never dumps. Google carries its model in the URL, so the log line's
        // model is the canonical request's. A failed write lands in the
        // attempt catch → `toProviderError` → one non-retryable error event,
        // and the SDK send never happens (probe: 0 server hits with a dead
        // TMPDIR, `:461-495`).
        if let Some(dump) = dump
            && let Err(failure) = dump_request_to_file(dump, model)
        {
            return Err(dump_failure_error(provider, &failure));
        }
        let response = send_google_request(transport, provider, model, body, options, true)
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
            return Err(ModelStreamError::thrown("Error", "Response body is empty"));
        }
        if !(200..300).contains(&status) {
            use crate::runtime::network::{Exchange, exchange_prearmed_continuing};
            let idle_ms = idle_timeout_ms(provider, options);
            let provider = provider.clone();
            let read_options = options.clone();
            let read = crate::runtime::network::ExchangeFuture::io(async move {
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
                    Err(result?)
                }
                Exchange::Aborted => Err(ModelStreamError::abort()),
                Exchange::Timeout { before_abort } => {
                    body_before_abort = before_abort;
                    Err(ModelStreamError::thrown(
                        "StreamIdleTimeoutError",
                        format!(
                            "Stream idle timeout: no data received for {}ms",
                            crate::js_value::number_string(idle_ms)
                        ),
                    ))
                }
            };
        }
        let sdk_headers = sdk_response_headers(response.headers());
        let (mut body, _) = response.into_body();
        let mut state = providers::google::StreamState::with_response_headers(sdk_headers);
        let idle_ms = idle_timeout_ms(provider, options);
        let mut guard = StreamGuard::new(provider, (options.clock)());
        let mut buffer = sse::SseBuffer::with_limit(sse_limit());
        loop {
            if !body_before_abort {
                throw_if_aborted(options)?;
            }
            let chunk = read_frame(&mut body, idle_ms, options, &mut body_before_abort).await?;
            if !body_before_abort {
                throw_if_aborted(options)?;
            }
            let parsed = match chunk {
                Some(ref bytes) => buffer.push(bytes),
                None => buffer.finish(),
            };
            for item in parsed {
                let item = item.map_err(sse_error)?;
                guard.check_duration((options.clock)())?;
                let raw = match item {
                    sse::SseEvent::Done => continue,
                    sse::SseEvent::Data(raw) => raw,
                };
                for event in state.normalize(&raw)? {
                    let tag = field(&event, "type");
                    if tag.as_string().is_some_and(|tag| tag.equals_str("error")) {
                        return Err(ModelStreamError::Provider(object_entries(field(
                            &event, "error",
                        ))));
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
            ));
        }
        Ok(())
    }
    .await;
    (outcome, body_before_abort)
}

/// `toProviderError(provider, error)`: provider errors pass through, anything
/// else is normalized without a status (the SDK's thrown values here carry
/// none: `TypeError`, `SyntaxError`, ...).
fn to_provider_error(provider: &JsValue, error: ModelStreamError) -> ModelStreamError {
    match error {
        ModelStreamError::Provider(_) => error,
        other => ModelStreamError::Provider(normalize_model_error(
            &provider_id(provider),
            &"google".into(),
            ErrorInput::Error(&other.as_js_error()),
            None,
        )),
    }
}

/// One `sendGoogleCompleteRequest` (`streamModel.ts:349-364`): the SDK's
/// `models.generateContent` — POST `:generateContent`, `throwErrorIfNotOK`,
/// `response.json()`, `sdkHttpResponse` — then `throwIfGoogleAbort` and
/// `toProviderError` on any failure. Returns the SDK response object.
pub(super) async fn google_complete_attempt(
    transport: &dyn ModelHttpTransport,
    provider: &JsValue,
    request: &JsValue,
    model: &JsValue,
    options: &StreamOptions,
) -> Result<JsValue, ModelStreamError> {
    let mut before_abort = false;
    let outcome = async {
        let body = providers::google::build_request(request, model)?;
        let model_id = field(request, "model").clone();
        let response = send_google_request(transport, provider, &model_id, &body, options, false)
            .await
            .map_err(|(error, early)| {
                before_abort = early;
                error
            })?;
        before_abort = response.body().1;
        if !(200..300).contains(&response.status().as_u16()) {
            return Err(google_response_error(response, options, provider).await?);
        }
        let headers = sdk_response_headers(response.headers());
        let (mut body, early) = response.into_body();
        let text = response_text(&mut body, options, early).await?;
        let wire = providers::google::parse_error_body(&text)?;
        Ok(providers::google::sdk_complete_response(&wire, headers))
    }
    .await;
    match outcome {
        Ok(raw) => Ok(raw),
        Err(error) => match throw_if_google_abort(error, options, before_abort) {
            Ok(error) => Err(to_provider_error(provider, error)),
            Err(abort) => Err(abort),
        },
    }
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
pub(super) async fn stream_google_model(
    transport: &dyn ModelHttpTransport,
    request: &JsValue,
    config: &JsValue,
    options: &StreamOptions,
    sink: &mut (dyn FnMut(StreamOutput) + Send),
    provider: &JsValue,
) -> Result<(), ModelStreamError> {
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
        // The dump is the pre-SDK `{model, contents, config}`; the wire body
        // is what goes out. Both come from the same checks, so the dump build
        // cannot fail once the wire body was built.
        let built = providers::google::build_request(&current, &model).and_then(|body| {
            let dump = if std::env::var("PILOTDECK_DUMP_REQUEST").as_deref() == Ok("1") {
                Some(providers::google::sdk_parameters_for_dump(
                    &current, &model,
                )?)
            } else {
                None
            };
            Ok((body, dump))
        });
        let (outcome, before_abort) = match built {
            Ok((body, dump)) => {
                google_attempt(
                    transport,
                    provider,
                    &model_id,
                    &body,
                    dump.as_ref(),
                    options,
                    sink,
                    &mut checkpoint,
                )
                .await
            }
            Err(error) => (Err(error), false),
        };
        let Err(error) = outcome else {
            return Ok(());
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
            current = continuation_request(&current, &checkpoint.partial_text);
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
