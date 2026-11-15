use crate::{event, sse::SseDecoder};
use agentcpm_iface::{
    BoxFuture,
    env::{
        HttpStreamCancel, HttpStreamControl, HttpStreamHead, HttpStreamRequest, HttpStreamSink,
        HttpStreamTransport,
    },
    model_session::*,
    types::{Error, ErrorCategory},
};
use pilotdeck_model_core::{
    codec::{self, providers::google},
    request,
};
use pilotdeck_protocol::{JsObject, JsString, JsValue, parse_json, stringify_json};
use std::{
    collections::VecDeque,
    future::poll_fn,
    sync::{Arc, Mutex},
    task::{Poll, Waker},
    time::Instant,
};

fn invalid(message: impl Into<String>) -> Error {
    Error::new(
        ErrorCategory::Protocol,
        agentcpm_iface::ERR_INVALID_PARAMS,
        message,
    )
}
fn unsupported() -> Error {
    invalid("Unsupported model session operation")
}
fn field<'a>(value: &'a JsValue, key: &str) -> &'a JsValue {
    static UNDEFINED: JsValue = JsValue::Undefined;
    value.get(key).unwrap_or(&UNDEFINED)
}
fn text(value: &JsValue) -> String {
    value
        .as_string()
        .map_or_else(String::new, |s| String::from_utf16_lossy(s.units()))
}
fn json(value: &JsValue) -> Result<String, String> {
    stringify_json(value)
        .map_err(|e| format!("{e:?}"))?
        .ok_or_else(|| "not JSON".into())
}
fn codec_error(e: codec::CodecError) -> String {
    String::from_utf16_lossy(e.message().units())
}

fn monotonic_deadline(
    deadline: Option<agentcpm_iface::types::Deadline>,
) -> Result<Option<Instant>, Error> {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    let Some(deadline) = deadline else {
        return Ok(None);
    };
    // Sampling monotonic first and rounding wall time up cannot extend an admitted deadline.
    let monotonic_now = Instant::now();
    let wall = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| invalid("system clock precedes epoch"))?;
    let now = wall.as_nanos().saturating_add(999_999) / 1_000_000;
    let now = u64::try_from(now).map_err(|_| invalid("system clock overflow"))?;
    let remaining = deadline
        .remaining_ms(now)
        .ok_or_else(|| invalid("execution deadline expired"))?;
    monotonic_now
        .checked_add(Duration::from_millis(remaining))
        .map(Some)
        .ok_or_else(|| invalid("execution deadline cannot be represented"))
}

/// Host-supplied configuration and transport are trusted; model JSON never supplies credentials.
pub struct PilotDeckModelFactory {
    pub config: JsValue,
    pub transport: Arc<dyn HttpStreamTransport>,
}

/// Trusted per-generation services. The supplier must verify the real admitted
/// context and frozen binding/revision before returning; strings are not proof.
/// Configuration and all diagnostics supplied here must contain no secrets.
/// All service resources must belong to this scope and its close_scope receipt.
/// An error is not itself evidence that partially created resources are released.
pub trait ModelJobServicesProvider: Send + Sync {
    fn supports_binding(&self, binding: &ModelBinding) -> bool;
    fn for_job(
        &self,
        context: &ModelExecutionContext,
        binding: &ModelBinding,
        revision: ConfigRevision,
    ) -> Result<ModelJobServices, agentcpm_iface::env::http_pull::HttpFailure>;
}

pub struct ModelJobServices {
    pub safe_config: JsValue,
    pub host: Arc<
        dyn pilotdeck_model_runtime::services::ModelHostServices<
                Rejection = agentcpm_iface::env::http_pull::HttpFailure,
                Preparation = agentcpm_iface::env::http_pull::HttpRequestPreparation,
            >,
    >,
    /// Original turn options, including its original cancellation source.
    pub options: pilotdeck_model_runtime::http::StreamOptions,
}

/// Explicit opt-in to the shared driver. There is deliberately no ambient or
/// legacy fallback when the trusted supplier cannot authorize a generation.
pub struct SharedPilotDeckModelFactory {
    pub jobs: Arc<dyn ModelJobServicesProvider>,
    pub transport: Arc<dyn agentcpm_iface::env::http_pull::HttpPullTransport>,
    pub executor: tokio::runtime::Handle,
}
struct SharedWorkerConfig {
    jobs: Arc<dyn ModelJobServicesProvider>,
    transport: Arc<dyn agentcpm_iface::env::http_pull::HttpPullTransport>,
    executor: tokio::runtime::Handle,
}
impl ModelSessionFactory for SharedPilotDeckModelFactory {
    fn capabilities(&self) -> SessionCapabilities {
        SessionCapabilities {
            completion: true,
            streaming_output: true,
            tool_calls: true,
            ..Default::default()
        }
    }
    fn supported_profiles(&self) -> Vec<String> {
        vec![
            MODEL_REQUEST_PROFILE_V1.into(),
            MODEL_EVENT_PROFILE_V1.into(),
            MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1.into(),
        ]
    }
    fn open(
        &self,
        config: ModelSessionConfig,
    ) -> BoxFuture<'_, Result<Arc<dyn ModelSession>, Error>> {
        Box::pin(async move {
            if !config.binding.extensions.is_empty() || !self.jobs.supports_binding(&config.binding)
            {
                return Err(unsupported());
            }
            build_session(
                config,
                None,
                Some(Arc::new(SharedWorkerConfig {
                    jobs: self.jobs.clone(),
                    transport: self.transport.clone(),
                    executor: self.executor.clone(),
                })),
            )
        })
    }
}

struct State {
    state: SessionState,
    queue: VecDeque<ModelSessionEvent>,
    waiter: Option<Waker>,
    seq: u64,
    queued_bytes: u64,
    input_seq: u64,
    input: Option<(InputId, u32, Vec<u8>)>,
    inspected: Option<(InputId, ConfigRevision, bool)>,
    active: Option<(InputId, ResponseId, HttpStreamCancel)>,
    closing: bool,
    uncertain: bool,
    scope_owner: Option<(String, String)>,
    gate: Option<pilotdeck_model_runtime::http::YieldGate>,
    gate_pending: bool,
    release_profile: bool,
    pending_cleanup: usize,
    terminal_emitted: bool,
    response_exposed: bool,
    cancelled_response: Option<ResponseId>,
}
struct Session {
    id: ModelSessionId,
    config: ModelSessionConfig,
    factory: Option<Arc<WorkerConfig>>,
    shared: Option<Arc<SharedWorkerConfig>>,
    state: Arc<Mutex<State>>,
}
struct WorkerConfig {
    config: JsValue,
    transport: Arc<dyn HttpStreamTransport>,
}

