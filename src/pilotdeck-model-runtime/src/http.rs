//! Original PilotDeck async model driver, shared without a socket implementation.
#![allow(clippy::result_large_err)]
use crate::ModelStreamError;
use crate::driver_transport::{
    DriverBody as ModelBody, DriverBodyFailure as BodyFailure,
    DriverHttpTransport as ModelHttpTransport, DriverNetworkFailure as TransportFailure,
    HostOperationError,
};
use crate::headers::{DriverFailure, HeaderProjection, RequestMode};
use crate::network::Cancellation as AbortFlag;
use crate::providers::{self, StreamState};
use crate::services::{InvocationAuditSink, ModelHostServices};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, Response};
use pilotdeck_model_core::codec::normalize_error::{
    self as errors, ErrorInput, JsError, NetworkFetchError, is_premature_stream_close, js_round,
    normalize_model_error,
};
use pilotdeck_model_core::codec::projection::{field, is, object, put};
use pilotdeck_protocol::{JsObject, JsString, JsValue, parse_json};
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::watch;
mod complete;
mod google;
mod sse;
pub use complete::complete_model;
pub use google::dump_failure_error;
use google::stream_google_model;

fn wall_clock() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as f64)
        .unwrap_or(0.0)
}

pub enum StreamOutput {
    Event(JsValue),
    /// `onRetryProgress` payload.
    RetryProgress(JsValue),
}

#[derive(Clone)]
pub struct StreamOptions {
    pub abort: Option<AbortFlag>,
    /// `options.streamTimeoutMs`.
    pub stream_timeout_ms: Option<f64>,
    /// `Math.random` for retry jitter.
    pub random: Arc<dyn Fn() -> f64 + Send + Sync>,
    /// `Date.now` for Retry-After dates and the max-duration guard.
    pub clock: Arc<dyn Fn() -> f64 + Send + Sync>,
    /// undici `bodyTimeout` of the global dispatcher (PilotDeck installs
    /// 600000 ms in `cli/proxy.ts`): a body read that sees no bytes for this
    /// long fails with `TypeError: terminated`.
    /// Production always uses `UNDICI_BODY_TIMEOUT_MS`; undici's zero disables
    /// this timeout, but zero is not supported by our Node timer normalization.
    pub body_timeout_ms: f64,
    /// The consumer pull boundary of the module port that owns this stream.
    /// `None` for router-internal callers, whose sinks are driven to
    /// completion without a consumer pacing them.
    pub yield_gate: Option<YieldGate>,
    /// `options.invocation`: the gateway-owned raw provider audit.
    pub invocation: Option<InvocationAudit>,
}
impl Default for StreamOptions {
    fn default() -> Self {
        Self {
            abort: None,
            stream_timeout_ms: None,
            random: Arc::new(|| {
                (uuid::Uuid::new_v4().as_u128() >> 75) as f64 / (1u64 << 53) as f64
            }),
            clock: Arc::new(wall_clock),
            body_timeout_ms: UNDICI_BODY_TIMEOUT_MS,
            yield_gate: None,
            invocation: None,
        }
    }
}

/// `ModelRuntimeOptions.invocation` (streamModel.ts:37-41): the sink plus the
/// `InvocationLogContext` its records start with, in that key order.
#[derive(Clone)]
pub struct InvocationAudit {
    pub sink: Arc<dyn InvocationAuditSink>,
    pub context: JsObject,
}

/// One staged attempt (`InvocationAttempt`, streamModel.ts:866-875).
struct InvocationAttempt {
    request_log_id: JsString,
    request_id: JsString,
    started_at: String,
    request_body: String,
    attempt: f64,
    provider: JsValue,
    protocol: JsValue,
    model: JsValue,
    stream: bool,
}

/// `beginInvocation`: stage the attempt before the request is sent; a
/// staging failure keeps the request from going out.
fn begin_invocation<S: ModelHostServices + ?Sized>(
    host: &S,
    options: &StreamOptions,
    provider: &JsValue,
    model: &JsValue,
    stream: bool,
    body: &JsValue,
    attempt: f64,
) -> Result<Option<InvocationAttempt>, ModelStreamError> {
    begin_invocation_body(
        host,
        options,
        provider,
        model,
        stream,
        &providers::final_body(body, provider),
        attempt,
    )
}

/// Stage an already projected audit body. Google SDK audit parameters use a
/// shallow extraBody merge, independently of HTTP wire sanitization.
fn begin_invocation_body<S: ModelHostServices + ?Sized>(
    host: &S,
    options: &StreamOptions,
    provider: &JsValue,
    model: &JsValue,
    stream: bool,
    body: &JsValue,
    attempt: f64,
) -> Result<Option<InvocationAttempt>, ModelStreamError> {
    let Some(invocation) = &options.invocation else {
        return Ok(None);
    };
    let request_body = pilotdeck_model_core::codec::js::json_stringify(body)
        .map_err(|_| {
            ModelStreamError::thrown(
                "RangeError",
                pilotdeck_model_core::codec::js::StackOverflow::MESSAGE,
            )
        })?
        .unwrap_or_default();
    let state = InvocationAttempt {
        request_log_id: uuid::Uuid::new_v4().to_string().into(),
        request_id: uuid::Uuid::new_v4().to_string().into(),
        started_at: iso_now(host, options),
        request_body,
        attempt,
        provider: field(provider, "id").clone(),
        protocol: field(provider, "protocol").clone(),
        model: model.clone(),
        stream,
    };
    let mut record = invocation_head(invocation, &state);
    record.insert(
        "requestBytes".into(),
        JsValue::Number(state.request_body.len() as f64),
    );
    record.insert("outcome".into(), "success".into());
    record.insert("responseComplete".into(), JsValue::Bool(false));
    record.insert("startedAt".into(), state.started_at.as_str().into());
    record.insert("completedAt".into(), state.started_at.as_str().into());
    invocation
        .sink
        .stage(&record)
        .map_err(|error| ModelStreamError::thrown("Error", error.to_string()))?;
    Ok(Some(state))
}

fn iso_now<S: ModelHostServices + ?Sized>(host: &S, options: &StreamOptions) -> String {
    host.format_epoch_iso((options.clock)().floor() as i64)
}

/// The record prefix both stages share: the context, then the attempt.
fn invocation_head(invocation: &InvocationAudit, state: &InvocationAttempt) -> JsObject {
    let mut record = invocation.context.clone();
    record.insert(
        "requestLogId".into(),
        JsValue::String(state.request_log_id.clone()),
    );
    record.insert(
        "requestId".into(),
        JsValue::String(state.request_id.clone()),
    );
    record.insert("attempt".into(), JsValue::Number(state.attempt));
    record.insert("provider".into(), state.provider.clone());
    record.insert("protocol".into(), state.protocol.clone());
    record.insert("model".into(), state.model.clone());
    record.insert("stream".into(), JsValue::Bool(state.stream));
    record.insert("requestBody".into(), state.request_body.as_str().into());
    record
}

/// The attempt's settled response, for `finishInvocation`.
struct InvocationResponse<'a> {
    body: Option<&'a [u8]>,
    status: Option<u16>,
    complete: bool,
}

