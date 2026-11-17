//! Model host calls mapped to the public D96 resource and model profile contracts.
use crate::model_session_events::{ModelEvents, ResponseEnd, SessionEnd};
use crate::resources::{ResourceCodec, ResourceRequest, ResourceStep};
use agentcpm_iface::model_session::{
    MODEL_CONSUMER_DEMAND_PROFILE_V1, MODEL_EVENT_PROFILE_V1, MODEL_INVOCATION_PROFILE_V1,
    MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1, MODEL_REQUEST_PROFILE_V1,
    REQUIRED_MODEL_CONSUMER_DEMAND_PROFILE, REQUIRED_MODEL_EVENT_PROFILE,
    REQUIRED_MODEL_INVOCATION_PROFILE, REQUIRED_MODEL_LOCAL_RESOURCE_FACTS_PROFILE,
    REQUIRED_MODEL_REQUEST_PROFILE,
};
use agentcpm_scheduler_protocol::{
    CallContext,
    model_session_resources::{EVENTS_READ, EventsPage, EventsReadRequest, MAX_BYTES},
};
use pilotdeck_agent::ports::*;
use pilotdeck_protocol::{JsObject, JsString, JsValue, model::BoundarySnapshot};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};

const MAX_DELIVERY_ADVANCE_RETRIES: usize = 150;
const DELIVERY_ADVANCE_RETRY_DELAY: Duration = Duration::from_millis(20);

/// One invocation-bound codec. It owns protocol cursors and prepared handles,
/// never a provider socket, Gateway, history store or authorization decision.
pub struct ModelSessionCodec {
    context: CallContext,
    next: u64,
    models: BTreeMap<JsString, Invocation>,
    pending: Option<Pending>,
    local_resource_facts: bool,
    consumer_demand: bool,
    invocation_context: bool,
    reconciliation_cause: Option<agentcpm_scheduler_protocol::Failure>,
}

struct Invocation {
    serial: u64,
    request: CapturedRequest,
    id: Option<String>,
    events: Option<ModelEvents>,
    revision: u64,
    inline_bytes: usize,
    input_bytes: usize,
    input_items: usize,
    body: Option<Vec<u8>>,
    appended: usize,
    chunk: u32,
    committed: bool,
    rejected_before_open: bool,
    closing: bool,
    aborted: bool,
    delivery_advance_retries: usize,
    interrupted_cancel_polls: usize,
}

impl Invocation {
    fn local_abort_released(aborted: bool, events: &ModelEvents) -> bool {
        (aborted || matches!(events.response_end, Some(ResponseEnd::Interrupted { .. })))
            && events.session_end == Some(SessionEnd::Unknown)
            && events.expects_local_release
            && events.local_release_seen
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Open,
    Ready,
    Append,
    Commit,
    Advance,
    /// A `ModelClose` rerouted into a delivery advance after a source error.
    /// The close still owns the outcome: it must never strand the caller on a
    /// rejected advance, and it settles through the explicit close fallback.
    CloseAdvance,
    Read,
    CloseAck,
    CloseRead,
}
struct Pending {
    handle: JsString,
    phase: Phase,
    buffered: Vec<StreamEvent>,
}

impl ModelSessionCodec {
    pub fn new(context: CallContext) -> Self {
        Self {
            context,
            next: 0,
            models: BTreeMap::new(),
            pending: None,
            local_resource_facts: false,
            consumer_demand: false,
            invocation_context: false,
            reconciliation_cause: None,
        }
    }

    /// Explicitly opt in only when the observer connection also negotiated
    /// the matching initialize feature and the backend supports the profile.
    pub fn new_with_local_resource_facts(context: CallContext) -> Self {
        let mut codec = Self::new(context);
        codec.local_resource_facts = true;
        codec
    }

    /// Opt into the closed-gate consumer-demand profile explicitly.
    pub fn with_consumer_demand(mut self) -> Self {
        self.consumer_demand = true;
        self
    }

    /// Opt in only when the Kernel Host and backend negotiate the invocation profile.
    pub fn with_invocation_context(mut self) -> Self {
        self.invocation_context = true;
        self
    }

    fn delivery_advance(&self, model: &Invocation) -> PortResult<ResourceStep> {
        let events = model
            .events
            .as_ref()
            .ok_or_else(|| invalid("model observer is not bound"))?;
        let job = events
            .job_id
            .as_deref()
            .ok_or_else(|| invalid("model commit omitted job id"))?;
        let previous = events.delivered_sequence.map(|v| v.to_string());
        Ok(Self::request(
            "model.delivery_advance",
            json!({
                "model_session_id": model.id, "job_id": job,
                "input_id": input_id(model.serial), "previous_event_seq": previous,
                "idempotency_key": self.key(model.serial, &format!("advance:{}", previous.as_deref().unwrap_or("initial"))),
            }),
        ))
    }

    fn key(&self, serial: u64, phase: &str) -> String {
        format!("{}:model:{serial}:{phase}", self.context.call_ref)
    }

    fn request(method: &'static str, payload: Value) -> ResourceStep {
        ResourceStep::Request(ResourceRequest { method, payload })
    }

