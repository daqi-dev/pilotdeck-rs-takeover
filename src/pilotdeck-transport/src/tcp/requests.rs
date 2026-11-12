use super::{
    ControlReply, Hub, State,
    io::{line, lock},
    replay::Binding,
};
use pilotdeck_protocol::{
    JsObject, JsString, JsValue, MODULE_PROTOCOL_VERSION,
    wire::{EnvelopeRef, MessageRef, RequestBodyRef, RequestRef},
};
use std::{io, sync::Arc, time::Instant};

impl Hub {
    pub(super) fn receive(self: &Arc<Self>, peer_id: &JsString, value: &JsValue) -> io::Result<()> {
        let envelope = match EnvelopeRef::decode(value) {
            Ok(envelope) => envelope,
            Err(error) => return self.error(peer_id, error.code, &error.message),
        };
        let mut state = lock(&self.state);
        if state.stopped || !state.peers.contains_key(peer_id) {
            return Err(io::Error::other("TCP peer closed."));
        }
        if let MessageRef::Response(response) = envelope.message {
            let Some(id) = state.callbacks.get(response.in_reply_to).cloned() else {
                return Ok(());
            };
            let peer_binding = state.peers[peer_id].binding.clone();
            let Some(execution) = state.executions.get_mut(&id) else {
                return Ok(());
            };
            // An old socket cannot settle a callback after this stream rebound.
            if execution.peer.as_ref() != Some(peer_id) {
                return Ok(());
            }
            // Binding fields are optional on the wire. A response from the
            // newly bound physical socket may name its current generation;
            // translate that verified field back to the immutable engine's
            // internal owner. Unrelated values still fail its correlation gate.
            let mut routed = response.fields.clone();
            for (key, current, original) in [
                (
                    "moduleInstanceId",
                    &peer_binding.module_instance_id,
                    &execution.initial_binding.module_instance_id,
                ),
                (
                    "connectionGeneration",
                    &peer_binding.connection_generation,
                    &execution.initial_binding.connection_generation,
                ),
            ] {
                if routed.get(key).and_then(JsValue::as_string) == Some(current) {
                    routed.insert(key.into(), JsValue::String(original.clone()));
                }
            }
            execution.engine.server.receive(&JsValue::Object(routed))?;
            if let Some(index) = execution
                .pending
                .iter()
                .position(|(id, _)| id == response.in_reply_to)
                && let Some((_, pending)) = execution.pending.remove(index)
            {
                execution.pending_bytes = execution
                    .pending_bytes
                    .saturating_sub(line(&pending)?.len());
            }
            state.callbacks.remove(response.in_reply_to);
            return Ok(());
        }
        let MessageRef::Request(request) = envelope.message else {
            return Ok(());
        };
        match request.body {
            RequestBodyRef::Hello { .. } | RequestBodyRef::Capabilities { .. } => {
                let peer = &state.peers[peer_id];
                let mut response = response(request, true);
                for (key, value) in [
                    ("protocolVersion", MODULE_PROTOCOL_VERSION.into()),
                    ("moduleId", JsValue::String(peer.binding.module_id.clone())),
                    (
                        "moduleInstanceId",
                        JsValue::String(self.module_instance.clone()),
                    ),
                    (
                        "connectionGeneration",
                        JsValue::String(peer.binding.connection_generation.clone()),
                    ),
                    ("capabilitiesVersion", "1".into()),
                    (
                        "payload",
                        JsValue::Object(
                            if matches!(request.body, RequestBodyRef::Capabilities { .. }) {
                                capabilities(self.options.stream_resume)
                            } else {
                                JsObject::new()
                            },
                        ),
                    ),
                ] {
                    response.insert(key.into(), value);
                }
                peer.output.send(&JsValue::Object(response))
            }
            RequestBodyRef::Execute { identity, .. } => {
                // Source order matters: an active operation refuses every
                // repeat before request-cache lookup, including an identical
                // envelope received through another physical connection.
                if let Some(id) = state.operations.get(identity.operation_id) {
                    let execution = &state.executions[id];
                    if execution.terminal.is_none()
                        && !execution.retired
                        && !execution.engine.owner.is_closed()
                    {
                        return reject(
                            &state,
                            peer_id,
                            request,
                            if &execution.run_id == identity.run_id {
                                "OPERATION_ALREADY_ACTIVE"
                            } else {
                                "OPERATION_IDENTITY_MISMATCH"
                            },
                            false,
                        );
                    }
                }
                if let Some(id) = state.requests.get(identity.request_id) {
                    let execution = &state.executions[id];
                    if &execution.boundary != value {
                        return reject(
                            &state,
                            peer_id,
                            request,
                            "REQUEST_IDENTITY_CONFLICT",
                            false,
                        );
                    }
                    if let Some(cached) = execution.final_event.as_ref() {
                        return state.peers[peer_id].output.send(cached);
                    }
                    if execution.retired || execution.engine.owner.is_closed() {
                        let mut reply = response(request, false);
                        reply.insert("final".into(), JsValue::Bool(true));
                        reply.insert("outcome".into(), "result_unknown".into());
                        reply.insert("code".into(), "RESULT_UNKNOWN".into());
                        return state.peers[peer_id].output.send(&JsValue::Object(reply));
                    }
                    return reject(&state, peer_id, request, "OPERATION_ALREADY_ACTIVE", false);
                }
                if let Some(id) = state.operations.get(identity.operation_id) {
                    let execution = &state.executions[id];
                    let code = if &execution.run_id != identity.run_id {
                        "OPERATION_IDENTITY_MISMATCH"
                    } else if execution.terminal.is_none() {
                        "OPERATION_ALREADY_ACTIVE"
                    } else {
                        "OPERATION_REQUIRES_RECONCILIATION"
                    };
                    return reject(&state, peer_id, request, code, false);
                }
                if state.executions.len() >= self.options.max_executions {
                    return reject(&state, peer_id, request, "EXECUTION_QUEUE_FULL", false);
                }
                self.start_engine(&mut state, peer_id, value)
            }
            RequestBodyRef::Status { request_id } => {
                let Some(id) = state.requests.get(request_id).cloned() else {
                    return reject(&state, peer_id, request, "UNKNOWN_REQUEST", false);
                };
                if state.executions[&id].retired || state.executions[&id].engine.owner.is_closed() {
                    let execution = &state.executions[&id];
                    let mut reply = response(request, true);
                    reply.insert(
                        "payload".into(),
                        JsValue::Object(object([
                            ("runId", JsValue::String(execution.run_id.clone())),
                            (
                                "operationId",
                                JsValue::String(execution.operation_id.clone()),
                            ),
                            (
                                "requestIds",
                                JsValue::Array(vec![JsValue::String(execution.request_id.clone())]),
                            ),
                            ("state", "resolving".into()),
                            ("outcome", "result_unknown".into()),
                            ("cancelRequested", JsValue::Bool(false)),
                            ("updatedAt", JsValue::String(self.clock.now_iso())),
                        ])),
                    );
                    return state.peers[peer_id].output.send(&JsValue::Object(reply));
                }
                forward_control(
                    &mut state,
                    peer_id,
                    request,
                    &id,
                    self.options.queue_capacity,
                )
            }
            RequestBodyRef::Cancel { operation_id, .. } => {
                let Some(id) = state.operations.get(operation_id).cloned() else {
                    return reject(&state, peer_id, request, "UNKNOWN_OPERATION", false);
                };
                if state.executions[&id].retired || state.executions[&id].engine.owner.is_closed() {
                    return reject(&state, peer_id, request, "UNKNOWN_OPERATION", false);
                }
                forward_control(
                    &mut state,
                    peer_id,
                    request,
                    &id,
                    self.options.queue_capacity,
                )
            }
            RequestBodyRef::ModuleCall { .. } => {
                reject(&state, peer_id, request, "UNEXPECTED_MODULE_CALL", true)
            }
            RequestBodyRef::Resume {
                stream_id,
                previous_binding,
                last_applied_sequence,
            } => {
                if !self.options.stream_resume {
                    return reject(&state, peer_id, request, "RESUME_UNSUPPORTED", false);
                }
                let previous = Binding {
                    module_instance_id: previous_binding.module_instance_id.clone(),
                    connection_generation: previous_binding.connection_generation.clone(),
                };
                let now = Instant::now();
                let replay = match state.replay.resume(
                    stream_id,
                    &previous,
                    last_applied_sequence.value(),
                    now,
                ) {
                    Ok(replay) => replay,
                    Err(error) => return reject(&state, peer_id, request, error.code(), false),
                };
                let Some(id) = state.streams.get(stream_id).cloned() else {
                    return reject(&state, peer_id, request, "CURSOR_EXPIRED", false);
                };
                let peer = &state.peers[peer_id];
                let binding = Binding {
                    module_instance_id: self.module_instance.clone(),
                    connection_generation: peer.binding.connection_generation.clone(),
                };
                let output = peer.output.clone();
                let mut reply = response(request, true);
                reply.insert("streamId".into(), JsValue::String(stream_id.clone()));
                reply.insert(
                    "replayedThroughSequence".into(),
                    JsValue::Number(replay.through),
                );
                let mut delivery = line(&JsValue::Object(reply))?;
                for event in replay.events {
                    delivery.extend(line(&event)?);
                }
                let execution = state.executions.get_mut(&id).expect("registered stream");
                for (_, pending) in &execution.pending {
                    delivery.extend(line(pending)?);
                }
                // One queue packet preserves response / replay / pending-call
                // order, including stores larger than the socket queue count.
                // Publish the binding only after this bounded batch is admitted.
                output.send_line(delivery)?;
                execution.peer = Some(peer_id.clone());
                if execution.terminal.is_some() {
                    execution.terminal = Some(now);
                }
                state.replay.rebind(stream_id, binding, now);
                Ok(())
            }
            RequestBodyRef::Ack {
                stream_id,
                last_applied_sequence,
            } => {
                if !self.options.stream_resume {
                    return reject(&state, peer_id, request, "ACK_UNSUPPORTED", false);
                }
                let now = Instant::now();
                if let Err(error) =
                    state
                        .replay
                        .acknowledge(stream_id, last_applied_sequence.value(), now)
                {
                    return reject(&state, peer_id, request, error.code(), false);
                }
                if let Some(id) = state.streams.get(stream_id).cloned()
                    && let Some(execution) = state.executions.get_mut(&id)
                    && execution.terminal.is_some()
                {
                    execution.terminal = Some(now);
                }
                state.peers[peer_id]
                    .output
                    .send(&JsValue::Object(response(request, true)))
            }
        }
    }
    pub(super) fn error(&self, peer_id: &JsString, code: &str, message: &str) -> io::Result<()> {
        let state = lock(&self.state);
        let Some(peer) = state.peers.get(peer_id) else {
            return Err(io::Error::other("TCP peer closed."));
        };
        peer.output.send(&JsValue::Object(object([
            ("kind", "error".into()),
            ("messageId", JsValue::String(new_id())),
            ("code", code.into()),
            ("message", message.into()),
            ("retryability", "unsafe".into()),
        ])))
    }
    pub(super) fn output(&self, id: &JsString, mut value: JsValue) -> io::Result<()> {
        let mut state = lock(&self.state);
        if state.stopped {
            return Err(io::Error::other("TCP service is closed."));
        }
        if !state.executions.contains_key(id) {
            return Err(io::Error::other("Execution retired."));
        }
        if let Some(reply_to) = value.get("inReplyTo").and_then(JsValue::as_string).cloned()
            && let Some(control) = state.controls.remove(&reply_to)
        {
            if &control.engine != id {
                return Err(io::Error::other("Cross-execution control response."));
            }
            if let JsValue::Object(response) = &mut value {
                response.insert("inReplyTo".into(), JsValue::String(control.original));
            }
            if let Some(peer) = state.peers.get(&control.peer) {
                let _ = peer.output.send(&value);
            }
            return Ok(());
        }
        let kind = value.get("kind").and_then(JsValue::as_string);
        if kind.is_some_and(|kind| kind.equals_str("response")) {
            let stream = value.get("streamId").and_then(JsValue::as_string).cloned();
            if let Some(stream) = stream {
                let execution = &state.executions[id];
                let binding = Binding {
                    module_instance_id: self.module_instance.clone(),
                    connection_generation: execution.initial_binding.connection_generation.clone(),
                };
                state
                    .replay
                    .start(stream.clone(), binding, Instant::now())
                    .map_err(io::Error::other)?;
                state.streams.insert(stream.clone(), id.clone());
                state.executions.get_mut(id).unwrap().stream = Some(stream);
            }
        } else if kind.is_some_and(|kind| kind.equals_str("request")) {
            let message = value
                .get("messageId")
                .and_then(JsValue::as_string)
                .ok_or_else(|| io::Error::other("Missing callback identity."))?
                .clone();
            let size = line(&value)?.len();
            let execution = state.executions.get_mut(id).unwrap();
            if execution.pending.len() >= self.options.queue_capacity
                || execution.pending_bytes.saturating_add(size) > self.options.max_queued_bytes
            {
                return Err(io::Error::other("Pending module callback limit exceeded."));
            }
            execution.pending_bytes += size;
            execution
                .pending
                .push_back((message.clone(), value.clone()));
            state.callbacks.insert(message, id.clone());
        } else if kind.is_some_and(|kind| kind.equals_str("event")) {
            let stream = state.executions[id]
                .stream
                .clone()
                .ok_or_else(|| io::Error::other("Event precedes stream acceptance."))?;
            state
                .replay
                .append(&stream, value.clone(), Instant::now())
                .map_err(io::Error::other)?;
        }
        if value.get("final") == Some(&JsValue::Bool(true)) {
            let execution = state.executions.get_mut(id).unwrap();
            execution.terminal = Some(Instant::now());
            execution.final_event = Some(value.clone());
            execution.pending_bytes = 0;
            let callbacks = execution
                .pending
                .drain(..)
                .map(|(id, _)| id)
                .collect::<Vec<_>>();
            for callback in callbacks {
                state.callbacks.remove(&callback);
            }
        }
        if let Some(peer) = state.executions[id]
            .peer
            .as_ref()
            .and_then(|peer| state.peers.get(peer))
        {
            // Disconnect only drops physical delivery. Resumable logical
            // execution and the exact pending call remain owned by this hub.
            let _ = peer.output.send(&value);
        }
        Ok(())
    }
}