/// `finishInvocation`: append the terminal record once. An aborted signal
/// wins over the reported outcome, then a stream idle timeout. An append
/// failure is only reported, like the TS `.catch`.
fn finish_invocation<S: ModelHostServices + ?Sized>(
    host: &S,
    state: &mut Option<InvocationAttempt>,
    options: &StreamOptions,
    outcome: &str,
    response: InvocationResponse<'_>,
    error: Option<&ModelStreamError>,
) {
    let (Some(state), Some(invocation)) = (state.take(), &options.invocation) else {
        return;
    };
    let outcome = if aborted(options) {
        "aborted"
    } else if error.is_some_and(|error| error.name().equals_str("StreamIdleTimeoutError")) {
        "timeout"
    } else {
        outcome
    };
    let mut record = invocation_head(invocation, &state);
    if let Some(body) = response.body {
        record.insert(
            "responseBody".into(),
            String::from_utf8_lossy(body).as_ref().into(),
        );
    }
    record.insert(
        "requestBytes".into(),
        JsValue::Number(state.request_body.len() as f64),
    );
    if let Some(body) = response.body {
        record.insert("responseBytes".into(), JsValue::Number(body.len() as f64));
    }
    if let Some(status) = response.status {
        record.insert("httpStatus".into(), JsValue::Number(f64::from(status)));
    }
    record.insert("outcome".into(), outcome.into());
    record.insert("responseComplete".into(), JsValue::Bool(response.complete));
    record.insert("startedAt".into(), state.started_at.as_str().into());
    record.insert("completedAt".into(), iso_now(host, options).as_str().into());
    if let Err(error) = invocation.sink.append(&record) {
        let text = |key: &str| {
            invocation
                .context
                .get(key)
                .and_then(JsValue::as_string)
                .map(|value| String::from_utf16_lossy(value.units()))
                .unwrap_or_else(|| "unknown".into())
        };
        host.diagnostic(&format!(
            "[PilotDeck] failed to persist model invocation audit run={} turn={} requestLogId={} attempt={} {}",
            text("runId"),
            text("turnId"),
            String::from_utf16_lossy(state.request_log_id.units()),
            pilotdeck_model_core::codec::js::number_string(state.attempt),
            error
        ));
    }
}
impl StreamOptions {
    /// Suspend at the TS yield boundary: after an event becomes
    /// consumer-visible, the provider stream stays suspended until the
    /// consumer pulls it. A no-op without a gate or without outstanding
    /// events, so pre-content buffering never blocks. The abort flag also
    /// ends the suspension — TS registers the abort listener directly on the
    /// response body (`streamModel.ts`'s `cancelReader`), so cancellation
    /// tears the request down even while the generator is suspended at a
    /// yield; the transport's next read observes the flag and unwinds, which
    /// drops the connection like a cancelled reader. The post-termination
    /// side effects of the Router must not follow that release: they use
    /// [`Self::terminal_yield_boundary`], which only a pull opens.
    pub async fn yield_boundary(&self) {
        if let Some(gate) = &self.yield_gate {
            tokio::select! {
                () = gate.demand() => {}
                () = wait_abort(&self.abort) => {}
            }
        }
    }

    /// The Router's terminal boundary: like [`Self::yield_boundary`] it holds
    /// the code after the yield behind the pulls that consume the already
    /// delivered events, but a turn cancel does not release it. TS's
    /// `execute_failed`, `observeRequest` and the terminal error event run
    /// only when the generator resumes on a pull (`RouterRuntime.ts:962-998`
    /// sits behind the per-event yields of `:873-877`); a cancel only tears
    /// the provider connection down (`streamModel.ts`'s `cancelReader`) — it
    /// never advances a suspended generator past its yield, so publishing the
    /// failure without the consuming pulls would diverge from TS. The body
    /// task stays suspended here until the consumer pulls again or closes
    /// the stream, like a TS generator nobody pulls.
    pub async fn terminal_yield_boundary(&self) {
        if let Some(gate) = &self.yield_gate {
            gate.demand().await;
        }
    }
}