fn emit(
    state: &mut State,
    id: &ModelSessionId,
    config: &ModelSessionConfig,
    input: Option<InputId>,
    response: Option<ResponseId>,
    kind: EventKind,
) {
    state.seq += 1;
    state.queue.push_back(ModelSessionEvent {
        session_id: id.0.clone(),
        model_session_id: id.clone(),
        event_seq: state.seq,
        config_revision: config.expected_revision.unwrap_or_default(),
        input_id: input,
        response_id: response,
        tool_call_id: None,
        kind,
    });
    if let Some(waker) = state.waiter.take() {
        waker.wake();
    }
}
impl ModelSessionFactory for PilotDeckModelFactory {
    fn capabilities(&self) -> SessionCapabilities {
        SessionCapabilities {
            completion: true,
            streaming_output: true,
            tool_calls: true,
            ..Default::default()
        }
    }
    fn supported_profiles(&self) -> Vec<String> {
        vec![
            MODEL_REQUEST_PROFILE_V1.into(),
            MODEL_EVENT_PROFILE_V1.into(),
        ]
    }
    fn open(
        &self,
        config: ModelSessionConfig,
    ) -> BoxFuture<'_, Result<Arc<dyn ModelSession>, Error>> {
        Box::pin(async move {
            build_session(
                config,
                Some(Arc::new(WorkerConfig {
                    config: self.config.clone(),
                    transport: self.transport.clone(),
                })),
                None,
            )
        })
    }
}
fn build_session(
    config: ModelSessionConfig,
    factory: Option<Arc<WorkerConfig>>,
    shared: Option<Arc<SharedWorkerConfig>>,
) -> Result<Arc<dyn ModelSession>, Error> {
    if shared.is_none()
        && config
            .extensions
            .iter()
            .any(|(key, _)| key == REQUIRED_MODEL_LOCAL_RESOURCE_FACTS_PROFILE)
    {
        return Err(unsupported());
    }
    if config.binding.model.is_empty()
        || config
            .binding
            .endpoint_ref
            .as_deref()
            .is_none_or(str::is_empty)
    {
        return Err(invalid("model and endpoint binding are required"));
    }
    if config.mode != SessionMode::Request
        || !config.input_formats.is_empty()
        || !config.output_formats.is_empty()
    {
        return Err(unsupported());
    }
    if !config.extensions.iter().any(|(key, value)| {
        key == REQUIRED_MODEL_REQUEST_PROFILE && value == MODEL_REQUEST_PROFILE_V1
    }) || !config
        .extensions
        .iter()
        .any(|(key, value)| key == REQUIRED_MODEL_EVENT_PROFILE && value == MODEL_EVENT_PROFILE_V1)
    {
        return Err(invalid("required model profiles are missing"));
    }
    for (key, value) in &config.extensions {
        if (key == REQUIRED_MODEL_REQUEST_PROFILE && value != MODEL_REQUEST_PROFILE_V1)
            || (key == REQUIRED_MODEL_EVENT_PROFILE && value != MODEL_EVENT_PROFILE_V1)
            || (key == REQUIRED_MODEL_LOCAL_RESOURCE_FACTS_PROFILE
                && value != MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1)
        {
            return Err(unsupported());
        }
    }
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = ModelSessionId(format!(
        "pd-model-{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut state = State {
        state: SessionState::Ready,
        queue: VecDeque::new(),
        waiter: None,
        seq: 0,
        queued_bytes: 0,
        input_seq: 0,
        input: None,
        inspected: None,
        active: None,
        closing: false,
        uncertain: false,
        scope_owner: None,
        gate: None,
        gate_pending: false,
        release_profile: shared.is_some()
            && config.extensions.iter().any(|(key, value)| {
                key == REQUIRED_MODEL_LOCAL_RESOURCE_FACTS_PROFILE
                    && value == MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1
            }),
        pending_cleanup: 0,
        terminal_emitted: false,
        response_exposed: false,
        cancelled_response: None,
    };
    emit(
        &mut state,
        &id,
        &config,
        None,
        None,
        EventKind::SessionReady,
    );
    Ok(Arc::new(Session {
        id,
        config,
        factory,
        shared,
        state: Arc::new(Mutex::new(state)),
    }) as Arc<dyn ModelSession>)
}
impl ModelSession for Session {
    fn model_session_id(&self) -> &ModelSessionId {
        &self.id
    }
    fn state(&self) -> SessionState {
        self.state.lock().unwrap().state
    }
    fn append_input(&self, input: InputEnvelope) -> BoxFuture<'_, Result<InputAccepted, Error>> {
        Box::pin(async move {
            let accepted = {
                let mut s = self.state.lock().unwrap();
                if s.state != SessionState::Ready || s.closing || s.active.is_some() {
                    return Err(invalid("session is not available for input"));
                }
                if s.inspected.is_some() {
                    return Err(invalid("input group is sealed"));
                }
                if input.last_chunk {
                    return Err(invalid("last_chunk requires a scoped commit"));
                }
                if input.model_session_id != self.id
                    || input.kind != InputKind::RequestSnapshot
                    || input.content_type != MODEL_REQUEST_PROFILE_V1
                    || input
                        .expected_config_revision
                        .is_some_and(|r| r != self.config.expected_revision.unwrap_or_default())
                {
                    return Err(invalid("invalid request snapshot"));
                }
                if input.content.len() as u64 > self.config.limits.inline_block_bytes
                    || input.content.len() as u64 > self.config.limits.reliable_input_bytes
                {
                    return Err(invalid("input chunk exceeds limit"));
                }
                if s.input.is_none()
                    && (input.chunk_seq != 0 || self.config.limits.reliable_input_items == 0)
                {
                    return Err(invalid("invalid input sequence or item limit"));
                }
                let slot = s
                    .input
                    .get_or_insert_with(|| (input.input_id.clone(), 0, Vec::new()));
                if slot.0 != input.input_id
                    || slot.1 != input.chunk_seq
                    || (slot.2.len() + input.content.len()) as u64
                        > self.config.limits.reliable_input_bytes
                {
                    return Err(invalid("invalid input sequence or limit"));
                }
                slot.1 += 1;
                slot.2.extend_from_slice(&input.content);
                let remaining_bytes = self.config.limits.reliable_input_bytes - slot.2.len() as u64;
                s.input_seq += 1;
                InputAccepted {
                    input_id: input.input_id.clone(),
                    input_seq: s.input_seq,
                    status: InputStatus::Accepted,
                    remaining_items: self.config.limits.reliable_input_items.saturating_sub(1),
                    remaining_bytes,
                }
            };
            Ok(accepted)
        })
    }
    fn inspect_input(
        &self,
        input_id: InputId,
        expected_revision: ConfigRevision,
    ) -> BoxFuture<'_, Result<ModelInputRequirements, Error>> {
        Box::pin(async move {
            let mut state = self.state.lock().unwrap();
            if state.state != SessionState::Ready || state.closing || state.active.is_some() {
                return Err(invalid("session is not available for inspection"));
            }
            if expected_revision != self.config.expected_revision.unwrap_or_default() {
                return Err(invalid(
                    "inspection revision does not match effective configuration",
                ));
            }
            if let Some((id, revision, requires_media_input)) = &state.inspected {
                if id != &input_id || *revision != expected_revision {
                    return Err(invalid("another input is sealed"));
                }
                return Ok(ModelInputRequirements {
                    input_id,
                    config_revision: expected_revision,
                    requires_media_input: *requires_media_input,
                });
            }
            let (_, _, bytes) = state
                .input
                .as_ref()
                .filter(|(id, _, _)| id == &input_id)
                .ok_or_else(|| invalid("unknown input"))?;
            let raw = std::str::from_utf8(bytes).map_err(|e| invalid(e.to_string()))?;
            let parsed = parse_json(raw).map_err(|e| invalid(format!("{e:?}")))?;
            let requires_media_input = request::inspect_request_media(&parsed)
                .map_err(|e| invalid(format!("Unsupported: {e}")))?;
            state.inspected = Some((input_id.clone(), expected_revision, requires_media_input));
            Ok(ModelInputRequirements {
                input_id,
                config_revision: expected_revision,
                requires_media_input,
            })
        })
    }
    fn commit_input(&self, _: InputId) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async {
            Err(invalid(
                "model commit requires an admitted execution context",
            ))
        })
    }
    fn commit_input_scoped(
        &self,
        input_id: InputId,
        context: ModelExecutionContext,
    ) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            if context.session_id().is_empty()
                || context.model_session_id().is_empty()
                || context.job_id().is_empty()
                || context.principal().is_empty()
                || context.cancel().is_cancelled()
            {
                return Err(invalid("invalid or cancelled execution context"));
            }
            let deadline = monotonic_deadline(context.deadline())?;
            let scope_owner = (
                context.session_id().to_owned(),
                context.model_session_id().to_owned(),
            );
            let mut s = self.state.lock().unwrap();
            if s.state != SessionState::Ready || s.active.is_some() {
                return Err(invalid("session is not available"));
            }
            if s.scope_owner
                .as_ref()
                .is_some_and(|owner| owner != &scope_owner)
            {
                return Err(invalid("execution context belongs to another session"));
            }
            if s.input.as_ref().is_none_or(|x| x.0 != input_id) {
                return Err(invalid("unknown input"));
            }
            if !matches!(&s.inspected, Some((id, revision, false)) if id == &input_id && *revision == self.config.expected_revision.unwrap_or_default())
            {
                return Err(invalid(
                    "input was not inspected as a permitted text request",
                ));
            }
            s.inspected = None;
            let (_, _, bytes) = s.input.take().unwrap();
            let raw = String::from_utf8(bytes).map_err(|e| invalid(e.to_string()))?;
            let mut parsed = parse_json(&raw).map_err(|e| invalid(format!("{e:?}")))?;
            let bound_text = |value: &JsValue| {
                value
                    .as_string()
                    .and_then(|value| String::from_utf16(value.units()).ok())
            };
            if bound_text(field(&parsed, "provider")).as_deref()
                != self.config.binding.endpoint_ref.as_deref()
                || bound_text(field(&parsed, "model")).as_deref()
                    != Some(self.config.binding.model.as_str())
            {
                return Err(invalid(
                    "request does not match the authorised model binding",
                ));
            }
            if self.shared.is_some() && field(&parsed, "stream") == &JsValue::Bool(false) {
                return Err(unsupported());
            }
            let JsValue::Object(object) = &mut parsed else {
                return Err(invalid("request snapshot is not an object"));
            };
            object.insert("stream".into(), JsValue::Bool(true));
            let response = ResponseId(format!("{}-{}", self.id.0, s.input_seq));
            let cancel = context.cancel();
            s.scope_owner = Some(scope_owner);
            s.active = Some((input_id.clone(), response.clone(), cancel.clone()));
            s.response_exposed = false;
            s.cancelled_response = None;
            if self.shared.is_some() && s.release_profile {
                s.pending_cleanup += 1;
            }
            if self.shared.is_none() {
                emit(
                    &mut s,
                    &self.id,
                    &self.config,
                    Some(input_id.clone()),
                    Some(response.clone()),
                    EventKind::ResponseStarted,
                );
            }
            let state = self.state.clone();
            let config = self.config.clone();
            let id = self.id.clone();
            let factory = self.factory.clone();
            let shared = self.shared.clone();
            drop(s);
            if let Some(shared) = shared {
                let handle = shared.executor.clone();
                handle.spawn(run_shared(
                    shared,
                    state,
                    id,
                    config,
                    (input_id, response),
                    context,
                    parsed,
                ));
            } else if let Some(factory) = factory {
                std::thread::spawn(move || {
                    run(
                        factory,
                        state,
                        id,
                        config,
                        (input_id, response),
                        (context, deadline),
                        parsed,
                    )
                });
            } else {
                return Err(unsupported());
            }
            Ok(())
        })
    }
    fn append_tool_result(
        &self,
        _: ToolResultEnvelope,
    ) -> BoxFuture<'_, Result<InputAccepted, Error>> {
        Box::pin(async { Err(unsupported()) })
    }
    fn commit_tool_result(&self, _: &str) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Err(unsupported()) })
    }
    fn update_config(
        &self,
        _: ModelSessionConfig,
        _: ConfigRevision,
    ) -> BoxFuture<'_, Result<ConfigUpdate, Error>> {
        Box::pin(async { Err(unsupported()) })
    }
    fn interrupt(&self, response: &ResponseId) -> BoxFuture<'_, Result<InterruptAck, Error>> {
        let response = response.clone();
        Box::pin(async move {
            let mut s = self.state.lock().unwrap();
            if s.state == SessionState::Closed || s.terminal_emitted {
                return Err(invalid("session is closed"));
            }
            let Some((input, current, cancel)) = s.active.clone() else {
                // A typed host cancellation may have stopped this exact driver
                // before interrupt was polled. Acknowledge stopped delivery,
                // without guessing its cause or inventing another terminal.
                if s.cancelled_response.as_ref() == Some(&response) {
                    return Ok(InterruptAck::new(
                        response,
                        true,
                        BackendStopStatus::Unknown,
                        "interrupt".into(),
                    ));
                }
                return Err(invalid("no active response"));
            };
            if current != response {
                return Err(invalid("unknown response"));
            }
            cancel.cancel();
            if let Some(gate) = &s.gate {
                gate.pull();
            }
            s.active = None;
            s.state = SessionState::Unknown;
            s.response_exposed = true;
            emit(
                &mut s,
                &self.id,
                &self.config,
                Some(input),
                Some(current.clone()),
                EventKind::ResponseInterrupted {
                    delivery_stopped: true,
                    backend_stop: BackendStopStatus::Unknown,
                },
            );
            Ok(InterruptAck::new(
                current,
                true,
                BackendStopStatus::Unknown,
                "interrupt".into(),
            ))
        })
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            let mut s = self.state.lock().unwrap();
            if s.state == SessionState::Closed
                || (s.state == SessionState::Unknown && !s.release_profile)
                || s.terminal_emitted
            {
                return Ok(());
            }
            s.closing = true;
            s.inspected = None;
            if let Some((input, _, _)) = s.input.take() {
                emit(
                    &mut s,
                    &self.id,
                    &self.config,
                    Some(input),
                    None,
                    EventKind::InputDropped {
                        reason: "session closed before commit".into(),
                    },
                );
            }
            if s.active.is_none() && (!s.release_profile || s.pending_cleanup == 0) {
                let terminal = if s.uncertain || s.state == SessionState::Unknown {
                    s.state = SessionState::Unknown;
                    SessionTerminal::Unknown
                } else {
                    s.state = SessionState::Closed;
                    SessionTerminal::Closed
                };
                s.terminal_emitted = true;
                emit(
                    &mut s,
                    &self.id,
                    &self.config,
                    None,
                    None,
                    EventKind::SessionClosed { terminal },
                );
            } else if s.state != SessionState::Unknown {
                s.state = SessionState::Closing;
            }
            Ok(())
        })
    }
    fn abort(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            let mut s = self.state.lock().unwrap();
            if s.state == SessionState::Closed
                || (s.state == SessionState::Unknown && !s.release_profile)
                || s.terminal_emitted
            {
                return Ok(());
            }
            s.closing = true;
            s.inspected = None;
            if let Some((input, _, _)) = s.input.take() {
                emit(
                    &mut s,
                    &self.id,
                    &self.config,
                    Some(input),
                    None,
                    EventKind::InputDropped {
                        reason: "session aborted before commit".into(),
                    },
                );
            }
            let terminal = if let Some((_, _, cancel)) = s.active.take() {
                cancel.cancel();
                if let Some(gate) = &s.gate {
                    gate.pull();
                }
                s.state = SessionState::Unknown;
                SessionTerminal::Unknown
            } else if s.uncertain || s.state == SessionState::Unknown {
                s.state = SessionState::Unknown;
                SessionTerminal::Unknown
            } else {
                s.state = SessionState::Closed;
                SessionTerminal::Closed
            };
            if !s.release_profile || s.pending_cleanup == 0 {
                s.terminal_emitted = true;
                emit(
                    &mut s,
                    &self.id,
                    &self.config,
                    None,
                    None,
                    EventKind::SessionClosed { terminal },
                );
            }
            Ok(())
        })
    }
    fn next_event(&self) -> BoxFuture<'_, Result<Option<ModelSessionEvent>, Error>> {
        {
            let mut s = self.state.lock().unwrap();
            if s.gate_pending {
                if let Some(gate) = &s.gate {
                    gate.pull();
                }
                s.gate_pending = false;
            }
        }
        Box::pin(poll_fn(move |cx| {
            let mut s = self.state.lock().unwrap();
            if let Some(event) = s.queue.pop_front() {
                if let EventKind::Structured { event } = &event.kind {
                    s.queued_bytes = s
                        .queued_bytes
                        .saturating_sub(event.event_json().len() as u64);
                    if s.gate.is_some() {
                        s.gate_pending = true;
                    }
                }
                Poll::Ready(Ok(Some(event)))
            } else if matches!(s.state, SessionState::Closed | SessionState::Failed)
                || (s.state == SessionState::Unknown && (!s.release_profile || s.terminal_emitted))
            {
                Poll::Ready(Ok(None))
            } else {
                s.waiter = Some(cx.waker().clone());
                Poll::Pending
            }
        }))
    }
}