    fn page(model: &Invocation, delay: bool) -> PortResult<ResourceStep> {
        let id = model
            .id
            .as_ref()
            .ok_or_else(|| invalid("model session has no accepted identity"))?;
        let mut request = EventsReadRequest::new(id);
        request.cursor = model
            .events
            .as_ref()
            .and_then(|events| events.cursor.clone());
        request.max_bytes = MAX_BYTES;
        let step = Self::request(
            EVENTS_READ,
            serde_json::to_value(request)
                .map_err(|_| invalid("model event request cannot be encoded"))?,
        );
        Ok(if delay {
            ResourceStep::Delay {
                duration: Duration::from_millis(20),
                next: Box::new(step),
            }
        } else {
            step
        })
    }

    fn append(&self, model: &Invocation) -> PortResult<ResourceStep> {
        let body = model
            .body
            .as_ref()
            .ok_or_else(|| invalid("model request was not prepared"))?;
        let end = model
            .appended
            .saturating_add(model.inline_bytes)
            .min(body.len());
        if end <= model.appended {
            return Err(invalid("model input has no remaining bytes"));
        }
        let content = body[model.appended..end]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Ok(Self::request(
            "model.input_append",
            json!({
                "model_session_id": model.id,
                "input_id": input_id(model.serial),
                "client_message_id": format!("pd-input-{}-{}", model.serial, model.chunk),
                "expected_config_revision": model.revision,
                "chunk_seq": model.chunk,
                "last_chunk": false,
                "kind": "request_snapshot",
                "content_type": if self.invocation_context { MODEL_INVOCATION_PROFILE_V1 } else { MODEL_REQUEST_PROFILE_V1 },
                "content_hex": content,
                "idempotency_key": self.key(model.serial, &format!("append:{}", model.chunk)),
            }),
        ))
    }

    fn close(&self, model: &Invocation, abort: bool) -> ResourceStep {
        Self::request(
            if abort {
                "model.session_abort"
            } else {
                "model.session_close"
            },
            json!({
                "model_session_id": model.id,
                "idempotency_key": self.key(model.serial, if abort { "abort" } else { "close" }),
            }),
        )
    }

    fn prepared(model: &Invocation, handle: &JsString) -> ResourceStep {
        let mut boundary = JsObject::new();
        boundary.insert("request".into(), model.request.wire.request().clone());
        boundary.insert(
            "provider".into(),
            JsValue::String(model.request.route.provider.clone()),
        );
        boundary.insert(
            "model".into(),
            JsValue::String(model.request.route.model.clone()),
        );
        ResourceStep::Reply(Box::new(HostReply::ModelPrepared(Box::new(
            PreparedInvocation {
                handle: PreparedRequestHandle(handle.clone()),
                request: model.request.clone(),
                route: model.request.route.clone(),
                max_context_tokens: None,
                max_output_tokens: None,
                metadata: None,
                boundary: BoundarySnapshot::capture(&JsValue::Object(boundary)),
            },
        ))))
    }

    fn unknown(&self) -> PortError {
        PortError::AcceptedUnknown {
            identity: Box::new(ExecutionIdentity {
                session_id: self.context.session_ref.as_str().into(),
                turn_id: self.context.interaction.turn.to_string().into(),
                run_id: self.context.call_ref.as_str().into(),
                operation_id: self.context.invocation.correlation_id.as_str().into(),
                request_id: self.context.call_ref.as_str().into(),
                idempotency_key: Some(self.context.invocation.idempotency_key.as_str().into()),
                module_instance_id: self.context.binding.id.as_str().into(),
                connection_generation: self.context.interaction.generation.to_string().into(),
            }),
            last_applied_sequence: None,
            cause: None,
        }
    }

    fn advance(
        &mut self,
        call: &HostCall,
        pending: &mut Pending,
        response: Value,
    ) -> PortResult<ResourceStep> {
        let mut model = self
            .models
            .remove(&pending.handle)
            .ok_or_else(|| invalid("prepared model handle disappeared"))?;
        let result = self.advance_model(call, pending, &mut model, response);
        self.models.insert(pending.handle.clone(), model);
        result
    }