/// The TS generator pull boundary between a provider stream and its
/// consumer. A TS async generator suspends at every consumer-facing
/// `yield`, so provider reads, retry handling and terminal side effects
/// stay behind the pull that consumes the last delivered event
/// (`RouterRuntime.ts:775-808`). The push-based Rust port reproduces that
/// with two counters: the producer `deliver()`s when an event becomes
/// consumer-visible and `demand()`s the boundary before producing more;
/// the consumer `pull()`s when the consumer asks for the next event —
/// deferred past the queue-take to the start of the following `next()` call,
/// since a TS generator resumes from its `yield` only on the next pull.
#[derive(Clone, Default)]
pub struct YieldGate {
    /// `(delivered, pulled)`.
    state: Arc<watch::Sender<(u64, u64)>>,
}
impl YieldGate {
    pub fn new() -> Self {
        Self::default()
    }
    /// An event became consumer-visible (`execute`'s sink).
    pub fn deliver(&self) {
        self.state.send_if_modified(|state| {
            state.0 += 1;
            true
        });
    }
    /// The consumer took a delivered event.
    pub fn pull(&self) {
        self.state.send_if_modified(|state| {
            state.1 += 1;
            true
        });
    }
    /// Wait until every delivered event has been pulled.
    pub async fn demand(&self) {
        let mut receiver = self.state.subscribe();
        loop {
            {
                let state = receiver.borrow_and_update();
                if state.0 <= state.1 {
                    return;
                }
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

const DEFAULT_STREAM_IDLE_TIMEOUT_MS: f64 = UNDICI_BODY_TIMEOUT_MS;
/// `DEFAULT_REQUEST_TIMEOUT_MS` (`streamModel.ts:714`): the non-streaming
/// fallback when the provider has no `timeoutMs`.
const DEFAULT_REQUEST_TIMEOUT_MS: f64 = 600_000.0;
/// `UNDICI_TRANSPORT_TIMEOUT_MS` (`cli/proxy.ts`, = LITELLM_COMPLETION_HTTP_FALLBACK_MS).
pub use crate::network::UNDICI_BODY_TIMEOUT_MS;
use pilotdeck_model_core::driver::{continuation_request, retry_decide};

/// `resolveStreamIdleTimeout(provider, options)`.
pub fn idle_timeout_ms(provider: &JsValue, options: &StreamOptions) -> f64 {
    if let Some(ms) = options.stream_timeout_ms.filter(|ms| *ms > 0.0) {
        return ms;
    }
    match field(field(provider, "retry"), "streamIdleTimeoutMs") {
        JsValue::Number(ms) if *ms > 0.0 => *ms,
        _ => DEFAULT_STREAM_IDLE_TIMEOUT_MS,
    }
}

fn duration(ms: f64) -> Duration {
    crate::network::node_timer_duration(ms)
}

async fn wait_abort(abort: &Option<AbortFlag>) {
    match abort {
        Some(flag) => flag.cancelled().await,
        None => std::future::pending().await,
    }
}
fn aborted(options: &StreamOptions) -> bool {
    options.abort.as_ref().is_some_and(AbortFlag::is_aborted)
}
fn throw_if_aborted(options: &StreamOptions) -> Result<(), ModelStreamError> {
    if aborted(options) {
        Err(ModelStreamError::abort())
    } else {
        Ok(())
    }
}
/// `delay(ms, signal)`.
pub async fn delay(ms: f64, options: &StreamOptions) -> Result<bool, ModelStreamError> {
    use crate::network::{Exchange, delay};
    match delay(
        duration(ms),
        wait_abort(&options.abort),
        || options.abort.as_ref().and_then(AbortFlag::abort_event_at),
        None,
        false,
    )
    .await
    {
        Exchange::Response((), before_abort) => Ok(before_abort),
        Exchange::Aborted => Err(ModelStreamError::abort()),
        Exchange::Timeout { .. } => {
            unreachable!("provider retry waits have no surrounding deadline")
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming checkpoint

#[derive(Default)]
struct Checkpoint(pilotdeck_model_core::driver::Checkpoint);
impl std::ops::Deref for Checkpoint {
    type Target = pilotdeck_model_core::driver::Checkpoint;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl Checkpoint {
    fn on_event(&mut self, event: &JsValue) -> Result<(), ModelStreamError> {
        self.0.on_event(event).map_err(providers::codec_error)
    }
}

// ---------------------------------------------------------------------------
// Errors as streamModel classifies them

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

use pilotdeck_model_core::driver::errors::{error_event, retryable_message};
fn is_retryable_stream_error(error: &ModelStreamError) -> bool {
    pilotdeck_model_core::driver::errors::is_retryable_stream_error(error.view())
}
fn interruption_error(
    provider: &JsValue,
    error: &ModelStreamError,
    checkpoint: &Checkpoint,
) -> JsValue {
    pilotdeck_model_core::driver::errors::interruption_error(provider, error.view(), &checkpoint.0)
}
fn retry_reason(error: &ModelStreamError) -> &'static str {
    pilotdeck_model_core::driver::errors::retry_reason(error.view())
}
fn retry_after(error: &ModelStreamError) -> Option<f64> {
    pilotdeck_model_core::driver::errors::retry_after(error.view())
}

// ---------------------------------------------------------------------------
// Request sending

/// Preserve each redirect hop's completion source for both provider transports.
/// Errors retain whether their continuation precedes a later parent timer, so
/// Google's outer catch can use the same ordering as a successful response.
async fn provider_exchange<R: Send + 'static, P: Clone + Send + Sync + 'static>(
    transport: &dyn ModelHttpTransport<Rejection = R, Preparation = P>,
    request: Request<Full<Bytes>>,
    preparation: P,
    options: &StreamOptions,
    deadline: Option<tokio::time::Instant>,
    failure: impl FnOnce(TransportFailure) -> ModelStreamError,
    timeout: impl FnOnce() -> ModelStreamError,
) -> Result<Response<(ModelBody<R>, bool)>, (DriverFailure<R>, bool)> {
    use crate::network::{Exchange, exchange_with_prearmed_parent};
    let send = transport.send(request, preparation);
    match exchange_with_prearmed_parent(
        send,
        wait_abort(&options.abort),
        || options.abort.as_ref().and_then(AbortFlag::abort_event_at),
        deadline,
    )
    .await
    {
        Exchange::Response(result, before_abort) => result
            .map(|response| response.map(|body| (body, before_abort)))
            .map_err(|error| {
                let error = match error {
                    HostOperationError::Original(error) => error,
                    HostOperationError::Rejected(error) => {
                        return (DriverFailure::Host(error), before_abort);
                    }
                };
                // TS :749 (Google :502) observes same-stack abort even for a
                // rejected Promise. A later timer cannot interrupt the error
                // callback's microtasks, including when both timers are overdue.
                let error = if !before_abort && aborted(options) {
                    ModelStreamError::abort()
                } else {
                    failure(error)
                };
                // Google rechecks cancellation after SDK normalization too.
                (error.into(), before_abort)
            }),
        Exchange::Aborted => Err((ModelStreamError::abort().into(), false)),
        Exchange::Timeout { before_abort } => Err((timeout().into(), before_abort)),
    }
}

struct ReplayBody<R> {
    data: Option<Bytes>,
    tail: Option<BodyFailure>,
    rejection: std::marker::PhantomData<fn() -> R>,
}

impl<R> hyper::body::Body for ReplayBody<R> {
    type Data = Bytes;
    type Error = HostOperationError<BodyFailure, R>;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Bytes>, Self::Error>>> {
        let this = self.as_mut().get_mut();
        if let Some(data) = this.data.take() {
            return Poll::Ready(Some(Ok(hyper::body::Frame::data(data))));
        }
        if let Some(tail) = this.tail.take() {
            return Poll::Ready(Some(Err(HostOperationError::Original(tail))));
        }
        Poll::Ready(None)
    }
}

fn replay_body<R: Send + 'static>(bytes: Bytes, tail: Option<BodyFailure>) -> ModelBody<R> {
    ReplayBody {
        data: (!bytes.is_empty()).then_some(bytes),
        tail,
        rejection: std::marker::PhantomData,
    }
    .boxed()
}

fn replay_response<R: Send + 'static>(
    parts: hyper::http::response::Parts,
    bytes: Bytes,
    tail: Option<BodyFailure>,
    before_abort: bool,
) -> Response<(ModelBody<R>, bool)> {
    Response::from_parts(parts, (replay_body(bytes, tail), before_abort))
}

/// What a non-stream, multi-endpoint 2xx shape probe observed. Only stream
/// calls and single-endpoint calls skip this. The body read starts after the
/// response headers arrived; at that point `networkFetch` has returned and
/// detached its parent-abort listener, so neither `sendProviderRequest`'s
/// request timer nor a caller abort can stop it. Only undici's body idle
/// timeout / a socket failure does (r4b real-TS probe).
enum ShapeProbe<R> {
    HostRejected(R),
    /// The whole body arrived. `parsed` is `None` for empty/invalid JSON, which
    /// `Response.json()` rejects and therefore treats as the wrong shape.
    Complete {
        bytes: Bytes,
        parsed: Option<JsValue>,
    },
    /// A body frame failed (undici `TypeError: terminated`) or the body idle
    /// timeout fired; the caller must keep the prefix for a possible last
    /// candidate.
    ReadError {
        bytes: Bytes,
    },
    /// The body's received bytes passed V8's `MAX_STRING_LENGTH`. TS
    /// `copy.json()` reads the whole clone to EOF, then the byte→string
    /// conversion throws a plain `Error`; the candidate is rejected. The
    /// original body is drained and discarded, so the last candidate replays
    /// that error instead of retaining 512 MiB.
    StringTooLong,
}

/// Which outer event aborts the next endpoint fetch after a rejected shape.
/// `sendProviderRequest`'s catch rechecks `signal.aborted` and throws the
/// caller's abort reason even when the timeout fired first (`streamModel.ts:
/// 748-753`), so a parent abort that happened while the body was draining wins
/// regardless of deadline ordering. Only when the parent is still un-aborted is
/// an elapsed deadline reported as the timeout.
enum HeaderAbort {
    Timer,
    Parent,
}

fn shape_fallback_abort(
    options: &StreamOptions,
    deadline: Option<tokio::time::Instant>,
) -> Option<HeaderAbort> {
    if options.abort.as_ref().is_some_and(AbortFlag::is_aborted) {
        return Some(HeaderAbort::Parent);
    }
    deadline
        .is_some_and(|at| tokio::time::Instant::now() >= at)
        .then_some(HeaderAbort::Timer)
}

/// The body exceeded the original byte limit for text conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextLimitExceeded;

impl std::fmt::Display for TextLimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(crate::transport::STRING_TOO_LONG)
    }
}
impl std::error::Error for TextLimitExceeded {}

/// The byte→string conversion half shared by `read_shape_probe` and
/// `response_text`: both convert the body under V8's `MAX_STRING_LENGTH`,
/// which counts the received UTF-8 bytes — not the decoded UTF-16 units
/// (node v22.22 real-TS probe, `_tmp/20261025-issue-647-probe/RESULT.md`) —
/// and only throws after the body reached EOF, so an over-limit text keeps
/// draining while its accumulated value is dropped.
pub struct CappedText {
    decoder: sse::Utf8Decoder,
    text: String,
    bytes: usize,
    too_long: bool,
    converted_bytes: usize,
}

impl Default for CappedText {
    fn default() -> Self {
        Self::new()
    }
}

impl CappedText {
    pub fn new() -> Self {
        Self {
            decoder: sse::Utf8Decoder::default(),
            text: String::new(),
            bytes: 0,
            too_long: false,
            converted_bytes: 0,
        }
    }

    /// Feed one body frame. Returns `false` once the text can only be
    /// rejected: the conversion has not run yet, so the caller keeps
    /// draining to EOF — where a body failure still wins — but retains
    /// none of the over-limit prefix.
    pub fn push(&mut self, data: &[u8]) -> bool {
        if self.too_long {
            return false;
        }
        // The cap counts the frame's raw bytes, including the bytes of a
        // trailing incomplete UTF-8 sequence that only becomes U+FFFD at
        // EOF; exactly the limit is still allowed.
        self.bytes += data.len();
        if self.bytes > string_limit() {
            self.too_long = true;
            self.text = String::new();
            return false;
        }
        self.decode(data);
        true
    }

    /// Bytes admitted to the actual UTF-8 conversion entry. A rejected frame
    /// does not increase this counter, even while the caller keeps draining.
    pub fn converted_bytes(&self) -> usize {
        self.converted_bytes
    }

    /// Retained output-buffer capacity, for checking release on an over-limit
    /// body without exposing or mutating its text.
    pub fn text_capacity(&self) -> usize {
        self.text.capacity()
    }

    fn decode(&mut self, data: &[u8]) {
        // Observe the actual text conversion entry, not merely the cap
        // decision: a late cap check must fail the allocation-order test.
        self.converted_bytes += data.len();
        self.decoder.decode(data, &mut self.text);
    }

    /// EOF: every received byte was already counted frame by frame, so the
    /// cap verdict cannot change here; `finish` only completes a trailing
    /// incomplete UTF-8 sequence for the accepted text. `Err` is the
    /// over-limit outcome; each reader maps it to its own error shape.
    pub fn finish(mut self) -> Result<String, TextLimitExceeded> {
        self.decoder.finish(&mut self.text);
        if self.too_long {
            return Err(TextLimitExceeded);
        }
        Ok(self.text)
    }
}

async fn read_shape_probe<R: Send + 'static, B>(
    body: &mut B,
    options: &StreamOptions,
) -> ShapeProbe<R>
where
    B: hyper::body::Body<Data = Bytes, Error = HostOperationError<BodyFailure, R>> + Unpin + Send,
{
    use crate::network::{Exchange, body_frame};
    let mut capped = CappedText::new();
    let mut bytes = Vec::new();
    let mut continuation = false;
    loop {
        // A fresh undici `bodyTimeout` per frame, with no request deadline and
        // no caller abort: both were detached from this body when the headers
        // arrived, and TS still reads the clone to the end (probe r4b).
        let frame = body_frame(
            body,
            Some(duration(options.body_timeout_ms)),
            None,
            &mut continuation,
        )
        .await;
        match frame {
            Exchange::Response(Some(Ok(frame)), _) => {
                if let Some(data) = frame.data_ref() {
                    if capped.push(data) {
                        bytes.extend_from_slice(data);
                    } else {
                        // `copy.json()` reads the body to EOF before the
                        // byte→string conversion throws; do not fall back
                        // early, and retain none of the rejected candidate.
                        bytes = Vec::new();
                    }
                }
            }
            Exchange::Response(Some(Err(HostOperationError::Rejected(error))), _) => {
                return ShapeProbe::HostRejected(error);
            }
            Exchange::Response(Some(Err(HostOperationError::Original(_))), _)
            | Exchange::Timeout { .. } => {
                return ShapeProbe::ReadError {
                    bytes: Bytes::from(bytes),
                };
            }
            Exchange::Response(None, _) => break,
            // `None` above means there is no abort source; keep the prefix
            // rather than inventing an abort result.
            Exchange::Aborted => {
                return ShapeProbe::ReadError {
                    bytes: Bytes::from(bytes),
                };
            }
        }
    }
    match capped.finish() {
        Err(TextLimitExceeded) => ShapeProbe::StringTooLong,
        Ok(text) => ShapeProbe::Complete {
            bytes: Bytes::from(bytes),
            parsed: parse_json(&text).ok(),
        },
    }
}

/// `sendProviderRequest` + `sendWithEndpointFallback` + `networkFetch`.
///
/// `stream` selects the baseline's timeout: streaming resolves
/// `resolveStreamIdleTimeout` and reports `Stream idle timeout…`; `complete()`
/// uses `provider.timeoutMs ?? DEFAULT_REQUEST_TIMEOUT_MS`, ignores
/// `streamTimeoutMs`, and reports `Model request timed out after <ms>ms.`
/// (`streamModel.ts:716-755`).
///
/// The timer bounds every endpoint's *headers* exchange. A 2xx shape body read
/// starts after the headers arrived, when `networkFetch` has already detached
/// its parent-abort listener; the remaining clone/`json()` read is therefore
/// not bounded by this timer (real-TS r4b probe). It still decides whether the
/// *next* endpoint may be tried: once the timer or caller abort fired, the
/// outer controller is aborted and the next `networkFetch` rejects before any
/// request is sent. `response.text()` runs after this function returns, as in
/// the baseline.
async fn send_provider_request<S: ModelHostServices + ?Sized>(
    transport: &dyn ModelHttpTransport<Rejection = S::Rejection, Preparation = S::Preparation>,
    host: &S,
    provider: &JsValue,
    body: &JsValue,
    options: &StreamOptions,
    stream: bool,
) -> Result<Response<(ModelBody<S::Rejection>, bool)>, DriverFailure<S::Rejection>> {
    let (timeout_ms, timeout_text) = if stream {
        let ms = idle_timeout_ms(provider, options);
        (
            ms,
            format!(
                "Stream idle timeout: no data received for {}ms",
                pilotdeck_model_core::codec::js::number_string(ms)
            ),
        )
    } else {
        let ms = field(provider, "timeoutMs")
            .as_number()
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT_MS);
        (
            ms,
            format!(
                "Model request timed out after {}ms.",
                pilotdeck_model_core::codec::js::number_string(ms)
            ),
        )
    };
    let final_body = providers::final_body(body, provider);
    let headers = host.materialize(
        provider,
        &final_body,
        if stream {
            RequestMode::Stream
        } else {
            RequestMode::Complete
        },
    )?;
    let encoded = pilotdeck_model_core::codec::js::json_stringify(&final_body)
        .map_err(|_| {
            ModelStreamError::thrown(
                "RangeError",
                pilotdeck_model_core::codec::js::StackOverflow::MESSAGE,
            )
        })?
        .unwrap_or_default();

    // PILOTDECK_DUMP_REQUEST: debug aid that writes the provider request body
    // to a file under the OS temp directory. TS dumps only inside the stream
    // loop (`streamModel.ts:196-203`); `complete()` calls `sendProviderRequest`
    // directly without it (`:124`), so the non-streaming path stays cold.
    if stream {
        // TS throws the raw fs Error out of the generator here (the dump
        // sits outside the send try). Swallowing the failure and continuing
        // to send is this port's pre-existing behavior (already in main,
        // unchanged by this candidate's diff); registered in
        // A6.4-MODEL-DUMP-SECURE-001.
        match host.dump_request_if_enabled(&final_body, field(body, "model")) {
            Ok(()) => {}
            Err(HostOperationError::Original(failure)) => host.diagnostic(&format!(
                "[model-debug] Failed to write request dump: {}",
                String::from_utf16_lossy(failure.message.units())
            )),
            Err(HostOperationError::Rejected(error)) => return Err(DriverFailure::Host(error)),
        }
    }

    let encoded = Bytes::from(encoded);
    let endpoints = providers::chat_endpoint_candidates(provider);
    let count = endpoints.len();
    let to_provider = |error: NetworkFetchError| {
        ModelStreamError::Provider(normalize_model_error(
            &provider_id(provider),
            &protocol(provider),
            ErrorInput::Error(&error.as_js_error()),
            None,
        ))
    };
    // This timer surrounds every endpoint and redirect. Race each observable
    // transport exchange before follow() can erase its scheduled_at() source.
    // TS `const timeout = effectiveTimeoutMs ? setTimeout(...) : undefined`
    // treats 0/NaN as "no timer"; a negative value schedules Node's 1 ms timer.
    let deadline = (!timeout_ms.is_nan() && timeout_ms != 0.0)
        .then(|| tokio::time::Instant::now() + duration(timeout_ms));
    let abort_error = |abort: HeaderAbort| match abort {
        HeaderAbort::Parent => ModelStreamError::abort(),
        HeaderAbort::Timer => to_provider(NetworkFetchError::new(
            "network_timeout",
            timeout_text.clone(),
            JsValue::Undefined,
        )),
    };
    let mut last = None;
    for (index, endpoint) in endpoints.iter().enumerate() {
        let last_candidate = index + 1 == count;
        let HeaderProjection {
            preparation,
            url,
            headers: header_map,
        } = headers
            .validate_url(endpoint)
            .map_err(|error| error.map_original(to_provider))?;
        let response = crate::fetch_redirect::follow_with_context(
            url,
            hyper::Method::POST,
            header_map,
            encoded.clone(),
            preparation,
            || {
                DriverFailure::Model(to_provider(NetworkFetchError::classify(
                    "fetch failed",
                    None,
                )))
            },
            |mut request, preparation| {
                let timeout_text = &timeout_text;
                async move {
                    let timeout_text = timeout_text.clone();
                    let wire = crate::fetch_redirect::undici_wire_headers(
                        request.uri(),
                        request.headers(),
                    );
                    *request.headers_mut() = wire;
                    provider_exchange(
                        transport,
                        request,
                        preparation,
                        options,
                        deadline,
                        |failure| {
                            to_provider(NetworkFetchError::classify(
                                &failure.message,
                                failure.cause_code.as_deref(),
                            ))
                        },
                        || {
                            to_provider(NetworkFetchError::new(
                                "network_timeout",
                                timeout_text,
                                JsValue::Undefined,
                            ))
                        },
                    )
                    .await
                    .map_err(|(error, _)| error)
                }
            },
        )
        .await?;
        let status = response.status().as_u16();
        let ok = (200..300).contains(&status);
        if !ok && count != 1 && matches!(status, 400 | 404 | 405) {
            last = Some(response);
            continue;
        }
        if ok && !stream && count > 1 {
            // TS `shouldUseEndpointResponse`: clone and parse the 2xx body
            // after headers. The read itself is detached from the request
            // timer, but a rejection moves to the next candidate only while
            // the outer controller is still un-aborted; the final candidate is
            // returned even when its shape is wrong or its read failed.
            let (parts, (mut body, before_abort)) = response.into_parts();
            match read_shape_probe(&mut body, options).await {
                ShapeProbe::HostRejected(error) => return Err(DriverFailure::Host(error)),
                ShapeProbe::Complete { bytes, parsed } => {
                    let accepted = parsed.as_ref().is_some_and(|value| {
                        providers::expected_complete_response_shape(
                            field(provider, "protocol"),
                            value,
                        )
                    });
                    let response = replay_response(parts, bytes, None, before_abort);
                    if accepted {
                        return Ok(response);
                    }
                    if !last_candidate && let Some(abort) = shape_fallback_abort(options, deadline)
                    {
                        return Err(abort_error(abort).into());
                    }
                    last = Some(response);
                    continue;
                }
                ShapeProbe::ReadError { bytes } => {
                    if !last_candidate && let Some(abort) = shape_fallback_abort(options, deadline)
                    {
                        return Err(abort_error(abort).into());
                    }
                    last = Some(replay_response(
                        parts,
                        bytes,
                        Some(BodyFailure::Terminated),
                        before_abort,
                    ));
                    continue;
                }
                ShapeProbe::StringTooLong => {
                    // The body was already drained to EOF above; only now may
                    // the rejected shape try the next candidate, and the last
                    // candidate must surface V8's plain `Error` from text().
                    if !last_candidate && let Some(abort) = shape_fallback_abort(options, deadline)
                    {
                        return Err(abort_error(abort).into());
                    }
                    last = Some(replay_response(
                        parts,
                        Bytes::new(),
                        Some(BodyFailure::StringTooLong),
                        before_abort,
                    ));
                    continue;
                }
            }
        }
        return Ok(response);
    }
    last.ok_or_else(|| to_provider(NetworkFetchError::classify("fetch failed", None)))
        .map_err(Into::into)
}

/// `await response.text()`: UTF-8 with BOM stripped.
///
/// The byte→string conversion runs only after the body reached EOF, exactly
/// like TS `Response.text()`: an over-`MAX_STRING_LENGTH` body is still read
/// to the end (real TS probes, `_tmp/20261025-issue-641/RESULT.md`), so a
/// mid-read body failure keeps priority over the string error. The cap
/// counts the received UTF-8 bytes, not the decoded UTF-16 units (node
/// v22.22 real-TS probe, `_tmp/20261025-issue-647-probe/RESULT.md`); a
/// trailing incomplete UTF-8 sequence becomes one U+FFFD in the returned
/// text, and its bytes were already counted when received. The conversion
/// throws V8's plain `Error`, never a `RangeError`.
pub async fn response_text<R: Send + 'static, B>(
    body: &mut B,
    options: &StreamOptions,
    mut continuation: bool,
) -> Result<String, DriverFailure<R>>
where
    B: hyper::body::Body<Data = Bytes, Error = HostOperationError<BodyFailure, R>> + Unpin + Send,
{
    // TS reads this body after the request timer was cleared and the abort
    // listener detached; only undici's `bodyTimeout` bounds a stalled body
    // (node probe: `TypeError: terminated`, cause UND_ERR_BODY_TIMEOUT).
    // Google wraps this body read in its still-pending SDK promise below.
    let mut capped = CappedText::new();
    loop {
        use crate::network::{Exchange, body_frame};
        match body_frame(
            body,
            Some(duration(options.body_timeout_ms)),
            None,
            &mut continuation,
        )
        .await
        {
            Exchange::Aborted => return Err(ModelStreamError::abort().into()),
            // A codec rejection reaches `response.text()` the same way: the
            // corpus' corrupt-gzip cells answer with the bare "terminated"
            // (the cause text is only kept by the raw stream readers).
            Exchange::Timeout { .. }
            | Exchange::Response(
                Some(Err(HostOperationError::Original(
                    BodyFailure::Terminated | BodyFailure::Decode(_),
                ))),
                _,
            ) => {
                return Err(ModelStreamError::thrown("TypeError", "terminated").into());
            }
            // The drained over-limit shape body on the last candidate: real TS
            // `response.text()` throws V8's plain `Error`, not `RangeError`
            // (probe `probe-shape-string-limit-last.mts`).
            Exchange::Response(
                Some(Err(HostOperationError::Original(BodyFailure::StringTooLong))),
                _,
            ) => {
                return Err(ModelStreamError::thrown(
                    "Error",
                    crate::fetch_redirect::STRING_TOO_LONG,
                )
                .into());
            }
            Exchange::Response(Some(Err(HostOperationError::Rejected(error))), _) => {
                return Err(DriverFailure::Host(error));
            }
            Exchange::Response(None, _) => break,
            Exchange::Response(Some(Ok(frame)), _) => {
                if let Some(data) = frame.data_ref() {
                    // The string conversion has not run yet; `push` keeps
                    // draining past the cap so EOF / the body idle timeout
                    // still decide and a body failure keeps priority over
                    // the string error.
                    capped.push(data);
                }
            }
        }
    }
    capped.finish().map_err(|TextLimitExceeded| {
        ModelStreamError::thrown("Error", crate::fetch_redirect::STRING_TOO_LONG).into()
    })
}