async fn run_shared(
    shared: Arc<SharedWorkerConfig>,
    state: Arc<Mutex<State>>,
    id: ModelSessionId,
    config: ModelSessionConfig,
    ids: (InputId, ResponseId),
    context: ModelExecutionContext,
    request: JsValue,
) {
    use agentcpm_iface::env::http_pull::HttpPullContext;
    use pilotdeck_model_runtime::{
        headers::DriverFailure,
        http::{self, StreamOutput},
    };
    let (input, response) = ids;
    let adapter = crate::pull::PullAdapter::new(
        shared.transport.clone(),
        HttpPullContext {
            session_id: context.session_id().into(),
            model_session_id: context.model_session_id().into(),
            job_id: context.job_id().into(),
            principal: context.principal().into(),
            deadline: context.deadline(),
            cancel: context.cancel(),
        },
    );
    let services = shared.jobs.for_job(
        &context,
        &config.binding,
        config.expected_revision.unwrap_or_default(),
    );
    let active = {
        let mut s = state.lock().unwrap();
        let active = s
            .active
            .as_ref()
            .is_some_and(|(_, current, _)| current == &response);
        if active {
            s.response_exposed = true;
            emit(
                &mut s,
                &id,
                &config,
                Some(input.clone()),
                Some(response.clone()),
                EventKind::ResponseStarted,
            );
        }
        active
    };
    if !active {
        drop(services);
        let released = adapter.cleanup().await;
        finish_cleanup(&state, &id, &config, &input, &response, &context, released);
        return;
    }
    let mut services = match services {
        Ok(services) => services,
        Err(error) => {
            let released = adapter.cleanup().await;
            finish_cleanup(&state, &id, &config, &input, &response, &context, released);
            // Only the actual scope/attempt receipt can prove release after
            // a supplier error; the error by itself is never sufficient.
            finish_shared(
                &state,
                &id,
                &config,
                (input, response),
                Some((error.to_string(), false)),
                released,
                error.kind == agentcpm_iface::env::http_pull::HttpFailureKind::Cancelled,
            );
            return;
        }
    };
    let gate = services.options.yield_gate.clone().unwrap_or_default();
    services.options.yield_gate = Some(gate.clone());
    let active = {
        let mut s = state.lock().unwrap();
        let active = s
            .active
            .as_ref()
            .is_some_and(|(_, current, _)| current == &response);
        if active {
            s.gate = Some(gate.clone());
        }
        active
    };
    if !active {
        drop(services);
        let released = adapter.cleanup().await;
        finish_cleanup(&state, &id, &config, &input, &response, &context, released);
        return;
    }
    let mut delivery_error: Option<String> = None;
    let mut provider_error: Option<(String, bool)> = None;
    let mut sink = |output| {
        let StreamOutput::Event(value) = output else {
            return;
        };
        if delivery_error.is_some() {
            return;
        }
        let kind = match event::structured(&value) {
            Ok(kind) => kind,
            Err(error) => {
                delivery_error = Some(error);
                context.cancel().cancel();
                return;
            }
        };
        let size = match &kind {
            EventKind::Structured { event } => event.event_json().len() as u64,
            _ => 0,
        };
        let mut s = state.lock().unwrap();
        if s.active
            .as_ref()
            .is_none_or(|(_, current, _)| current != &response)
        {
            return;
        }
        if s.queued_bytes.saturating_add(size) > config.limits.output_record_bytes {
            delivery_error = Some("model output buffer limit exceeded".into());
            drop(s);
            context.cancel().cancel();
            return;
        }
        if text(field(&value, "type")) == "error" {
            let error = field(&value, "error");
            provider_error = Some((
                text(field(error, "message")),
                field(error, "retryable").as_bool().unwrap_or(false),
            ));
        }
        s.queued_bytes += size;
        gate.deliver();
        emit(
            &mut s,
            &id,
            &config,
            Some(input.clone()),
            Some(response.clone()),
            kind,
        );
    };
    // The Kernel token is an independent hard gate, not a reconstruction of
    // the product turn's AbortSignal timestamp or its arbitration order.
    let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel();
    let token = context.cancel();
    let _registration = token.register_abort(Box::new(move || {
        let _ = cancel_tx.send(());
    }));
    let mut driving = Box::pin(http::stream_model(
        &adapter,
        services.host.as_ref(),
        &request,
        &services.safe_config,
        &services.options,
        &mut sink,
    ));
    let result = poll_fn(|cx| {
        use std::future::Future;
        if std::pin::Pin::new(&mut cancel_rx).poll(cx).is_ready() {
            return Poll::Ready(Err(DriverFailure::Host(
                agentcpm_iface::env::http_pull::HttpFailure::new(
                    agentcpm_iface::env::http_pull::HttpFailurePhase::Scope,
                    agentcpm_iface::env::http_pull::HttpFailureKind::Cancelled,
                    "model job cancelled",
                ),
            )));
        }
        driving.as_mut().poll(cx)
    })
    .await;
    drop(driving);
    drop(sink);
    drop(services);
    let released = adapter.cleanup().await;
    finish_cleanup(&state, &id, &config, &input, &response, &context, released);
    let cancelled = delivery_error.is_none()
        && matches!(&result,
        Err(DriverFailure::Host(error)) if error.kind == agentcpm_iface::env::http_pull::HttpFailureKind::Cancelled);
    let failure = delivery_error
        .map(|error| (error, false))
        .or(provider_error)
        .or_else(|| match result {
            Ok(()) => None,
            Err(DriverFailure::Host(error)) => Some((error.to_string(), false)),
            Err(DriverFailure::Model(error)) => {
                let retryable =
                    pilotdeck_model_core::driver::errors::is_retryable_stream_error(error.view());
                Some((error.message_text(), retryable))
            }
        })
        .or_else(|| {
            (!released).then(|| {
                (
                    "model transport resource release was not confirmed".into(),
                    false,
                )
            })
        });
    finish_shared(
        &state,
        &id,
        &config,
        (input, response),
        failure,
        released,
        cancelled,
    );
}