    fn advance_model(
        &self,
        call: &HostCall,
        pending: &mut Pending,
        model: &mut Invocation,
        response: Value,
    ) -> PortResult<ResourceStep> {
        match pending.phase {
            Phase::Open => {
                let id = string(&response, "model_session_id")?.to_string();
                if id.is_empty() {
                    return Err(invalid("open returned an empty model session identity"));
                }
                // Save the known identity before validating the rest: failed
                // negotiation must never lose an already opened resource.
                model.id = Some(id.clone());
                model.events = Some(ModelEvents::new(
                    self.context.session_ref.clone(),
                    id.clone(),
                    input_id(model.serial),
                ));
                let profiles = response
                    .get("negotiated_profiles")
                    .and_then(Value::as_array)
                    .ok_or_else(|| invalid("host did not negotiate model profiles"))?;
                if ![MODEL_EVENT_PROFILE_V1, MODEL_REQUEST_PROFILE_V1]
                    .iter()
                    .all(|profile| profiles.iter().any(|value| value.as_str() == Some(profile)))
                {
                    return Err(invalid(
                        "model backend does not support the required public profiles",
                    ));
                }
                if self.invocation_context
                    && !profiles
                        .iter()
                        .any(|v| v.as_str() == Some(MODEL_INVOCATION_PROFILE_V1))
                {
                    return Err(invalid(
                        "model backend did not negotiate invocation context",
                    ));
                }
                if self.local_resource_facts
                    && !profiles
                        .iter()
                        .any(|value| value.as_str() == Some(MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1))
                {
                    return Err(invalid(
                        "model backend did not negotiate local resource facts",
                    ));
                }
                if self.consumer_demand
                    && !profiles
                        .iter()
                        .any(|v| v.as_str() == Some(MODEL_CONSUMER_DEMAND_PROFILE_V1))
                {
                    return Err(invalid("model backend did not negotiate consumer demand"));
                }
                let capabilities = response
                    .get("capabilities")
                    .ok_or_else(|| invalid("open omitted capabilities"))?;
                if capabilities.get("completion").and_then(Value::as_bool) != Some(true)
                    || capabilities
                        .get("streaming_output")
                        .and_then(Value::as_bool)
                        != Some(true)
                {
                    return Err(invalid("model backend does not support request streaming"));
                }
                model.revision = integer(&response, "effective_revision")?;
                let limits = response
                    .get("limits")
                    .ok_or_else(|| invalid("open omitted actual limits"))?;
                model.inline_bytes = positive_size(limits, "inline_block_bytes")?;
                model.input_bytes = positive_size(limits, "reliable_input_bytes")?;
                model.input_items = positive_size(limits, "reliable_input_items")?;
                let events = model.events.as_mut().expect("stored before negotiation");
                events.expects_local_release = self.local_resource_facts;
                events.demand_revision = self.consumer_demand.then_some(model.revision);
                match string(&response, "state")? {
                    "ready" => events.ready = true,
                    "opening" => {}
                    "unknown" => return Err(self.unknown()),
                    _ => return Err(invalid("model open did not enter opening or ready")),
                }
                if model.events.as_ref().is_some_and(|events| events.ready) {
                    Ok(Self::prepared(model, &pending.handle))
                } else {
                    pending.phase = Phase::Ready;
                    Self::page(model, false)
                }
            }
            Phase::Append => {
                check_model(&response, model)?;
                if string(&response, "input_id")? != input_id(model.serial)
                    || !matches!(string(&response, "status")?, "accepted" | "consumed")
                {
                    return Err(invalid("model append did not confirm the bound input"));
                }
                let body = model
                    .body
                    .as_ref()
                    .ok_or_else(|| invalid("model request disappeared"))?;
                model.appended = model
                    .appended
                    .saturating_add(model.inline_bytes)
                    .min(body.len());
                model.chunk = model
                    .chunk
                    .checked_add(1)
                    .ok_or_else(|| invalid("model request chunk sequence overflowed"))?;
                if model.appended < body.len() {
                    self.append(model)
                } else {
                    pending.phase = Phase::Commit;
                    Ok(Self::request(
                        "model.input_commit",
                        json!({
                            "model_session_id": model.id, "input_id": input_id(model.serial),
                            "idempotency_key": self.key(model.serial, "commit"),
                        }),
                    ))
                }
            }
            Phase::Commit => {
                check_model(&response, model)?;
                if string(&response, "input_id")? != input_id(model.serial)
                    || response.get("admitted").and_then(Value::as_bool) != Some(true)
                {
                    return Err(invalid(
                        "model commit did not confirm admission of the bound input",
                    ));
                }
                model.committed = true;
                if self.local_resource_facts || self.consumer_demand {
                    let job = string(&response, "job_id")?;
                    if job.is_empty() {
                        return Err(invalid("model commit returned an empty job id"));
                    }
                    model
                        .events
                        .as_mut()
                        .ok_or_else(|| invalid("model observer is not bound"))?
                        .job_id = Some(job.to_owned());
                }
                if self.consumer_demand {
                    pending.phase = Phase::Advance;
                    self.delivery_advance(model)
                } else {
                    pending.phase = Phase::Read;
                    Self::page(model, false)
                }
            }
            Phase::Advance | Phase::CloseAdvance => {
                check_model(&response, model)?;
                let events = model
                    .events
                    .as_ref()
                    .ok_or_else(|| invalid("model observer is not bound"))?;
                let previous = events.delivered_sequence.map(|v| v.to_string());
                if response.get("job_id").and_then(Value::as_str) != events.job_id.as_deref()
                    || response.get("input_id").and_then(Value::as_str)
                        != Some(input_id(model.serial).as_str())
                    || response.get("previous_event_seq")
                        != Some(&previous.map(Value::String).unwrap_or(Value::Null))
                {
                    return Err(invalid(
                        "delivery ack changed the original generation or sequence",
                    ));
                }
                // Both advances only drain the held yield into the read loop;
                // the observed response terminal then drives the explicit
                // close transition (Read -> CloseAck), whatever the caller is.
                pending.phase = Phase::Read;
                Self::page(model, false)
            }
            Phase::CloseAck => {
                check_model(&response, model)?;
                // ACK and registry summaries are not terminal event evidence.
                pending.phase = Phase::CloseRead;
                Self::page(model, false)
            }
            Phase::Ready | Phase::Read | Phase::CloseRead => {
                let page: EventsPage = serde_json::from_value(response)
                    .map_err(|_| invalid("model event page has the wrong public schema"))?;
                let has_more = page.has_more;
                let model_aborted = model.aborted;
                let interrupted_polls = model.interrupted_cancel_polls;
                let events = model
                    .events
                    .as_mut()
                    .ok_or_else(|| invalid("model event observer is not bound"))?;
                let source = events.apply(page)?;
                if matches!(pending.phase, Phase::Ready) {
                    if !source.is_empty() || events.response_end.is_some() {
                        return Err(invalid("model emitted a response before input commit"));
                    }
                    if events.ready {
                        return Ok(Self::prepared(model, &pending.handle));
                    }
                }
                pending.buffered.extend(source);
                // A later close/transport failure must not erase provider data
                // already accepted in this page. Let the owning source reducer
                // consume that prefix before reporting final reconciliation.
                if matches!(call, HostCall::ModelPull { .. })
                    && !pending.buffered.is_empty()
                    && events.session_end != Some(SessionEnd::Closed)
                {
                    return Ok(ResourceStep::Reply(Box::new(HostReply::ModelChunk(
                        StreamChunk {
                            events: std::mem::take(&mut pending.buffered),
                            done: false,
                        },
                    ))));
                }
                match &events.session_end {
                    Some(SessionEnd::Unknown) => {
                        if has_more {
                            return Self::page(model, false);
                        }
                        if matches!(call, HostCall::ModelClose { .. })
                            && Invocation::local_abort_released(model_aborted, events)
                        {
                            return Ok(ResourceStep::Reply(Box::new(HostReply::ModelClosed)));
                        }
                        // The explicit close was acknowledged and the response
                        // provably ended after its source error was already
                        // delivered: no output remains to reconcile, so an
                        // unknown registry terminal cannot extend the close.
                        if matches!(call, HostCall::ModelClose { .. })
                            && matches!(pending.phase, Phase::CloseRead)
                            && events.source_error_seen
                            && events.response_end.is_some()
                        {
                            return Ok(ResourceStep::Reply(Box::new(HostReply::ModelClosed)));
                        }
                        let wait_interrupt_cancel = matches!(call, HostCall::ModelPull { .. })
                            && matches!(events.response_end, Some(ResponseEnd::Interrupted { .. }))
                            && events.expects_local_release
                            && events.local_release_seen
                            && interrupted_polls < 50;
                        if wait_interrupt_cancel {
                            model.interrupted_cancel_polls += 1;
                            return Self::page(model, true);
                        }
                        return Err(self.unknown());
                    }
                    Some(SessionEnd::Failed { released: false }) => return Err(self.unknown()),
                    Some(SessionEnd::Failed { released: true }) => {
                        return Err(known_failure(
                            "MODEL_SESSION_FAILED",
                            "model session failed and released its resources",
                        ));
                    }
                    Some(SessionEnd::Closed) => {
                        if has_more {
                            return Self::page(model, false);
                        }
                        if matches!(call, HostCall::ModelClose { .. }) {
                            return Ok(ResourceStep::Reply(Box::new(HostReply::ModelClosed)));
                        }
                        if !model.committed || events.response_end.is_none() {
                            return Err(self.unknown());
                        }
                        if matches!(events.response_end, Some(ResponseEnd::Interrupted { .. })) {
                            if matches!(call, HostCall::ModelPull { .. }) && interrupted_polls < 50
                            {
                                model.interrupted_cancel_polls += 1;
                                return Self::page(model, true);
                            }
                            return Err(self.unknown());
                        }
                        if let Some(ResponseEnd::Failed { message, .. }) = &events.response_end {
                            // A source error event is delivered exactly as received.
                            // If none exists, the transport failure is still known;
                            // never fabricate a provider error or successful answer.
                            if !events.source_error_seen {
                                return Err(known_failure("MODEL_RESPONSE_FAILED", message));
                            }
                        }
                        return Ok(ResourceStep::Reply(Box::new(HostReply::ModelChunk(
                            StreamChunk {
                                events: std::mem::take(&mut pending.buffered),
                                done: true,
                            },
                        ))));
                    }
                    None => {}
                }
                if events.subscription_closed && !has_more {
                    return Err(self.unknown());
                }
                let wait_interrupt_cancel = matches!(pending.phase, Phase::Read)
                    && matches!(call, HostCall::ModelPull { .. })
                    && matches!(events.response_end, Some(ResponseEnd::Interrupted { .. }))
                    && !has_more
                    && interrupted_polls < 50;
                if wait_interrupt_cancel {
                    model.interrupted_cancel_polls += 1;
                    return Self::page(model, true);
                }
                if matches!(pending.phase, Phase::Read)
                    && events.response_end.is_some()
                    && !has_more
                {
                    model.closing = true;
                    pending.phase = Phase::CloseAck;
                    return Ok(self.close(model, false));
                }
                if !pending.buffered.is_empty() {
                    if matches!(call, HostCall::ModelPull { .. })
                        && matches!(pending.phase, Phase::Read)
                    {
                        return Ok(ResourceStep::Reply(Box::new(HostReply::ModelChunk(
                            StreamChunk {
                                events: std::mem::take(&mut pending.buffered),
                                done: false,
                            },
                        ))));
                    }
                    if matches!(call, HostCall::ModelClose { .. })
                        && matches!(pending.phase, Phase::CloseRead)
                    {
                        pending.buffered.clear();
                    } else {
                        return Err(self.unknown());
                    }
                }
                Self::page(model, !has_more)
            }
        }
    }
}

impl ResourceCodec for ModelSessionCodec {
    fn retry_rejected(
        &mut self,
        method: &str,
        failure: &agentcpm_scheduler_protocol::Failure,
    ) -> Option<ResourceStep> {
        if method != "model.delivery_advance" || failure.code != "MODEL_SESSION_BAD_STATE" {
            return None;
        }
        let handle = {
            let pending = self.pending.as_ref()?;
            if !matches!(pending.phase, Phase::Advance | Phase::CloseAdvance) {
                return None;
            }
            pending.handle.clone()
        };
        let wire = failure.wire();
        // A close-rerouted advance acknowledges an already delivered yield.
        // Once the kernel proves that yield's generation is gone, no retry can
        // ever succeed: fall back once to the explicit close the caller asked
        // for, so the cleanup never strands on a rejected advance.
        if matches!(
            self.pending.as_ref().map(|pending| pending.phase),
            Some(Phase::CloseAdvance)
        ) && close_advance_generation_gone(&wire)
        {
            let (abort, session_id, serial) = {
                let model = self.models.get_mut(&handle)?;
                let abort = model.events.as_ref().is_none_or(|events| {
                    events.response_end.is_none()
                        || matches!(events.response_end, Some(ResponseEnd::Interrupted { .. }))
                });
                model.closing = true;
                if abort {
                    model.aborted = true;
                }
                (abort, model.id.clone(), model.serial)
            };
            let step = Self::request(
                if abort {
                    "model.session_abort"
                } else {
                    "model.session_close"
                },
                json!({
                    "model_session_id": session_id,
                    "idempotency_key": self.key(serial, if abort { "abort" } else { "close" }),
                }),
            );
            if let Some(pending) = self.pending.as_mut() {
                pending.phase = Phase::CloseAck;
            }
            return Some(step);
        }
        if !wire.contains("commit handshake is not ready") && !wire.contains("Busy") {
            return None;
        }
        {
            let model = self.models.get_mut(&handle)?;
            if !model.committed
                || model
                    .events
                    .as_ref()
                    .is_some_and(|events| events.session_end.is_some())
                || model.delivery_advance_retries >= MAX_DELIVERY_ADVANCE_RETRIES
            {
                return None;
            }
            model.delivery_advance_retries += 1;
        }
        let model = self.models.get(&handle)?;
        let next = self.delivery_advance(model).ok()?;
        Some(ResourceStep::Delay {
            duration: DELIVERY_ADVANCE_RETRY_DELAY,
            next: Box::new(next),
        })
    }
    fn reconcile_failure(
        &mut self,
        call: &HostCall,
        method: &str,
        failure: &agentcpm_scheduler_protocol::Failure,
        _error: &PortError,
        _prior_effect: bool,
    ) -> Option<(ResourceRequest, PortError)> {
        if method != "model.delivery_advance" || !matches!(call, HostCall::ModelPull { .. }) {
            return None;
        }
        let pending = self.pending.as_ref()?;
        if !matches!(pending.phase, Phase::Advance) {
            return None;
        }
        let model = self.models.get(&pending.handle)?;
        if !model.committed {
            return None;
        }
        let ResourceStep::Request(request) = Self::page(model, false).ok()? else {
            return None;
        };
        self.reconciliation_cause = Some(failure.clone());
        let mut unknown = self.unknown();
        if let PortError::AcceptedUnknown { cause, .. } = &mut unknown {
            *cause = Some(Box::new(pilotdeck_protocol::model::HostDiagnosticCause {
                code: failure.code.clone(),
                message_key: failure.message_key.clone(),
                params: failure.params.clone(),
            }));
        }
        Some((request, unknown))
    }
    fn reconciliation_observed(
        &mut self,
        _call: &HostCall,
        result: Result<Value, agentcpm_scheduler_protocol::Failure>,
    ) {
        let Some(pending) = self.pending.as_ref() else {
            return;
        };
        if !matches!(pending.phase, Phase::Advance) {
            return;
        }
        let Some(model) = self.models.get_mut(&pending.handle) else {
            return;
        };
        let Ok(page) = result.and_then(|v| {
            serde_json::from_value::<EventsPage>(v).map_err(|_| {
                agentcpm_scheduler_protocol::Failure::new(
                    "RECONCILE_SCHEMA",
                    "model.reconciliation.schema",
                )
            })
        }) else {
            return;
        };
        // Stage the observation. Never consume an unreturned source event or
        // advance a cursor on a malformed page, gap, or incomplete evidence.
        if let Some(events) = model.events.as_mut() {
            let mut observed = events.clone();
            if let Ok(source) = observed.apply(page)
                && source.is_empty()
                && observed.response_end.is_some()
                && (!observed.expects_local_release || observed.local_release_seen)
            {
                *events = observed;
            }
        }
    }
    fn reconciliation_cause(&self) -> Option<&agentcpm_scheduler_protocol::Failure> {
        self.reconciliation_cause.as_ref()
    }
    fn resource_failed(&mut self, call: &HostCall, error: &PortError) {
        if matches!(call, HostCall::ModelPull { .. })
            && matches!(error, PortError::Cancelled { .. })
            && self
                .pending
                .as_ref()
                .is_some_and(|pending| matches!(pending.phase, Phase::Advance | Phase::Read))
        {
            self.pending = None;
            return;
        }
        // A failed close is spent: its model is already marked closing, so a
        // later close reconstructs its own read from the observed cursor. A
        // stale pending must never block the next call (e.g. the recovery's
        // ModelPrepare after a source error).
        if matches!(call, HostCall::ModelClose { .. }) && self.pending.is_some() {
            self.pending = None;
            return;
        }
        if let Some(pending) = self.pending.as_ref()
            && matches!(pending.phase, Phase::Open)
            && let PortError::HostFailure { code, .. } = error
            && [
                "CAPABILITY_DENIED",
                "POLICY_DENIED",
                "DELEGATION_DENIED",
                "SESSION_FORBIDDEN",
                "AUTHORITY_REVOKED",
                "CAPABILITY_PARAMS",
            ]
            .iter()
            .any(|known| code.equals_str(known))
            && let Some(model) = self.models.get_mut(&pending.handle)
            && model.id.is_none()
        {
            model.rejected_before_open = true;
        }
    }
    fn cleanup_calls(&self) -> Vec<HostCall> {
        self.models
            .iter()
            .filter(|(_, model)| {
                !model.rejected_before_open
                    && !model.events.as_ref().is_some_and(|events| {
                        matches!(
                            events.session_end,
                            Some(SessionEnd::Closed | SessionEnd::Failed { released: true })
                        ) || Invocation::local_abort_released(model.aborted, events)
                    })
            })
            .map(|(handle, _)| HostCall::ModelClose {
                handle: PreparedRequestHandle(handle.clone()),
            })
            .collect()
    }
    fn advertised(&self, module: &str, operation: &str) -> bool {
        module == "model" && matches!(operation, "prepare" | "stream_next" | "close_stream")
    }
    fn unresolved_effect(
        &self,
        call: &HostCall,
        dispatched: Option<&'static str>,
    ) -> Option<&'static str> {
        let HostCall::ModelClose { handle } = call else {
            return dispatched;
        };
        match self.pending.as_ref().map(|pending| pending.phase) {
            // The explicit close or abort request is still in flight: its
            // effect remains unresolved, so an interrupted sequence stays
            // unknown.
            Some(Phase::CloseAck) => {
                let aborted = self
                    .models
                    .get(&handle.0)
                    .is_some_and(|model| model.aborted);
                Some(if aborted {
                    "model.session_abort"
                } else {
                    "model.session_close"
                })
            }
            // The close was acknowledged, or only a read-only rerouted
            // advance is outstanding: the trailing reads confirm state the
            // kernel already settled, so an interrupted tail cannot turn the
            // whole close into an unknown outcome.
            Some(Phase::CloseRead) | Some(Phase::CloseAdvance) | None => None,
            _ => dispatched,
        }
    }
    fn resource_effect(
        &self,
        call: &HostCall,
        method: &str,
    ) -> pilotdeck_protocol::admission::CallEffect {
        if method == EVENTS_READ {
            return pilotdeck_protocol::admission::CallEffect::ReadOnly;
        }
        // A delivery advance only hands off an already delivered yield of a
        // committed input, whether a pull or the close after a source error
        // reroutes into it. It grants no new generation or write credit.
        if let HostCall::ModelPull { handle, .. } | HostCall::ModelClose { handle } = call
            && method == "model.delivery_advance"
            && self.models.get(&handle.0).is_some_and(|model| {
                model.committed
                    && model
                        .events
                        .as_ref()
                        .is_some_and(|events| events.delivered_sequence.is_some())
            })
        {
            return pilotdeck_protocol::admission::CallEffect::ReadOnly;
        }
        call.effect()
    }
    fn begin(&mut self, call: &HostCall) -> PortResult<ResourceStep> {
        if self.pending.is_some() && !matches!(call, HostCall::ModelClose { .. }) {
            return Err(self.unknown());
        }
        match call {
            HostCall::ModelPrepare {
                handle, request, ..
            } => {
                check_request_route(request)?;
                if self.models.contains_key(&handle.0) {
                    return Err(invalid("model preparation handle was reused"));
                }
                let provider = request
                    .route
                    .provider
                    .to_utf8()
                    .map_err(|_| invalid("provider route is not a Kernel UTF-8 identifier"))?;
                let model = request
                    .route
                    .model
                    .to_utf8()
                    .map_err(|_| invalid("model route is not a Kernel UTF-8 identifier"))?;
                self.next = self
                    .next
                    .checked_add(1)
                    .ok_or_else(|| invalid("model invocation sequence overflowed"))?;
                let serial = self.next;
                let mut extensions = json!({ REQUIRED_MODEL_EVENT_PROFILE: MODEL_EVENT_PROFILE_V1, REQUIRED_MODEL_REQUEST_PROFILE: MODEL_REQUEST_PROFILE_V1 });
                if self.invocation_context {
                    extensions.as_object_mut().expect("object").insert(
                        REQUIRED_MODEL_INVOCATION_PROFILE.into(),
                        Value::String(MODEL_INVOCATION_PROFILE_V1.into()),
                    );
                }
                if self.local_resource_facts {
                    extensions.as_object_mut().expect("object").insert(
                        REQUIRED_MODEL_LOCAL_RESOURCE_FACTS_PROFILE.into(),
                        Value::String(MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1.into()),
                    );
                }
                if self.consumer_demand {
                    extensions.as_object_mut().expect("object").insert(
                        REQUIRED_MODEL_CONSUMER_DEMAND_PROFILE.into(),
                        Value::String(MODEL_CONSUMER_DEMAND_PROFILE_V1.into()),
                    );
                }
                let result = Self::request(
                    "model.session_open",
                    json!({
                        "mode": "request", "model": model, "endpoint_ref": provider,
                        "extensions": extensions,
                        "idempotency_key": self.key(serial, "open"),
                    }),
                );
                self.models.insert(
                    handle.0.clone(),
                    Invocation {
                        serial,
                        request: *request.clone(),
                        id: None,
                        events: None,
                        revision: 0,
                        inline_bytes: 0,
                        input_bytes: 0,
                        input_items: 0,
                        body: None,
                        appended: 0,
                        chunk: 0,
                        committed: false,
                        rejected_before_open: false,
                        closing: false,
                        aborted: false,
                        delivery_advance_retries: 0,
                        interrupted_cancel_polls: 0,
                    },
                );
                self.pending = Some(Pending {
                    handle: handle.0.clone(),
                    phase: Phase::Open,
                    buffered: vec![],
                });
                Ok(result)
            }
            HostCall::ModelPull {
                handle,
                request,
                hints,
            } => {
                check_request_route(request)?;
                let mut model = self
                    .models
                    .remove(&handle.0)
                    .ok_or_else(|| invalid("model pull has no prepared handle"))?;
                model.delivery_advance_retries = 0;
                model.interrupted_cancel_polls = 0;
                let result = (|| {
                    if request.route != model.request.route {
                        return Err(invalid("model pull changed its prepared route"));
                    }
                    let body = if self.invocation_context {
                        let hints = pilotdeck_transport::model_hints(hints);
                        pilotdeck_protocol::model_invocation::encode(request.wire.request(), &hints)
                            .map_err(|_| invalid("model invocation cannot be encoded"))?
                    } else {
                        pilotdeck_protocol::stringify_json(request.wire.request())
                            .map_err(|_| invalid("model request cannot be encoded"))?
                            .ok_or_else(|| invalid("model request is not JSON"))?
                            .into_bytes()
                    };
                    if model
                        .body
                        .as_ref()
                        .is_some_and(|previous| previous != &body)
                    {
                        return Err(invalid("model pull changed the already committed request"));
                    }
                    let phase = if model.closing {
                        Phase::CloseRead
                    } else if model.committed
                        && self.consumer_demand
                        && model
                            .events
                            .as_ref()
                            .is_some_and(|e| e.response_end.is_none())
                    {
                        Phase::Advance
                    } else if model.committed {
                        Phase::Read
                    } else {
                        Phase::Append
                    };
                    let step = if model.closing {
                        Self::page(&model, false)?
                    } else if model.committed
                        && self.consumer_demand
                        && model
                            .events
                            .as_ref()
                            .is_some_and(|e| e.response_end.is_none())
                    {
                        self.delivery_advance(&model)?
                    } else if model.committed {
                        Self::page(&model, false)?
                    } else {
                        if model.body.is_some() {
                            return Err(self.unknown());
                        }
                        if model.inline_bytes == 0
                            || body.len() > model.input_bytes
                            || body.len().div_ceil(model.inline_bytes) > model.input_items
                        {
                            return Err(known_failure(
                                "MODEL_INPUT_LIMIT",
                                "model request exceeds the actual negotiated input limits",
                            ));
                        }
                        model.body = Some(body);
                        self.append(&model)?
                    };
                    self.pending = Some(Pending {
                        handle: handle.0.clone(),
                        phase,
                        buffered: vec![],
                    });
                    Ok(step)
                })();
                self.models.insert(handle.0.clone(), model);
                result
            }
            HostCall::ModelClose { handle } => {
                let model = self
                    .models
                    .get_mut(&handle.0)
                    .ok_or_else(|| invalid("model close has no prepared handle"))?;
                if model.id.is_none() {
                    return Err(self.unknown());
                }
                if model.events.as_ref().is_some_and(|events| {
                    events.session_end == Some(SessionEnd::Closed)
                        || Invocation::local_abort_released(model.aborted, events)
                }) {
                    self.pending = None;
                    return Ok(ResourceStep::Reply(Box::new(HostReply::ModelClosed)));
                }
                if model.closing {
                    let step = Self::page(model, false)?;
                    self.pending = Some(Pending {
                        handle: handle.0.clone(),
                        phase: Phase::CloseRead,
                        buffered: vec![],
                    });
                    return Ok(step);
                }
                let advance_after_source_error = model.committed
                    && self.consumer_demand
                    && self.reconciliation_cause.is_none()
                    && model.events.as_ref().is_some_and(|events| {
                        events.source_error_seen
                            && events.response_end.is_none()
                            && events.delivered_sequence.is_some()
                    });
                if advance_after_source_error {
                    model.delivery_advance_retries = 0;
                    // The close owns this advance: mark the model closing so a
                    // later close never re-issues the rerouted advance, and use
                    // a distinct phase so its rejection can fall back.
                    model.closing = true;
                    let model_ref = self
                        .models
                        .get(&handle.0)
                        .ok_or_else(|| invalid("model close has no prepared handle"))?;
                    let step = self.delivery_advance(model_ref)?;
                    self.pending = Some(Pending {
                        handle: handle.0.clone(),
                        phase: Phase::CloseAdvance,
                        buffered: vec![],
                    });
                    return Ok(step);
                }
                let abort = model.events.as_ref().is_none_or(|events| {
                    events.response_end.is_none()
                        || matches!(events.response_end, Some(ResponseEnd::Interrupted { .. }))
                });
                let serial = model.serial;
                let id = model.id.clone();
                model.closing = true;
                if abort {
                    model.aborted = true;
                }
                let step = Self::request(
                    if abort {
                        "model.session_abort"
                    } else {
                        "model.session_close"
                    },
                    json!({
                        "model_session_id": id,
                        "idempotency_key": self.key(serial, if abort { "abort" } else { "close" }),
                    }),
                );
                self.pending = Some(Pending {
                    handle: handle.0.clone(),
                    phase: Phase::CloseAck,
                    buffered: vec![],
                });
                Ok(step)
            }
            _ => {
                let (module, operation) = call.method();
                Err(PortError::NotAdvertised { module, operation })
            }
        }
    }