struct StreamGuard(pilotdeck_model_core::driver::StreamGuard);
impl StreamGuard {
    fn new(provider: &JsValue, now: f64, env: Option<&str>) -> Self {
        Self(pilotdeck_model_core::driver::StreamGuard::new(
            provider, now, env,
        ))
    }
    fn check_duration(&self, now: f64) -> Result<(), ModelStreamError> {
        self.0.check_duration(now).map_err(providers::codec_error)
    }
    fn observe(&mut self, event: &JsValue, now: f64) -> Result<(), ModelStreamError> {
        self.0.observe(event, now).map_err(providers::codec_error)
    }
}

fn object_entries(value: &JsValue) -> JsObject {
    value.as_object().cloned().unwrap_or_default()
}

/// Read one body frame under the idle timeout and the abort signal. Both a
/// response and a timeout carry their continuation order through Google's
/// outer abort recheck, including when the body stalls after partial text.
pub async fn read_frame<R: Send + 'static, B>(
    body: &mut B,
    idle_ms: f64,
    options: &StreamOptions,
    continuation: &mut bool,
) -> Result<Option<Bytes>, DriverFailure<R>>
where
    B: hyper::body::Body<Data = Bytes, Error = HostOperationError<BodyFailure, R>> + Unpin + Send,
{
    // Equal durations favor the native idle watchdog. Undici's HTTP parser
    // uses a coarse fast timer for bodyTimeout (499 ms ticks, including short
    // nonzero durations). Its phase and event-loop lag are not recoverable
    // from these two durations: do not add a fixed 499/1000 ms correction.
    // #411 TCP probes: body=5000, idle=5100/5300 can still produce idle while
    // this duration comparison produces body. Parity scenarios use equality
    // or >=1000 ms separation; this is a fixture constraint, not a guarantee
    // under arbitrary scheduler stalls. Unaccepted window and raw evidence:
    // specs/gateway-parity/diffs/A6.4-PROVIDER-IDLE-001.md.
    let idle_deadline = tokio::time::Instant::now() + duration(idle_ms);
    loop {
        let remaining = idle_deadline.saturating_duration_since(tokio::time::Instant::now());
        let body_idle = duration(options.body_timeout_ms);
        let body_first = body_idle < remaining;
        let wait = body_idle.min(remaining);
        use crate::network::{Exchange, body_frame_with_prearmed_parent};
        match body_frame_with_prearmed_parent(
            body,
            Some(wait),
            options.abort.as_ref(),
            continuation,
        )
        .await
        {
            Exchange::Aborted => return Err(ModelStreamError::abort().into()),
            Exchange::Timeout { .. } if body_first => {
                return Err(ModelStreamError::thrown("TypeError", "terminated").into());
            }
            Exchange::Timeout { .. } => {
                return Err(ModelStreamError::thrown(
                    "StreamIdleTimeoutError",
                    format!(
                        "Stream idle timeout: no data received for {}ms",
                        pilotdeck_model_core::codec::js::number_string(idle_ms)
                    ),
                )
                .into());
            }
            Exchange::Response(None, _) => return Ok(None),
            Exchange::Response(Some(Err(HostOperationError::Rejected(error))), _) => {
                return Err(DriverFailure::Host(error));
            }
            Exchange::Response(Some(Err(HostOperationError::Original(_))), _) => {
                return Err(ModelStreamError::thrown("TypeError", "terminated").into());
            }
            Exchange::Response(Some(Ok(frame)), _) => {
                if let Ok(data) = frame.into_data()
                    && !data.is_empty()
                {
                    return Ok(Some(data));
                }
            }
        }
    }
}