/// Complete one original worker's local cleanup independently of its delivery
/// slot. The original context/token is retained, never reconstructed from ids.
fn finish_cleanup(
    state: &Arc<Mutex<State>>,
    id: &ModelSessionId,
    config: &ModelSessionConfig,
    input: &InputId,
    response: &ResponseId,
    context: &ModelExecutionContext,
    released: bool,
) {
    let mut s = state.lock().unwrap();
    if !s.release_profile || s.pending_cleanup == 0 {
        return;
    }
    s.pending_cleanup -= 1;
    if released {
        let response = s.response_exposed.then(|| response.clone());
        emit(
            &mut s,
            id,
            config,
            Some(input.clone()),
            response,
            EventKind::LocalResourcesReleased {
                release: ModelLocalResourceRelease::new(context.clone()),
            },
        );
    }
    if s.closing && s.active.is_none() && s.pending_cleanup == 0 && !s.terminal_emitted {
        s.terminal_emitted = true;
        let terminal = if s.state == SessionState::Unknown || s.uncertain || !released {
            s.state = SessionState::Unknown;
            SessionTerminal::Unknown
        } else {
            s.state = SessionState::Closed;
            SessionTerminal::Closed
        };
        emit(
            &mut s,
            id,
            config,
            None,
            None,
            EventKind::SessionClosed { terminal },
        );
    } else if let Some(waker) = s.waiter.take() {
        waker.wake();
    }
}

fn finish_shared(
    state: &Arc<Mutex<State>>,
    id: &ModelSessionId,
    config: &ModelSessionConfig,
    ids: (InputId, ResponseId),
    failure: Option<(String, bool)>,
    released: bool,
    cancelled: bool,
) {
    let (input, response) = ids;
    let mut s = state.lock().unwrap();
    if s.active
        .as_ref()
        .is_none_or(|(_, current, _)| current != &response)
    {
        return;
    }
    s.active = None;
    if cancelled {
        s.cancelled_response = Some(response.clone());
    }
    s.gate = None;
    s.gate_pending = false;
    let kind = if let Some((message, retryable)) = failure {
        s.uncertain |= !released;
        EventKind::ResponseFailed {
            code: -32000,
            message,
            retryable,
            resource_released: released,
        }
    } else {
        EventKind::ResponseCompleted
    };
    emit(&mut s, id, config, Some(input), Some(response), kind);
    if s.closing {
        let terminal = if s.uncertain {
            s.state = SessionState::Unknown;
            SessionTerminal::Unknown
        } else {
            s.state = SessionState::Closed;
            SessionTerminal::Closed
        };
        s.terminal_emitted = true;
        emit(
            &mut s,
            id,
            config,
            None,
            None,
            EventKind::SessionClosed { terminal },
        );
    }
}

