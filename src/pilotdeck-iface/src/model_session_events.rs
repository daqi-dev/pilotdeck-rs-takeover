//! Strict observation of the public D96 event page and structured model profile.
//! This module preserves source events; it never invents a provider completion.
use agentcpm_iface::model_session::{MODEL_EVENT_PROFILE_V1, ModelEventType};
use agentcpm_scheduler_protocol::model_session_resources::EventsPage;
use pilotdeck_agent::ports::{PortError, PortResult, StreamEvent};
use pilotdeck_protocol::{JsValue, model::BoundarySnapshot};
use serde_json::Value;
mod projection;
use projection::validate_projection;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ResponseEnd {
    Completed,
    Failed {
        code: i32,
        message: String,
    },
    Interrupted {
        delivery_stopped: bool,
        backend_stop: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SessionEnd {
    Closed,
    Failed { released: bool },
    Unknown,
}

pub(crate) struct ModelEvents {
    agent_session: String,
    model_session: String,
    input_id: String,
    response_id: Option<String>,
    response_started: bool,
    sequence: u64,
    pub(crate) cursor: Option<String>,
    pub(crate) ready: bool,
    pub(crate) response_end: Option<ResponseEnd>,
    pub(crate) session_end: Option<SessionEnd>,
    pub(crate) subscription_closed: bool,
    pub(crate) source_error_seen: bool,
    pub(crate) expects_local_release: bool,
    pub(crate) job_id: Option<String>,
    pub(crate) local_release_seen: bool,
}

impl ModelEvents {
    pub(crate) fn new(agent_session: String, model_session: String, input_id: String) -> Self {
        Self {
            agent_session,
            model_session,
            input_id,
            response_id: None,
            response_started: false,
            sequence: 0,
            cursor: None,
            ready: false,
            response_end: None,
            session_end: None,
            subscription_closed: false,
            source_error_seen: false,
            expects_local_release: false,
            job_id: None,
            local_release_seen: false,
        }
    }

    pub(crate) fn apply(&mut self, page: EventsPage) -> PortResult<Vec<StreamEvent>> {
        if page.session_id != self.agent_session || page.model_session_id != self.model_session {
            return Err(invalid("model event page changed its bound sessions"));
        }
        if page.cursor.is_empty() || page.has_more && page.events.is_empty() {
            return Err(invalid(
                "model event page has an invalid pagination receipt",
            ));
        }
        let mut source = Vec::new();
        for event in page.events {
            if text(&event, "type")? != "model_session.event"
                || text(&event, "session_id")? != self.agent_session
                || text(&event, "model_session_id")? != self.model_session
            {
                return Err(invalid(
                    "model event changed its bound sessions or envelope",
                ));
            }
            let encoded_sequence = text(&event, "event_seq")?;
            let sequence = encoded_sequence
                .parse::<u64>()
                .map_err(|_| invalid("model event sequence is not a decimal u64 string"))?;
            if encoded_sequence != sequence.to_string()
                || self.sequence.checked_add(1) != Some(sequence)
            {
                return Err(invalid(
                    "model event sequence repeated, skipped or overflowed",
                ));
            }
            if let Some(input) = event.get("input_id")
                && input.as_str() != Some(self.input_id.as_str())
            {
                return Err(invalid("model event belongs to a different input"));
            }
            if let Some(response) = event.get("response_id") {
                let response = response
                    .as_str()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| invalid("model event response identity is invalid"))?;
                if self
                    .response_id
                    .as_deref()
                    .is_some_and(|known| known != response)
                {
                    return Err(invalid("model event belongs to a different response"));
                }
                self.response_id.get_or_insert_with(|| response.to_string());
            }
            let kind = text(&event, "kind")?;
            if matches!(
                kind,
                "structured"
                    | "response_started"
                    | "response_completed"
                    | "response_failed"
                    | "response_interrupted"
            ) && (event.get("input_id").and_then(Value::as_str) != Some(self.input_id.as_str())
                || event
                    .get("response_id")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty))
            {
                return Err(invalid(
                    "model response event lacks its admitted input and response identity",
                ));
            }
            match kind {
                "session_ready" => self.ready = true,
                "structured" => {
                    if text(&event, "profile")? != MODEL_EVENT_PROFILE_V1 {
                        return Err(invalid("model event profile was not negotiated"));
                    }
                    let raw = text(&event, "event_json")?;
                    let boundary = pilotdeck_protocol::parse_json(raw)
                        .map_err(|_| invalid("structured model event is not source JSON"))?;
                    let declared = text(&event, "event_type")?;
                    let actual = boundary
                        .get("type")
                        .and_then(JsValue::as_string)
                        .ok_or_else(|| invalid("structured model event lacks its source type"))?;
                    if declared != "other" && !actual.equals_str(declared) {
                        return Err(invalid(
                            "structured model event disagrees with its declared type",
                        ));
                    }
                    if declared == "other"
                        && actual
                            .to_utf8()
                            .ok()
                            .as_deref()
                            .and_then(ModelEventType::parse)
                            .is_some_and(|kind| kind != ModelEventType::Other)
                    {
                        return Err(invalid(
                            "known source event was disguised as an unprojected extension",
                        ));
                    }
                    validate_projection(&boundary, &event, declared)?;
                    self.source_error_seen |= actual.equals_str("error");
                    self.source_error_seen |= actual.equals_str("message_end")
                        && boundary
                            .get("finishReason")
                            .and_then(JsValue::as_string)
                            .is_some_and(|reason| reason.equals_str("error"));
                    // The source reducer is authoritative for open fields and
                    // coercions. Avoid a normalized re-encoding of raw events.
                    source.push(StreamEvent::BoundaryOnly(BoundarySnapshot::capture(
                        &boundary,
                    )));
                }
                "local_resources_released" => {
                    if (!self.response_started && event.get("response_id").is_some())
                        || event.get("end_kind").is_some()
                        || !self.expects_local_release
                        || event.get("input_id").and_then(Value::as_str)
                            != Some(self.input_id.as_str())
                        || self.job_id.as_deref().is_none_or(|job| {
                            event.get("job_id").and_then(Value::as_str) != Some(job)
                        })
                    {
                        return Err(invalid(
                            "local release is not bound to the negotiated generation",
                        ));
                    }
                    if self.response_id.is_some()
                        && event.get("response_id").and_then(Value::as_str)
                            != self.response_id.as_deref()
                    {
                        return Err(invalid("local release changed the response identity"));
                    }
                    self.local_release_seen = true;
                }
                "response_started" => {
                    self.response_started = true;
                    if self.response_end.is_some() {
                        return Err(invalid("response restarted after its terminal event"));
                    }
                }
                "response_completed" => {
                    if self.source_error_seen {
                        return Err(invalid(
                            "failed source response was classified as completed",
                        ));
                    }
                    self.finish_response(ResponseEnd::Completed)?;
                }
                "response_failed" => {
                    self.response_started = true;
                    let code = event
                        .get("code")
                        .and_then(Value::as_i64)
                        .and_then(|value| i32::try_from(value).ok())
                        .ok_or_else(|| invalid("response failure lacks its integer code"))?;
                    self.finish_response(ResponseEnd::Failed {
                        code,
                        message: text(&event, "message")?.to_string(),
                    })?;
                }
                "response_interrupted" => {
                    self.response_started = true;
                    let backend_stop = text(&event, "backend_stop")?;
                    if !matches!(backend_stop, "stopped" | "unsupported" | "unknown") {
                        return Err(invalid("response interrupt has an unknown stop state"));
                    }
                    self.finish_response(ResponseEnd::Interrupted {
                        delivery_stopped: boolean(&event, "delivery_stopped")?,
                        backend_stop: backend_stop.to_string(),
                    })?;
                }
                "session_closed" => {
                    let terminal = match text(&event, "terminal")? {
                        "closed" => SessionEnd::Closed,
                        "failed" => SessionEnd::Failed { released: false },
                        "unknown" => SessionEnd::Unknown,
                        _ => {
                            return Err(invalid("model session reported an unrecognized terminal"));
                        }
                    };
                    self.finish_session(terminal)?;
                }
                "session_failed" => self.finish_session(SessionEnd::Failed {
                    released: boolean(&event, "resource_released")?,
                })?,
                "subscription_closed" => self.subscription_closed = true,
                // Input/config facts are not provider events. Their schema is
                // owned by D96 and they cannot be turned into source text.
                "input_accepted" | "input_forwarded" | "input_consumed" | "config_applied" => {}
                "input_dropped" => return Err(invalid("committed model input was dropped")),
                // This adapter requires the structured profile. A legacy text
                // stream cannot silently replace source error/usage/raw data.
                _ => {
                    return Err(invalid(
                        "model backend emitted an event outside the negotiated profile",
                    ));
                }
            }
            self.sequence = sequence;
        }
        self.cursor = Some(page.cursor);
        self.subscription_closed |= page.subscription_closed;
        if page.terminal && self.session_end.is_none() && !page.has_more {
            return Err(invalid(
                "model registry is terminal without an observed terminal fact",
            ));
        }
        Ok(source)
    }

    fn finish_response(&mut self, terminal: ResponseEnd) -> PortResult<()> {
        if self
            .response_end
            .as_ref()
            .is_some_and(|previous| previous != &terminal)
        {
            return Err(invalid("model response has conflicting terminal facts"));
        }
        self.response_end = Some(terminal);
        Ok(())
    }

    fn finish_session(&mut self, terminal: SessionEnd) -> PortResult<()> {
        // Unknown/failed with resources retained may later be explicitly closed.
        // A confirmed close cannot be undone by a contradictory later event.
        if self.session_end == Some(SessionEnd::Closed) && terminal != SessionEnd::Closed {
            return Err(invalid(
                "closed model session has conflicting terminal facts",
            ));
        }
        self.session_end = Some(terminal);
        Ok(())
    }
}

fn invalid(message: &str) -> PortError {
    PortError::InvalidResponse {
        message: message.into(),
    }
}
fn text<'a>(value: &'a Value, field: &str) -> PortResult<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("model event omitted a required string field"))
}
fn boolean(value: &Value, field: &str) -> PortResult<bool> {
    value
        .get(field)
        .and_then(Value::as_bool)
        .ok_or_else(|| invalid("model event omitted a required boolean field"))
}