    fn resume(&mut self, call: &HostCall, response: Value) -> PortResult<ResourceStep> {
        let mut pending = self
            .pending
            .take()
            .ok_or_else(|| invalid("model resource reply has no pending operation"))?;
        let result = self.advance(call, &mut pending, response);
        // A failed close is one-shot: its model keeps the closing mark and a
        // later close rebuilds its own read, so a stale pending must not
        // outlive the failed call. Successful continuations keep their step.
        if !matches!(&result, Ok(ResourceStep::Reply(_)))
            && !(result.is_err() && matches!(call, HostCall::ModelClose { .. }))
        {
            self.pending = Some(pending);
        }
        result
    }
}

fn input_id(serial: u64) -> String {
    format!("pd-input-{serial}")
}
/// The kernel proved the close-rerouted advance can never be acknowledged: the
/// generation that owed the outstanding yield is already gone, so only the
/// explicit close remains. Matched on the public failure wire text.
fn close_advance_generation_gone(wire: &str) -> bool {
    wire.contains("generation is no longer active")
        || wire.contains("consumer advance does not identify the outstanding yield")
        || wire.contains("consumer demand is not active")
}
fn invalid(message: &str) -> PortError {
    PortError::InvalidResponse {
        message: message.into(),
    }
}
fn known_failure(code: &str, message: &str) -> PortError {
    PortError::HostFailure {
        code: code.into(),
        message: message.into(),
        boundary: BoundarySnapshot::capture(&JsValue::String(message.into())),
    }
}
fn string<'a>(value: &'a Value, key: &str) -> PortResult<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("model resource omitted a required string field"))
}
fn integer(value: &Value, key: &str) -> PortResult<u64> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("model resource omitted a required unsigned integer"))
}
fn positive_size(value: &Value, key: &str) -> PortResult<usize> {
    usize::try_from(integer(value, key)?)
        .ok()
        .filter(|size| *size > 0)
        .ok_or_else(|| invalid("model resource reported an unusable size limit"))
}
fn check_model(value: &Value, model: &Invocation) -> PortResult<()> {
    if Some(string(value, "model_session_id")?) != model.id.as_deref() {
        return Err(invalid("model resource changed its model session identity"));
    }
    Ok(())
}

fn check_request_route(request: &CapturedRequest) -> PortResult<()> {
    let raw = request.wire.request();
    if raw.get("provider").and_then(JsValue::as_string) != Some(&request.route.provider)
        || raw.get("model").and_then(JsValue::as_string) != Some(&request.route.model)
    {
        return Err(invalid(
            "canonical model request disagrees with its prepared route",
        ));
    }
    Ok(())
}