struct Sink {
    state: Arc<Mutex<State>>,
    id: ModelSessionId,
    config: ModelSessionConfig,
    input: InputId,
    response: ResponseId,
    decoder: SseDecoder,
    normalizer: codec::StreamState,
    google: Option<google::StreamState>,
    status: Option<u16>,
    error_body: Vec<u8>,
    saw_end: bool,
    saw_error: Option<(String, bool)>,
}
impl Sink {
    fn publish(&mut self, value: &JsValue) -> Result<(), String> {
        let kind = event::structured(value)?;
        let size = match &kind {
            EventKind::Structured { event } => event.event_json().len() as u64,
            _ => 0,
        };
        let mut s = self.state.lock().unwrap();
        if s.queued_bytes.saturating_add(size) > self.config.limits.output_record_bytes {
            return Err("model output buffer limit exceeded".into());
        }
        if s.active
            .as_ref()
            .is_none_or(|(_, response, _)| response != &self.response)
        {
            return Err("response cancelled".into());
        }
        s.queued_bytes += size;
        if text(field(value, "type")) == "message_end" {
            self.saw_end = true;
        }
        if text(field(value, "type")) == "error" {
            let error = field(value, "error");
            self.saw_error = Some((
                text(field(error, "message")),
                field(error, "retryable").as_bool().unwrap_or(false),
            ));
        }
        emit(
            &mut s,
            &self.id,
            &self.config,
            Some(self.input.clone()),
            Some(self.response.clone()),
            kind,
        );
        Ok(())
    }
    fn frames(&mut self, frames: Vec<String>) -> Result<(), String> {
        for frame in frames {
            if frame == "[DONE]" {
                self.saw_end = true;
                continue;
            }
            let raw =
                parse_json(&frame).map_err(|e| format!("invalid provider stream JSON: {e:?}"))?;
            let events = if let Some(google) = &mut self.google {
                google.normalize(&raw).map_err(codec_error)?
            } else {
                self.normalizer.normalize(&raw).map_err(codec_error)?
            };
            for event in events {
                self.publish(&event)?;
            }
        }
        Ok(())
    }
}
impl HttpStreamSink for Sink {
    fn head(&mut self, head: HttpStreamHead) -> Result<(), String> {
        self.status = Some(head.status);
        Ok(())
    }
    fn body(&mut self, bytes: &[u8]) -> Result<(), String> {
        if !self
            .status
            .is_some_and(|status| (200..300).contains(&status))
        {
            if self.error_body.len().saturating_add(bytes.len()) as u64
                > self.config.limits.output_record_bytes
            {
                return Err("provider error body limit exceeded".into());
            }
            self.error_body.extend_from_slice(bytes);
            return Ok(());
        }
        if self.decoder.buffered_len().saturating_add(bytes.len()) as u64
            > self.config.limits.output_record_bytes
        {
            return Err("provider stream frame limit exceeded".into());
        }
        let frames = self.decoder.push(bytes)?;
        self.frames(frames)
    }
}
fn run(
    factory: Arc<WorkerConfig>,
    state: Arc<Mutex<State>>,
    id: ModelSessionId,
    config: ModelSessionConfig,
    ids: (InputId, ResponseId),
    execution: (ModelExecutionContext, Option<Instant>),
    request_snapshot: JsValue,
) {
    let (input, response) = ids;
    let (context, deadline) = execution;
    let mut failure_retryable = false;
    // No transport call means no attempt was created. Once execute is entered,
    // this legacy port has no resource receipt, even if it later reaches EOF.
    let mut failure_released = true;
    let result = (|| -> Result<(), String> {
        let (provider, model) = codec::validate_model_request(&request_snapshot, &factory.config)
            .map_err(codec_error)?;
        let body = codec::build_model_request(&request_snapshot, &provider, &model)
            .map_err(codec_error)?;
        let is_google = text(field(&provider, "protocol")) == "google";
        let body = if is_google {
            google::final_body(&body, &provider)
        } else {
            request::final_body(&body, &provider)
        };
        let urls = if is_google {
            vec![
                google::endpoint_with_stream(&provider, field(&request_snapshot, "model"), true)
                    .map_err(codec_error)?,
            ]
        } else {
            codec::chat_endpoint_candidates(&provider)
        };
        if urls.is_empty() {
            return Err("no provider endpoint".into());
        }
        let headers = if is_google {
            google::headers(&provider).map_err(codec_error)?
        } else {
            codec::provider_headers(&provider, &body)
        };
        let headers = headers
            .into_iter()
            .map(|(k, v)| (String::from_utf16_lossy(k.units()), text(&v).into_bytes()))
            .filter(|(name, _)| {
                !["authorization", "x-api-key", "x-goog-api-key"]
                    .iter()
                    .any(|reserved| name.eq_ignore_ascii_case(reserved))
            })
            .collect();
        let mut request = HttpStreamRequest {
            method: "POST".into(),
            url: String::new(),
            headers,
            body: json(&body)?.into_bytes(),
        };
        let control = HttpStreamControl {
            deadline,
            session: context.session_id().to_owned(),
            principal: context.principal().to_owned(),
            correlation_id: context.correlation_id().to_owned(),
            cancel: context.cancel(),
        };
        let mut sink = Sink {
            state: state.clone(),
            id: id.clone(),
            config: config.clone(),
            input: input.clone(),
            response: response.clone(),
            decoder: SseDecoder::default(),
            normalizer: codec::StreamState::new(&provider),
            google: is_google.then(google::StreamState::default),
            status: None,
            error_body: Vec::new(),
            saw_end: false,
            saw_error: None,
        };
        sink.publish(
            &codec::request_started_event(&provider, &request_snapshot).map_err(codec_error)?,
        )?;
        for (index, url) in urls.iter().enumerate() {
            request.url.clone_from(url);
            sink.status = None;
            sink.error_body.clear();
            if control.cancel.is_cancelled()
                || control
                    .deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
            {
                return Err("execution cancelled or expired before dispatch".into());
            }
            failure_released = false;
            factory.transport.execute(&request, &control, &mut sink)?;
            if index + 1 == urls.len() || !matches!(sink.status, Some(400 | 404 | 405)) {
                break;
            }
        }
        if !sink
            .status
            .is_some_and(|status| (200..300).contains(&status))
        {
            let status = sink
                .status
                .ok_or("transport ended without response headers")?;
            let raw = String::from_utf8_lossy(&sink.error_body).into_owned();
            let raw = parse_json(&raw).unwrap_or_else(|_| JsValue::String(raw.as_str().into()));
            let provider_id = field(&provider, "id")
                .as_string()
                .cloned()
                .unwrap_or_default();
            let protocol = field(&provider, "protocol")
                .as_string()
                .cloned()
                .unwrap_or_default();
            let canonical = codec::normalize_error::normalize_model_error(
                &provider_id,
                &protocol,
                codec::normalize_error::ErrorInput::Value(&raw),
                Some(status as f64),
            );
            failure_retryable = canonical
                .get("retryable")
                .and_then(JsValue::as_bool)
                .unwrap_or(false);
            let message = canonical
                .get("message")
                .and_then(JsValue::as_string)
                .map_or_else(String::new, |s: &JsString| {
                    String::from_utf16_lossy(s.units())
                });
            let mut event = JsObject::new();
            event.insert("type".into(), "error".into());
            event.insert("error".into(), JsValue::Object(canonical));
            sink.publish(&JsValue::Object(event))?;
            return Err(message);
        }
        let frames = sink.decoder.finish()?;
        sink.frames(frames)?;
        if let Some((error, retryable)) = sink.saw_error {
            failure_retryable = retryable;
            return Err(error);
        }
        if !sink.saw_end {
            return Err("provider stream ended without a completion sentinel".into());
        }
        Ok(())
    })();
    let mut s = state.lock().unwrap();
    if s.active
        .as_ref()
        .is_none_or(|(_, active, _)| active != &response)
    {
        return;
    }
    s.active = None;
    let kind = match result {
        Ok(()) => EventKind::ResponseCompleted,
        Err(message) => {
            s.uncertain |= !failure_released;
            EventKind::ResponseFailed {
                code: -32000,
                message,
                retryable: failure_retryable,
                resource_released: failure_released,
            }
        }
    };
    emit(&mut s, &id, &config, Some(input), Some(response), kind);
    if s.closing {
        let terminal = if s.uncertain {
            s.state = SessionState::Unknown;
            SessionTerminal::Unknown
        } else {
            s.state = SessionState::Closed;
            SessionTerminal::Closed
        };
        emit(
            &mut s,
            &id,
            &config,
            None,
            None,
            EventKind::SessionClosed { terminal },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::Future,
        pin::pin,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        task::{Context, Wake, Waker},
        time::Duration,
    };

    struct ThreadWake(std::thread::Thread);
    impl Wake for ThreadWake {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }
    fn block_on<T>(future: impl Future<Output = T>) -> T {
        let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
        let mut cx = Context::from_waker(&waker);
        let mut future = pin!(future);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match future.as_mut().poll(&mut cx) {
                Poll::Ready(value) => return value,
                Poll::Pending => {
                    assert!(Instant::now() < deadline, "future timed out");
                    std::thread::park_timeout(Duration::from_millis(10));
                }
            }
        }
    }
    fn config() -> ModelSessionConfig {
        ModelSessionConfig {
            binding: ModelBinding::new("m".into(), Some("p".into()), vec![]),
            mode: SessionMode::Request,
            input_formats: vec![],
            output_formats: vec![],
            tool_catalog_ref: None,
            generation: GenerationSettings::default(),
            limits: SessionLimits::default(),
            timeouts: SessionTimeouts::default(),
            extensions: vec![
                (
                    REQUIRED_MODEL_REQUEST_PROFILE.into(),
                    MODEL_REQUEST_PROFILE_V1.into(),
                ),
                (
                    REQUIRED_MODEL_EVENT_PROFILE.into(),
                    MODEL_EVENT_PROFILE_V1.into(),
                ),
            ],
            expected_revision: None,
        }
    }
    struct Script {
        attempts: AtomicUsize,
        block: bool,
        exited: AtomicBool,
    }
    impl HttpStreamTransport for Script {
        fn execute(
            &self,
            _: &HttpStreamRequest,
            control: &HttpStreamControl,
            sink: &mut dyn HttpStreamSink,
        ) -> Result<(), String> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            sink.head(HttpStreamHead {
                status: if attempt == 0 && !self.block {
                    503
                } else {
                    200
                },
                headers: vec![],
            })?;
            if self.block {
                while !control.cancel.is_cancelled()
                    && control
                        .deadline
                        .is_none_or(|deadline| Instant::now() < deadline)
                {
                    std::thread::sleep(Duration::from_millis(2));
                }
                self.exited.store(true, Ordering::SeqCst);
                return Err("cancelled".into());
            }
            if attempt == 0 {
                sink.body(b"unavailable")?;
                return Ok(());
            }
            let bytes = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1,\"total_tokens\":3}}\n\ndata: [DONE]\n\n";
            for chunk in bytes.chunks(7) {
                sink.body(chunk)?;
            }
            Ok(())
        }
    }
    struct BodyCapture {
        script: Arc<Script>,
        bodies: Mutex<Vec<Vec<u8>>>,
    }
    impl HttpStreamTransport for BodyCapture {
        fn execute(
            &self,
            request: &HttpStreamRequest,
            control: &HttpStreamControl,
            sink: &mut dyn HttpStreamSink,
        ) -> Result<(), String> {
            self.bodies.lock().unwrap().push(request.body.clone());
            self.script.execute(request, control, sink)
        }
    }
    fn factory(script: Arc<Script>) -> PilotDeckModelFactory {
        PilotDeckModelFactory { config: parse_json(r#"{"providers":{"p":{"id":"p","protocol":"openai","url":"http://localhost","models":{"m":{"id":"m","capabilities":{"supportsStreaming":true},"multimodal":{"input":["text"]}}}}}}"#).unwrap(), transport: script }
    }
    fn snapshot(session: &dyn ModelSession, id: &str, kind: &str) -> InputEnvelope {
        InputEnvelope {
            model_session_id: session.model_session_id().clone(),
            client_message_id: id.into(),
            expected_config_revision: None,
            input_id: InputId(id.into()),
            chunk_seq: 0,
            last_chunk: false,
            kind: InputKind::RequestSnapshot,
            content_type: kind.into(),
            content: br#"{"provider":"p","model":"m","stream":true,"messages":[]}"#.to_vec(),
            media_seq: None,
            media_time_ms: None,
            media_duration_ms: None,
        }
    }
    fn scope(
        session: &dyn ModelSession,
        job: &str,
        cancel: HttpStreamCancel,
        deadline: Option<agentcpm_iface::types::Deadline>,
    ) -> ModelExecutionContext {
        ModelExecutionContext::new(
            "trusted-session".into(),
            session.model_session_id().0.clone(),
            job.into(),
            "trusted-principal".into(),
            deadline,
            cancel,
        )
    }
    fn event(session: &dyn ModelSession) -> ModelSessionEvent {
        block_on(session.next_event()).unwrap().expect("event")
    }
    fn inspect(session: &dyn ModelSession, id: &str) -> ModelInputRequirements {
        block_on(session.inspect_input(InputId(id.into()), ConfigRevision(0))).unwrap()
    }
    fn run_attempt(session: &dyn ModelSession, id: &str) -> Vec<EventKind> {
        block_on(session.append_input(snapshot(session, id, MODEL_REQUEST_PROFILE_V1))).unwrap();
        assert!(!inspect(session, id).requires_media_input);
        block_on(session.commit_input_scoped(
            InputId(id.into()),
            scope(session, id, HttpStreamCancel::default(), None),
        ))
        .unwrap();
        let mut result = Vec::new();
        loop {
            let kind = event(session).kind;
            let done = matches!(
                kind,
                EventKind::ResponseCompleted | EventKind::ResponseFailed { .. }
            );
            result.push(kind);
            if done {
                return result;
            }
        }
    }
    #[test]
    fn inspected_input_is_complete_sealed_and_same_bytes_are_committed() {
        let script = Arc::new(Script {
            attempts: AtomicUsize::new(0),
            block: false,
            exited: AtomicBool::new(false),
        });
        let capture = Arc::new(BodyCapture {
            script: script.clone(),
            bodies: Mutex::new(Vec::new()),
        });
        let mut factory = factory(script.clone());
        factory.transport = capture.clone();
        let session = block_on(factory.open(config())).ok().unwrap();
        let _ = event(&*session);
        let mut first = snapshot(&*session, "chunks", MODEL_REQUEST_PROFILE_V1);
        let full = br#"{"provider":"p","model":"m","messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}]}"#;
        first.content = full[..full.len() / 2].to_vec();
        block_on(session.append_input(first)).unwrap();
        assert!(
            block_on(session.inspect_input(InputId("chunks".into()), ConfigRevision(0))).is_err()
        );
        let mut second = snapshot(&*session, "chunks", MODEL_REQUEST_PROFILE_V1);
        second.chunk_seq = 1;
        second.content = full[full.len() / 2..].to_vec();
        block_on(session.append_input(second)).unwrap();
        assert!(
            block_on(session.commit_input_scoped(
                InputId("chunks".into()),
                scope(&*session, "uninspected", HttpStreamCancel::default(), None)
            ))
            .is_err()
        );
        assert!(
            block_on(session.inspect_input(InputId("chunks".into()), ConfigRevision(1))).is_err()
        );
        let result = inspect(&*session, "chunks");
        assert!(!result.requires_media_input);
        assert_eq!(inspect(&*session, "chunks"), result);
        let mut late = snapshot(&*session, "chunks", MODEL_REQUEST_PROFILE_V1);
        late.chunk_seq = 2;
        assert!(block_on(session.append_input(late)).is_err());
        assert_eq!(script.attempts.load(Ordering::SeqCst), 0);
        block_on(session.commit_input_scoped(
            InputId("chunks".into()),
            scope(&*session, "admitted", HttpStreamCancel::default(), None),
        ))
        .unwrap();
        loop {
            if matches!(
                event(&*session).kind,
                EventKind::ResponseCompleted | EventKind::ResponseFailed { .. }
            ) {
                break;
            }
        }
        assert_eq!(script.attempts.load(Ordering::SeqCst), 1);
        let bodies = capture.bodies.lock().unwrap();
        assert_eq!(bodies.len(), 1);
        let body = parse_json(std::str::from_utf8(&bodies[0]).unwrap()).unwrap();
        assert_eq!(
            field(&body, "messages"),
            &parse_json(r#"[{"role":"user","content":"hello"}]"#).unwrap()
        );
        assert_eq!(field(&body, "model"), &JsValue::String("m".into()));
        assert_eq!(field(&body, "stream"), &JsValue::Bool(true));
    }
    #[test]
    fn inspection_detects_nested_media_and_rejects_unknown_content_before_dispatch() {
        let script = Arc::new(Script {
            attempts: AtomicUsize::new(0),
            block: false,
            exited: AtomicBool::new(false),
        });
        let factory = factory(script.clone());
        for (id, request, expected) in [
            (
                "image",
                r#"{"provider":"p","model":"m","messages":[{"role":"user","content":[{"type":"image","source":"base64","data":"AA","mimeType":"image/png"}]}]}"#,
                Some(true),
            ),
            (
                "nested",
                r#"{"provider":"p","model":"m","messages":[{"role":"user","content":[{"type":"tool_result","toolCallId":"t","content":[{"type":"image","source":"base64","data":"AA","mimeType":"image/png"}]}]}]}"#,
                Some(true),
            ),
            (
                "unknown",
                r#"{"provider":"p","model":"m","messages":[{"role":"user","content":[{"type":"unrecognized"}]}]}"#,
                None,
            ),
        ] {
            let session = block_on(factory.open(config())).ok().unwrap();
            let _ = event(&*session);
            let mut input = snapshot(&*session, id, MODEL_REQUEST_PROFILE_V1);
            input.content = request.as_bytes().to_vec();
            block_on(session.append_input(input)).unwrap();
            let result = block_on(session.inspect_input(InputId(id.into()), ConfigRevision(0)));
            match expected {
                Some(media) => assert_eq!(result.unwrap().requires_media_input, media),
                None => assert!(result.is_err()),
            }
            assert!(
                block_on(session.commit_input_scoped(
                    InputId(id.into()),
                    scope(&*session, id, HttpStreamCancel::default(), None)
                ))
                .is_err()
            );
        }
        assert_eq!(script.attempts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn unscoped_expired_and_cancelled_commits_never_dispatch() {
        let script = Arc::new(Script {
            attempts: AtomicUsize::new(0),
            block: false,
            exited: AtomicBool::new(false),
        });
        let session = block_on(factory(script.clone()).open(config()))
            .ok()
            .unwrap();
        let _ = event(&*session);
        let mut last = snapshot(&*session, "last", MODEL_REQUEST_PROFILE_V1);
        last.last_chunk = true;
        assert!(block_on(session.append_input(last)).is_err());
        block_on(session.append_input(snapshot(&*session, "one", MODEL_REQUEST_PROFILE_V1)))
            .unwrap();
        assert!(block_on(session.commit_input(InputId("one".into()))).is_err());
        let expired = Some(agentcpm_iface::types::Deadline::at_ms(0));
        assert!(
            block_on(session.commit_input_scoped(
                InputId("one".into()),
                scope(&*session, "expired", HttpStreamCancel::default(), expired)
            ))
            .is_err()
        );
        let cancelled = HttpStreamCancel::default();
        cancelled.cancel();
        assert!(
            block_on(session.commit_input_scoped(
                InputId("one".into()),
                scope(&*session, "cancelled", cancelled, None)
            ))
            .is_err()
        );
        let empty = ModelExecutionContext::new(
            "".into(),
            "ms".into(),
            "job".into(),
            "principal".into(),
            None,
            HttpStreamCancel::default(),
        );
        assert!(block_on(session.commit_input_scoped(InputId("one".into()), empty)).is_err());
        assert!(block_on(session.commit_tool_result_scoped("old-tool", None)).is_err());
        assert!(
            block_on(session.commit_tool_result_scoped(
                "old-tool",
                Some(scope(
                    &*session,
                    "new-job",
                    HttpStreamCancel::default(),
                    None
                ))
            ))
            .is_err()
        );
        assert_eq!(script.attempts.load(Ordering::SeqCst), 0);
        assert!(!inspect(&*session, "one").requires_media_input);
        block_on(session.commit_input_scoped(
            InputId("one".into()),
            scope(&*session, "admitted", HttpStreamCancel::default(), None),
        ))
        .unwrap();
        loop {
            if matches!(
                event(&*session).kind,
                EventKind::ResponseCompleted | EventKind::ResponseFailed { .. }
            ) {
                break;
            }
        }
        assert_eq!(script.attempts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn session_scope_cannot_be_rebound_between_generations() {
        let script = Arc::new(Script {
            attempts: AtomicUsize::new(0),
            block: false,
            exited: AtomicBool::new(false),
        });
        let session = block_on(factory(script.clone()).open(config()))
            .ok()
            .unwrap();
        let _ = event(&*session);
        let _ = run_attempt(&*session, "first");
        block_on(session.append_input(snapshot(&*session, "second", MODEL_REQUEST_PROFILE_V1)))
            .unwrap();
        assert!(!inspect(&*session, "second").requires_media_input);
        let foreign = ModelExecutionContext::new(
            "other-session".into(),
            "other-model-session".into(),
            "other-job".into(),
            "trusted-principal".into(),
            None,
            HttpStreamCancel::default(),
        );
        assert!(block_on(session.commit_input_scoped(InputId("second".into()), foreign)).is_err());
        assert_eq!(script.attempts.load(Ordering::SeqCst), 1);
        block_on(session.commit_input_scoped(
            InputId("second".into()),
            scope(&*session, "second-job", HttpStreamCancel::default(), None),
        ))
        .unwrap();
        loop {
            if matches!(
                event(&*session).kind,
                EventKind::ResponseCompleted | EventKind::ResponseFailed { .. }
            ) {
                break;
            }
        }
        assert_eq!(script.attempts.load(Ordering::SeqCst), 2);
    }

    #[derive(Default)]
    struct ScopedProbe {
        started: Mutex<Vec<(String, String, String, Option<Instant>)>>,
        exited: Mutex<Vec<String>>,
    }
    impl HttpStreamTransport for ScopedProbe {
        fn execute(
            &self,
            _: &HttpStreamRequest,
            control: &HttpStreamControl,
            sink: &mut dyn HttpStreamSink,
        ) -> Result<(), String> {
            self.started.lock().unwrap().push((
                control.session.clone(),
                control.principal.clone(),
                control.correlation_id.clone(),
                control.deadline,
            ));
            sink.head(HttpStreamHead {
                status: 200,
                headers: vec![],
            })?;
            let limit = Instant::now() + Duration::from_secs(3);
            while !control.cancel.is_cancelled() && Instant::now() < limit {
                std::thread::sleep(Duration::from_millis(2));
            }
            self.exited
                .lock()
                .unwrap()
                .push(control.correlation_id.clone());
            Err("stopped locally".into())
        }
    }
    #[test]
    fn separate_admitted_jobs_have_isolated_cancellation_and_trusted_identity() {
        let probe = Arc::new(ScopedProbe::default());
        let dummy = Arc::new(Script {
            attempts: AtomicUsize::new(0),
            block: false,
            exited: AtomicBool::new(false),
        });
        let mut factory = factory(dummy);
        factory.transport = probe.clone();
        let a = block_on(factory.open(config())).ok().unwrap();
        let b = block_on(factory.open(config())).ok().unwrap();
        let _ = event(&*a);
        let _ = event(&*b);
        for (session, id) in [(&*a, "a"), (&*b, "b")] {
            let mut input = snapshot(session, id, MODEL_REQUEST_PROFILE_V1);
            input.content = br#"{"provider":"p","model":"m","messages":[],"session":"forged","principal":"forged","correlation_id":"forged","deadline":0}"#.to_vec();
            block_on(session.append_input(input)).unwrap();
            assert!(!inspect(session, id).requires_media_input);
        }
        let cancel_a = HttpStreamCancel::default();
        let cancel_b = HttpStreamCancel::default();
        let before = Instant::now();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let deadline_a = agentcpm_iface::types::Deadline::after_ms(now_ms, 10_000);
        block_on(a.commit_input_scoped(
            InputId("a".into()),
            scope(&*a, "job-a", cancel_a.clone(), Some(deadline_a)),
        ))
        .unwrap();
        block_on(b.commit_input_scoped(
            InputId("b".into()),
            scope(&*b, "job-b", cancel_b.clone(), None),
        ))
        .unwrap();
        let limit = Instant::now() + Duration::from_secs(2);
        while probe.started.lock().unwrap().len() != 2 {
            assert!(Instant::now() < limit);
            std::thread::yield_now();
        }
        let identities = probe.started.lock().unwrap().clone();
        let job_a = identities.iter().find(|entry| entry.2 == "job-a").unwrap();
        let job_b = identities.iter().find(|entry| entry.2 == "job-b").unwrap();
        assert_eq!(
            (&*job_a.0, &*job_a.1),
            ("trusted-session", "trusted-principal")
        );
        assert_eq!(
            (&*job_b.0, &*job_b.1),
            ("trusted-session", "trusted-principal")
        );
        assert!(
            job_a
                .3
                .is_some_and(|deadline| deadline <= before + Duration::from_millis(10_050))
        );
        assert_eq!(job_b.3, None);
        block_on(a.abort()).unwrap();
        assert!(cancel_a.is_cancelled());
        while !probe.exited.lock().unwrap().contains(&"job-a".to_owned()) {
            assert!(Instant::now() < limit);
            std::thread::yield_now();
        }
        assert!(!cancel_b.is_cancelled());
        assert!(!probe.exited.lock().unwrap().contains(&"job-b".to_owned()));
        cancel_b.cancel();
        while !probe.exited.lock().unwrap().contains(&"job-b".to_owned()) {
            assert!(Instant::now() < limit);
            std::thread::yield_now();
        }
    }

    #[test]
    fn profiles_input_commit_failure_then_late_usage_success() {
        let script = Arc::new(Script {
            attempts: AtomicUsize::new(0),
            block: false,
            exited: AtomicBool::new(false),
        });
        let factory = factory(script.clone());
        let mut missing = config();
        missing.extensions.clear();
        assert!(block_on(factory.open(missing)).is_err());
        let wrong = block_on(factory.open(config())).ok().unwrap();
        let _ = event(&*wrong);
        let mut request = snapshot(&*wrong, "wrong-binding", MODEL_REQUEST_PROFILE_V1);
        request.content = br#"{"provider":"other","model":"m","messages":[]}"#.to_vec();
        block_on(wrong.append_input(request)).unwrap();
        assert!(!inspect(&*wrong, "wrong-binding").requires_media_input);
        assert!(
            block_on(wrong.commit_input_scoped(
                InputId("wrong-binding".into()),
                scope(&*wrong, "wrong-job", HttpStreamCancel::default(), None)
            ))
            .is_err()
        );
        assert_eq!(script.attempts.load(Ordering::SeqCst), 0);
        let pending = block_on(factory.open(config())).ok().unwrap();
        let _ = event(&*pending);
        block_on(pending.append_input(snapshot(&*pending, "pending", MODEL_REQUEST_PROFILE_V1)))
            .unwrap();
        assert_eq!(script.attempts.load(Ordering::SeqCst), 0);
        assert!(
            block_on(pending.commit_input_scoped(
                InputId("wrong".into()),
                scope(&*pending, "wrong-id", HttpStreamCancel::default(), None)
            ))
            .is_err()
        );
        assert_eq!(script.attempts.load(Ordering::SeqCst), 0);
        block_on(pending.close()).unwrap();
        let session = block_on(factory.open(config())).ok().unwrap();
        assert!(matches!(event(&*session).kind, EventKind::SessionReady));
        assert!(block_on(session.append_input(snapshot(&*session, "bad", ""))).is_err());
        assert_eq!(script.attempts.load(Ordering::SeqCst), 0);
        let first = run_attempt(&*session, "one");
        assert!(matches!(
            first.last(),
            Some(EventKind::ResponseFailed {
                retryable: true,
                ..
            })
        ));
        let second = run_attempt(&*session, "two");
        let positions: Vec<_> = second
            .iter()
            .filter_map(|kind| match kind {
                EventKind::Structured { event } => Some(event.event_type().as_str()),
                EventKind::ResponseCompleted => Some("completed"),
                _ => None,
            })
            .collect();
        let end = positions.iter().position(|v| *v == "message_end").unwrap();
        let usage = positions.iter().position(|v| *v == "usage").unwrap();
        assert!(end < usage && usage < positions.iter().position(|v| *v == "completed").unwrap());
        assert_eq!(positions.iter().filter(|v| **v == "completed").count(), 1);
        block_on(session.close()).unwrap();
        block_on(session.close()).unwrap();
        assert!(matches!(
            event(&*session).kind,
            EventKind::SessionClosed {
                terminal: SessionTerminal::Unknown
            }
        ));
        assert!(block_on(session.next_event()).unwrap().is_none());
    }
    #[test]
    fn legacy_factory_direct_open_rejects_release_profile_it_does_not_support() {
        let script = Arc::new(Script {
            attempts: AtomicUsize::new(0),
            block: false,
            exited: AtomicBool::new(false),
        });
        let factory = factory(script.clone());
        let mut cfg = config();
        cfg.extensions.push((
            REQUIRED_MODEL_LOCAL_RESOURCE_FACTS_PROFILE.into(),
            MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1.into(),
        ));
        assert!(block_on(factory.open(cfg)).is_err());
        assert_eq!(script.attempts.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn google_wire_is_projected_once_for_citation_metadata() {
        let id = ModelSessionId("test".into());
        let input = InputId("input".into());
        let response = ResponseId("response".into());
        let state = Arc::new(Mutex::new(State {
            state: SessionState::Ready,
            queue: VecDeque::new(),
            waiter: None,
            seq: 0,
            queued_bytes: 0,
            input_seq: 1,
            input: None,
            inspected: None,
            active: Some((input.clone(), response.clone(), HttpStreamCancel::default())),
            closing: false,
            uncertain: false,
            scope_owner: None,
            gate: None,
            gate_pending: false,
            release_profile: false,
            pending_cleanup: 0,
            terminal_emitted: false,
            response_exposed: false,
            cancelled_response: None,
        }));
        let provider = parse_json(r#"{"protocol":"google"}"#).unwrap();
        let mut sink = Sink {
            state: state.clone(),
            id,
            config: config(),
            input,
            response,
            decoder: SseDecoder::default(),
            normalizer: codec::StreamState::new(&provider),
            google: Some(google::StreamState::default()),
            status: Some(200),
            error_body: vec![],
            saw_end: false,
            saw_error: None,
        };
        sink.frames(vec![r#"{"candidates":[{"citationMetadata":{"citationSources":[{"uri":"https://example.test"}]}}]}"#.into()]).unwrap();
        let state = state.lock().unwrap();
        let EventKind::Structured { event } = &state.queue.front().unwrap().kind else {
            panic!("structured event expected")
        };
        assert!(
            event
                .event_json()
                .contains(r#""citations":[{"uri":"https://example.test"}]"#)
        );
    }

    struct CaptureUrl(Mutex<Option<String>>);
    impl HttpStreamTransport for CaptureUrl {
        fn execute(
            &self,
            request: &HttpStreamRequest,
            _: &HttpStreamControl,
            sink: &mut dyn HttpStreamSink,
        ) -> Result<(), String> {
            *self.0.lock().unwrap() = Some(request.url.clone());
            sink.head(HttpStreamHead {
                status: 400,
                headers: vec![],
            })?;
            sink.body(br#"{"error":{"message":"bad request"}}"#)
        }
    }
    #[test]
    fn google_endpoint_uses_authorized_request_model_not_config_id() {
        let script = Arc::new(Script {
            attempts: AtomicUsize::new(0),
            block: false,
            exited: AtomicBool::new(false),
        });
        let capture = Arc::new(CaptureUrl(Mutex::new(None)));
        let mut factory = factory(script);
        factory.config = parse_json(r#"{"providers":{"p":{"id":"p","protocol":"google","url":"http://localhost","models":{"m":{"id":"different-model-id","capabilities":{"supportsStreaming":true}}}}}}"#).unwrap();
        factory.transport = capture.clone();
        let session = block_on(factory.open(config())).ok().unwrap();
        let _ = event(&*session);
        let _ = run_attempt(&*session, "google-alias");
        let url = capture
            .0
            .lock()
            .unwrap()
            .clone()
            .expect("request reached transport");
        assert!(url.contains("/models/m:streamGenerateContent"), "{url}");
        assert!(!url.contains("different-model-id"));
    }

    #[test]
    fn validation_failure_before_transport_releases_without_claiming_success() {
        let script = Arc::new(Script {
            attempts: AtomicUsize::new(0),
            block: false,
            exited: AtomicBool::new(false),
        });
        let session = block_on(factory(script.clone()).open(config()))
            .ok()
            .unwrap();
        let _ = event(&*session);
        let mut input = snapshot(&*session, "validation", MODEL_REQUEST_PROFILE_V1);
        input.content =
            br#"{"provider":"p","model":"m","messages":[],"systemPrompt":"hello"}"#.to_vec();
        block_on(session.append_input(input)).unwrap();
        assert!(!inspect(&*session, "validation").requires_media_input);
        block_on(session.commit_input_scoped(
            InputId("validation".into()),
            scope(
                &*session,
                "validation-job",
                HttpStreamCancel::default(),
                None,
            ),
        ))
        .unwrap();
        loop {
            match event(&*session).kind {
                EventKind::ResponseFailed {
                    message,
                    resource_released,
                    ..
                } => {
                    assert!(message.contains("does not support system prompts"));
                    assert!(resource_released);
                    break;
                }
                EventKind::ResponseCompleted => panic!("validation error reported success"),
                _ => {}
            }
        }
        assert_eq!(script.attempts.load(Ordering::SeqCst), 0);
        block_on(session.close()).unwrap();
        assert!(matches!(
            event(&*session).kind,
            EventKind::SessionClosed {
                terminal: SessionTerminal::Closed
            }
        ));
        assert!(block_on(session.next_event()).unwrap().is_none());
    }

    struct Partial(AtomicUsize);
    impl HttpStreamTransport for Partial {
        fn execute(
            &self,
            _: &HttpStreamRequest,
            _: &HttpStreamControl,
            sink: &mut dyn HttpStreamSink,
        ) -> Result<(), String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            sink.head(HttpStreamHead {
                status: 200,
                headers: vec![],
            })?;
            sink.body(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n")?;
            Err("connection dropped after dispatch".into())
        }
    }
    #[test]
    fn partial_stream_failure_never_claims_completion_or_release() {
        let script = Arc::new(Script {
            attempts: AtomicUsize::new(0),
            block: false,
            exited: AtomicBool::new(false),
        });
        let mut factory = factory(script);
        let partial = Arc::new(Partial(AtomicUsize::new(0)));
        factory.transport = partial.clone();
        let session = block_on(factory.open(config())).ok().unwrap();
        let _ = event(&*session);
        let events = run_attempt(&*session, "partial");
        assert_eq!(partial.0.load(Ordering::SeqCst), 1);
        assert!(events.iter().any(|kind| matches!(kind, EventKind::Structured { event } if event.event_type() == ModelEventType::TextDelta)));
        assert!(matches!(
            events.last(),
            Some(EventKind::ResponseFailed {
                resource_released: false,
                ..
            })
        ));
        assert!(
            !events
                .iter()
                .any(|kind| matches!(kind, EventKind::ResponseCompleted))
        );
        block_on(session.close()).unwrap();
        assert!(matches!(
            event(&*session).kind,
            EventKind::SessionClosed {
                terminal: SessionTerminal::Unknown
            }
        ));
    }

    #[test]
    fn output_budget_stops_production_without_unbounded_queue() {
        let script = Arc::new(Script {
            attempts: AtomicUsize::new(1),
            block: false,
            exited: AtomicBool::new(false),
        });
        let mut cfg = config();
        cfg.limits.output_record_bytes = 1;
        let session = block_on(factory(script).open(cfg)).ok().unwrap();
        let _ = event(&*session);
        let result = run_attempt(&*session, "limited");
        assert!(
            matches!(result.last(), Some(EventKind::ResponseFailed { message, .. }) if message.contains("buffer limit"))
        );
        assert!(
            result
                .iter()
                .all(|kind| !matches!(kind, EventKind::Structured { .. }))
        );
    }

    #[test]
    fn abort_cancels_worker_and_preserves_unknown() {
        let script = Arc::new(Script {
            attempts: AtomicUsize::new(0),
            block: true,
            exited: AtomicBool::new(false),
        });
        let session = block_on(factory(script.clone()).open(config()))
            .ok()
            .unwrap();
        let _ = event(&*session);
        block_on(session.append_input(snapshot(&*session, "one", MODEL_REQUEST_PROFILE_V1)))
            .unwrap();
        assert!(!inspect(&*session, "one").requires_media_input);
        block_on(session.commit_input_scoped(
            InputId("one".into()),
            scope(&*session, "one-job", HttpStreamCancel::default(), None),
        ))
        .unwrap();
        let limit = Instant::now() + Duration::from_secs(2);
        while script.attempts.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < limit);
            std::thread::yield_now();
        }
        block_on(session.abort()).unwrap();
        block_on(session.abort()).unwrap();
        while !script.exited.load(Ordering::SeqCst) {
            assert!(Instant::now() < limit);
            std::thread::yield_now();
        }
        assert_eq!(session.state(), SessionState::Unknown);
        let mut closed = 0;
        while let Some(e) = block_on(session.next_event()).unwrap() {
            if matches!(
                e.kind,
                EventKind::SessionClosed {
                    terminal: SessionTerminal::Unknown
                }
            ) {
                closed += 1;
            }
            assert!(!matches!(e.kind, EventKind::ResponseCompleted));
        }
        assert_eq!(closed, 1);
    }
}

#[cfg(test)]
#[path = "shared_tests.rs"]
mod shared_tests;