fn forward_control(
    state: &mut State,
    peer: &JsString,
    request: RequestRef<'_>,
    engine: &JsString,
    capacity: usize,
) -> io::Result<()> {
    if state.controls.len() >= capacity {
        return Err(io::Error::other("TCP control queue is full."));
    }
    // Request IDs are scoped to peers, whereas one live service accepts many
    // peers. Rewrite only control message correlation, never execute identity.
    let id = new_id();
    let mut value = request.fields.clone();
    value.insert("messageId".into(), JsValue::String(id.clone()));
    state.controls.insert(
        id.clone(),
        ControlReply {
            peer: peer.clone(),
            original: request.message_id.clone(),
            engine: engine.clone(),
        },
    );
    let result = state.executions[engine]
        .engine
        .server
        .receive(&JsValue::Object(value));
    if result.is_err() {
        state.controls.remove(&id);
    }
    result
}
fn reject(
    state: &State,
    peer: &JsString,
    request: RequestRef<'_>,
    code: &str,
    final_failure: bool,
) -> io::Result<()> {
    let mut reply = response(request, false);
    reply.insert("code".into(), code.into());
    if final_failure {
        reply.insert("final".into(), JsValue::Bool(true));
        reply.insert("outcome".into(), "failed".into());
    }
    state.peers[peer].output.send(&JsValue::Object(reply))
}
fn response(request: RequestRef<'_>, ok: bool) -> JsObject {
    let mut response = object([
        ("kind", "response".into()),
        ("messageId", JsValue::String(new_id())),
        ("inReplyTo", JsValue::String(request.message_id.clone())),
        ("ok", JsValue::Bool(ok)),
    ]);
    if let Some(JsValue::String(id)) = request.fields.get("requestId") {
        response.insert("requestId".into(), JsValue::String(id.clone()));
    }
    response
}
fn new_id() -> JsString {
    format!("tcp-{}", uuid::Uuid::new_v4()).into()
}
fn object<const N: usize>(fields: [(&str, JsValue); N]) -> JsObject {
    let mut object = JsObject::new();
    for (key, value) in fields {
        object.insert(key.into(), value);
    }
    object
}
fn capabilities(resume: bool) -> JsObject {
    let methods = [
        ("execute", true),
        ("cancel", true),
        ("status", true),
        ("resume", resume),
        ("ack", resume),
    ]
    .map(|(name, enabled)| {
        let mut method = object([("name", name.into()), ("enabled", JsValue::Bool(enabled))]);
        if name == "execute" {
            for (key, value) in [
                ("profiles", JsValue::Array(vec!["streaming".into()])),
                ("cancel", JsValue::Bool(true)),
                (
                    "resumeSupport",
                    if resume { "streaming" } else { "none" }.into(),
                ),
                ("retry", "retry_after_status".into()),
            ] {
                method.insert(key.into(), value);
            }
        }
        JsValue::Object(method)
    });
    object([
        ("capabilitiesVersion", "1".into()),
        ("methods", JsValue::Array(methods.into())),
    ])
}