/// V8's string length limit (`MAX_STRING_LENGTH`), shared by every
/// cross-event string accumulator of the stream path and the non-stream
/// shape probe.
///
/// There is one function and one default tail expression for both builds. A
/// test only overrides it explicitly through [`pilotdeck_model_core::codec::limits::LimitGuard`]; with no override
/// both the transport and pure provider state machines read the same limit.
/// Boundary tests scope a smaller value with the shared `LimitGuard`.
pub fn string_limit() -> usize {
    pilotdeck_model_core::codec::limits::string_limit()
}
#[cfg(test)]
pub(crate) use pilotdeck_model_core::codec::limits::LimitGuard;
/// The SSE reader's own `buffer += ...` bound, distinct from the accumulator
/// [`string_limit`]: only its undelimited remainder accumulates here.
///
/// There is one function and one default tail expression for both builds. A
/// test only overrides it explicitly through [`SseLimitGuard`]; with no
/// override the test build runs the same `count_http::MAX_STRING_LENGTH`
/// expression as production. The production build does not compile the
/// override branch.
fn sse_limit() -> usize {
    #[cfg(test)]
    {
        if let Some(limit) = SSE_LIMIT.with(std::cell::Cell::get) {
            return limit;
        }
    }
    crate::fetch_redirect::MAX_STRING_LENGTH
}
#[cfg(test)]
thread_local! {
    /// `None` (the default) means "use the shared V8 limit"; a separate knob
    /// so accumulator tests can keep the SSE buffer unbounded while
    /// [`pilotdeck_model_core::codec::limits::LimitGuard`] shrinks `string_limit`. Only [`SseLimitGuard`] sets an
    /// override, and nested guards restore the previous value on drop.
    pub(crate) static SSE_LIMIT: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}
/// Restores a test's SSE limit even if its assertion panics.
#[cfg(test)]
pub(crate) struct SseLimitGuard(Option<usize>);
#[cfg(test)]
impl SseLimitGuard {
    pub(crate) fn set(limit: usize) -> Self {
        Self(SSE_LIMIT.with(|cell| cell.replace(Some(limit))))
    }
}
#[cfg(test)]
impl Drop for SseLimitGuard {
    fn drop(&mut self) {
        SSE_LIMIT.with(|cell| cell.set(self.0));
    }
}

/// `buffer += more` on a JS string: past V8's limit it throws
/// `RangeError: Invalid string length` (node probe r2), leaving `buffer`
/// unchanged. Exactly the limit is still allowed.
pub fn append_js_string(buffer: &mut Vec<u16>, more: &[u16]) -> Result<(), ModelStreamError> {
    pilotdeck_model_core::codec::limits::append_js_string(buffer, more)
        .map_err(providers::codec_error)
}
pub(crate) fn invalid_string_length() -> ModelStreamError {
    ModelStreamError::thrown("RangeError", "Invalid string length")
}

fn sse_error(error: sse::SseError) -> ModelStreamError {
    match error {
        sse::SseError::Syntax(message) => ModelStreamError::thrown("SyntaxError", message),
        // V8 `RangeError: Invalid string length` (node probe), not retryable.
        sse::SseError::InvalidStringLength => invalid_string_length(),
    }
}

fn request_started_event(
    provider: &JsValue,
    current: &JsValue,
) -> Result<JsValue, ModelStreamError> {
    pilotdeck_model_core::codec::request_started_event(provider, current)
        .map_err(providers::codec_error)
}

/// `streamModel(request, config, options)` for the HTTP protocols.
pub async fn stream_model<S: ModelHostServices + ?Sized>(
    transport: &dyn ModelHttpTransport<Rejection = S::Rejection, Preparation = S::Preparation>,
    host: &S,
    request: &JsValue,
    config: &JsValue,
    options: &StreamOptions,
    sink: &mut (dyn FnMut(StreamOutput) + Send),
) -> Result<(), DriverFailure<S::Rejection>> {
    let mut current = request.clone();
    put(&mut current, "stream", JsValue::Bool(true));
    let (provider, _) = providers::validate_model_request(&current, config)?;
    if is(field(&provider, "protocol"), "google") {
        return stream_google_model(transport, host, &current, config, options, sink, &provider)
            .await;
    }
    let (max_retries, _) = retry_decide(&provider, 0.0, None, 0.0);
    let mut checkpoint = Checkpoint::default();
    let mut attempt = 0.0;
    let mut continuation_before_abort = false;
    while attempt <= max_retries {
        if !std::mem::take(&mut continuation_before_abort) {
            throw_if_aborted(options)?;
        }
        sink(StreamOutput::Event(request_started_event(
            &provider, &current,
        )?));
        options.yield_boundary().await;
        let (provider_now, model_now) = providers::validate_model_request(&current, config)?;
        let body = providers::build_model_request(&current, &provider_now, &model_now)?;
        let mut invocation = begin_invocation(
            host,
            options,
            &provider,
            field(&current, "model"),
            true,
            &body,
            attempt + 1.0,
        )?;

        let progress = |sink: &mut (dyn FnMut(StreamOutput) + Send),
                        reason: &str,
                        delay_ms: f64,
                        model: &JsValue| {
            let mut value = JsObject::new();
            value.insert("reason".into(), reason.into());
            value.insert("attempt".into(), JsValue::Number(attempt + 1.0));
            value.insert("maxAttempts".into(), JsValue::Number(max_retries));
            value.insert("delayMs".into(), JsValue::Number(js_round(delay_ms)));
            value.insert("provider".into(), field(&provider, "id").clone());
            value.insert("model".into(), model.clone());
            sink(StreamOutput::RetryProgress(JsValue::Object(value)));
        };

        let response = match send_provider_request(transport, host, &provider, &body, options, true)
            .await
        {
            Ok(response) => response,
            Err(DriverFailure::Host(error)) => {
                finish_invocation(
                    host,
                    &mut invocation,
                    options,
                    "incomplete",
                    InvocationResponse {
                        body: None,
                        status: None,
                        complete: false,
                    },
                    None,
                );
                return Err(DriverFailure::Host(error));
            }
            Err(DriverFailure::Model(error)) => {
                finish_invocation(
                    host,
                    &mut invocation,
                    options,
                    "transport_error",
                    InvocationResponse {
                        body: None,
                        status: None,
                        complete: false,
                    },
                    Some(&error),
                );
                if attempt < max_retries && is_retryable_stream_error(&error) {
                    let (_, delay_ms) = retry_decide(&provider, attempt, None, (options.random)());
                    progress(sink, "network_error", delay_ms, field(&current, "model"));
                    continuation_before_abort = delay(delay_ms, options).await?;
                    attempt += 1.0;
                    continue;
                }
                if is_retryable_stream_error(&error) && !checkpoint.is_empty() {
                    sink(StreamOutput::Event(error_event(interruption_error(
                        &provider,
                        &error,
                        &checkpoint,
                    ))));
                    options.yield_boundary().await;
                    return Ok(());
                }
                return Err(error.into());
            }
        };
        let status = response.status().as_u16();
        let retry_after_header = {
            let values: Vec<String> = response
                .headers()
                .get_all("retry-after")
                .iter()
                .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
                .collect();
            (!values.is_empty()).then(|| values.join(", "))
        };
        let (mut body, mut body_before_abort) = response.into_body();
        if matches!(status, 204 | 205) {
            // Null-body statuses: `response.body` is null in fetch.
            finish_invocation(
                host,
                &mut invocation,
                options,
                "incomplete",
                InvocationResponse {
                    body: None,
                    status: Some(status),
                    complete: false,
                },
                None,
            );
            let missing = JsError {
                message: "Missing response body.".into(),
                code: None,
                network_code: None,
                raw: JsValue::Object(JsObject::new()),
            };
            sink(StreamOutput::Event(error_event(JsValue::Object(
                normalize_model_error(
                    &provider_id(&provider),
                    &protocol(&provider),
                    ErrorInput::Error(&missing),
                    None,
                ),
            ))));
            options.yield_boundary().await;
            return Ok(());
        }
        if !(200..300).contains(&status) {
            let text = match response_text(&mut body, options, body_before_abort).await {
                Ok(text) => text,
                Err(DriverFailure::Model(error)) => return Err(error.into()),
                Err(DriverFailure::Host(error)) => {
                    finish_invocation(
                        host,
                        &mut invocation,
                        options,
                        "incomplete",
                        InvocationResponse {
                            body: None,
                            status: Some(status),
                            complete: false,
                        },
                        None,
                    );
                    return Err(DriverFailure::Host(error));
                }
            };
            finish_invocation(
                host,
                &mut invocation,
                options,
                "provider_error",
                InvocationResponse {
                    body: Some(text.as_bytes()),
                    status: Some(status),
                    complete: false,
                },
                None,
            );
            let raw = parse_json(&text).unwrap_or_else(|_| JsValue::String(text.as_str().into()));
            let mut error = normalize_model_error(
                &provider_id(&provider),
                &protocol(&provider),
                ErrorInput::Value(&raw),
                Some(f64::from(status)),
            );
            if error.get("retryAfterMs").is_none_or(JsValue::is_undefined)
                && let Some(ms) = errors::retry_after_header_parsed(
                    retry_after_header.as_deref(),
                    (options.clock)(),
                    &|value| host.parse_retry_after_date(value),
                )
            {
                errors::set_retry_after(&mut error, ms);
            }
            let retryable = error.get("retryable") == Some(&JsValue::Bool(true));
            if retryable && attempt < max_retries {
                let after = error.get("retryAfterMs").and_then(JsValue::as_number);
                let (_, delay_ms) = retry_decide(&provider, attempt, after, (options.random)());
                let reason = retry_reason(&ModelStreamError::Provider(error.clone()));
                progress(sink, reason, delay_ms, field(&current, "model"));
                drop(body);
                continuation_before_abort = delay(delay_ms, options).await?;
                attempt += 1.0;
                continue;
            }
            if retryable && !checkpoint.is_empty() {
                let error = ModelStreamError::Provider(error);
                sink(StreamOutput::Event(error_event(interruption_error(
                    &provider,
                    &error,
                    &checkpoint,
                ))));
                options.yield_boundary().await;
                return Ok(());
            }
            sink(StreamOutput::Event(error_event(JsValue::Object(error))));
            options.yield_boundary().await;
            return Ok(());
        }

        let mut state = StreamState::new(&provider);
        let mut sentinel = false;
        let idle_ms = idle_timeout_ms(&provider, options);
        let mut guard = StreamGuard::new(
            &provider,
            (options.clock)(),
            host.streaming_duration_limit().as_deref(),
        );
        let mut buffer = sse::SseBuffer::with_limit(sse_limit());
        // `rawResponseChunks`: every body byte read, for the audit record.
        let mut raw_response: Vec<u8> = Vec::new();
        let outcome: Result<(), DriverFailure<S::Rejection>> = async {
            loop {
                if !body_before_abort {
                    throw_if_aborted(options)?;
                }
                let chunk = read_frame(&mut body, idle_ms, options, &mut body_before_abort).await?;
                if invocation.is_some()
                    && let Some(bytes) = &chunk
                {
                    raw_response.extend_from_slice(bytes);
                }
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
                        sse::SseEvent::Done => {
                            sentinel = true;
                            continue;
                        }
                        sse::SseEvent::Data(raw) => raw,
                    };
                    for event in state.normalize(&raw)? {
                        let tag = field(&event, "type");
                        if tag.as_string().is_some_and(|t| t.equals_str("message_end")) {
                            sentinel = true;
                        }
                        if tag.as_string().is_some_and(|t| t.equals_str("error")) {
                            return Err(ModelStreamError::Provider(object_entries(field(
                                &event, "error",
                            )))
                            .into());
                        }
                        guard.observe(&event, (options.clock)())?;
                        checkpoint.on_event(&event)?;
                        sink(StreamOutput::Event(event));
                        options.yield_boundary().await;
                    }
                }
                if chunk.is_none() {
                    break;
                }
            }
            guard.check_duration((options.clock)())?;
            if !sentinel {
                return Err(ModelStreamError::thrown(
                    "IncompleteStreamError",
                    "Network stream ended before provider completion sentinel.",
                )
                .into());
            }
            Ok(())
        }
        .await;
        // Closing the body (here or on any return) cancels the connection.
        drop(body);
        let settled = |complete: bool| InvocationResponse {
            body: Some(raw_response.as_slice()),
            status: Some(status),
            complete,
        };
        let Err(error) = outcome else {
            finish_invocation(
                host,
                &mut invocation,
                options,
                "success",
                settled(true),
                None,
            );
            return Ok(());
        };
        let error = match error {
            DriverFailure::Model(error) => error,
            DriverFailure::Host(error) => {
                finish_invocation(
                    host,
                    &mut invocation,
                    options,
                    "incomplete",
                    settled(false),
                    None,
                );
                return Err(DriverFailure::Host(error));
            }
        };
        let retryable = is_retryable_stream_error(&error);
        finish_invocation(
            host,
            &mut invocation,
            options,
            if retryable {
                "incomplete"
            } else {
                "transport_error"
            },
            settled(false),
            Some(&error),
        );
        if attempt < max_retries && retryable && checkpoint.can_continue_text() {
            current = continuation_request(&current, checkpoint.partial_text());
            let (_, delay_ms) =
                retry_decide(&provider, attempt, retry_after(&error), (options.random)());
            progress(sink, "continuation", delay_ms, field(&current, "model"));
            continuation_before_abort = delay(delay_ms, options).await?;
            attempt += 1.0;
            continue;
        }
        if retryable && attempt < max_retries && checkpoint.is_empty() {
            let (_, delay_ms) =
                retry_decide(&provider, attempt, retry_after(&error), (options.random)());
            progress(
                sink,
                retry_reason(&error),
                delay_ms,
                field(&current, "model"),
            );
            continuation_before_abort = delay(delay_ms, options).await?;
            attempt += 1.0;
            continue;
        }
        if retryable {
            sink(StreamOutput::Event(error_event(interruption_error(
                &provider,
                &error,
                &checkpoint,
            ))));
            options.yield_boundary().await;
            return Ok(());
        }
        return Err(error.into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "http/complete_tests.rs"]
mod complete_tests;
#[cfg(test)]
#[path = "http/google_tests.rs"]
mod google_tests;
#[cfg(test)]
#[path = "http/host_rejection_tests.rs"]
mod host_rejection_tests;
#[cfg(test)]
#[path = "http/test_baseline.rs"]
mod test_baseline;
#[cfg(test)]
#[path = "http/test_support.rs"]
mod test_support;
#[cfg(test)]
#[path = "http_tests.rs"]
mod tests;
