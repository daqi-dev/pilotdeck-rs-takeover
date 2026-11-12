use super::*;
use crate::{metadata::object, runtime::turn::live::DurableMessageObserver};
use pilotdeck_agent::ports::{CallControl, PortError, PortResult};
use pilotdeck_protocol::{
    admission::{CallEffect, ExecutionIdentity},
    module::ModuleOutcome,
};
use pilotdeck_transport::server::{
    ExecuteRequest, ExecutionContext, ExecutionFactory, ExecutionTerminal,
};
use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

impl SopRuntime {
    pub(crate) fn factory(
        self: &Arc<Self>,
        inner: Arc<dyn ExecutionFactory>,
    ) -> Arc<dyn ExecutionFactory> {
        Arc::new(Factory {
            runtime: self.clone(),
            inner,
            executor: tokio::runtime::Handle::current(),
        })
    }
    pub(crate) fn durable_observer(self: &Arc<Self>) -> DurableMessageObserver {
        let session = self.session.clone();
        let runtime = self.clone();
        DurableMessageObserver {
            matches: Arc::new(move |identity, message| {
                identity.session_id == session
                    && message
                        .get("metadata")
                        .is_some_and(|v| is(v, "purpose", "staffdeck_sop_reply"))
            }),
            on_persisted: Arc::new(move |identity, _message| {
                let runtime = runtime.clone();
                let delivered = identity.turn_id.clone();
                Box::pin(async move {
                    let current = runtime
                        .store
                        .read()
                        .await
                        .map_err(crate::host::ModuleError::from)?;
                    let expected = current
                        .as_ref()
                        .and_then(|v| v.get("replyDelivery"))
                        .and_then(|v| v.get("turnId"))
                        .and_then(JsValue::as_string)
                        .cloned()
                        .ok_or_else(|| {
                            crate::host::ModuleError::new(
                                "No pending StaffDeck SOP reply delivery exists.",
                            )
                        })?;
                    runtime
                        .store
                        .mark_reply_durable(expected, delivered)
                        .await
                        .map_err(crate::host::ModuleError::from)?;
                    Ok(())
                })
            }),
        }
    }
}
struct Factory {
    runtime: Arc<SopRuntime>,
    inner: Arc<dyn ExecutionFactory>,
    executor: tokio::runtime::Handle,
}
impl Factory {
    fn wait<T>(
        &self,
        control: &CallControl,
        future: impl Future<Output = Result<T, SopError>>,
    ) -> PortResult<T> {
        self.executor.block_on(async {
            tokio::pin!(future);
            loop {
                control.check()?;
                tokio::select! {
                    biased;
                    result = &mut future => return result.map_err(port_error),
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                }
            }
        })
    }
    fn reply(
        &self,
        context: &mut ExecutionContext,
        identity: &ExecutionIdentity,
        request: &ExecuteRequest,
        result: &JsValue,
        recovered: bool,
    ) -> PortResult<JsValue> {
        let turn = String::from_utf16_lossy(identity.turn_id.units());
        let reply_turn = if recovered {
            format!("{turn}:recovered-step")
        } else {
            turn
        };
        let message = object([
            ("role", "assistant".into()),
            (
                "content",
                JsValue::Array(vec![object([
                    ("type", "text".into()),
                    (
                        "text",
                        result.get("replyFragment").cloned().unwrap_or("".into()),
                    ),
                ])]),
            ),
            (
                "metadata",
                object([
                    ("purpose", "staffdeck_sop_reply".into()),
                    (
                        "transientId",
                        format!("staffdeck-sop-reply:{reply_turn}").as_str().into(),
                    ),
                ]),
            ),
        ]);
        let event = object([
            ("type", "assistant_message".into()),
            ("sessionId", JsValue::String(identity.session_id.clone())),
            ("turnId", JsValue::String(identity.turn_id.clone())),
            ("message", message.clone()),
        ]);
        context.emit(
            "agent.assistant_message".into(),
            event.as_object().unwrap().clone(),
        )?;
        // Every callback waits for earlier events' application. The configured
        // durable observer marks delivery between persistence and visibility;
        // this pure metadata read only waits for that already-owned transition.
        let agent = request.boundary.get("payload").and_then(|v| v.get("agent"));
        let metadata = object([
            ("operation", "get_metadata".into()),
            (
                "provider",
                agent
                    .and_then(|v| v.get("provider"))
                    .cloned()
                    .unwrap_or("default".into()),
            ),
            (
                "model",
                agent
                    .and_then(|v| v.get("model"))
                    .cloned()
                    .unwrap_or("default".into()),
            ),
        ]);
        context.call(
            context.identity(identity.session_id.clone(), identity.turn_id.clone()),
            "model",
            "get_metadata",
            CallEffect::ReadOnly,
            metadata.as_object().unwrap().clone(),
        )?;
        Ok(message)
    }
}
impl ExecutionFactory for Factory {
    fn execute(
        &self,
        request: &ExecuteRequest,
        context: &mut ExecutionContext,
    ) -> PortResult<ExecutionTerminal> {
        let session = request
            .boundary
            .get("sessionId")
            .and_then(JsValue::as_string)
            .cloned()
            .unwrap_or_else(|| request.operation_id.clone());
        let turn = request
            .boundary
            .get("turnId")
            .and_then(JsValue::as_string)
            .cloned()
            .unwrap_or_else(|| request.operation_id.clone());
        if session != self.runtime.session {
            return Err(port_error(SopError::new(
                "SOP_SESSION_MISMATCH",
                "SOP execution belongs to another session.",
            )));
        }
        let identity = ExecutionIdentity {
            session_id: session,
            turn_id: turn,
            run_id: request.run_id.clone(),
            operation_id: request.operation_id.clone(),
            request_id: request.request_id.clone(),
        };
        let mut current = request.clone();
        let mut payload = request.boundary.get("payload").cloned().ok_or_else(|| {
            port_error(SopError::new(
                "SOP_INPUT_INVALID",
                "SOP execution payload is missing.",
            ))
        })?;
        let mut agent = payload.get("agent").cloned().unwrap_or_else(|| object([]));
        set(&mut agent, "stopOnStructuredOutput", JsValue::Bool(true));
        set(&mut payload, "agent", agent);
        let mut messages = payload
            .get("contextOverride")
            .and_then(|v| v.get("messages"))
            .or_else(|| payload.get("messages"))
            .cloned()
            .unwrap_or_else(|| JsValue::Array(vec![]));
        let current_state = self.wait(&context.control, self.runtime.store.read())?;
        if let Some(delivery) = current_state.as_ref().and_then(|v| v.get("replyDelivery")) {
            let active = current_state
                .as_ref()
                .and_then(|v| v.get("state"))
                .is_some_and(|v| is(v, "status", "active"));
            let result = delivery.get("result").unwrap();
            let message = if !is(delivery, "phase", "durable")
                || delivery.get("turnId").and_then(JsValue::as_string) != Some(&identity.turn_id)
            {
                self.reply(context, &identity, request, result, active)?
            } else {
                reply_message(result, &identity.turn_id)
            };
            append_once(&mut messages, message.clone());
            self.wait(
                &context.control,
                self.runtime.store.clear_reply(identity.turn_id.clone()),
            )?;
            if !active {
                let outcome = object([
                    ("type", "success".into()),
                    ("sessionId", JsValue::String(identity.session_id.clone())),
                    ("turnId", JsValue::String(identity.turn_id.clone())),
                    ("finalMessage", message),
                    ("stopReason", "completed".into()),
                    ("usage", object([])),
                    ("permissionDenials", JsValue::Array(vec![])),
                    ("turns", JsValue::Number(0.0)),
                    ("startedAt", now()),
                    ("completedAt", now()),
                    ("structuredOutput", object([("sop", result.clone())])),
                ]);
                emit_completion(context, &identity, &outcome)?;
                return Ok(ExecutionTerminal {
                    outcome: ModuleOutcome::Completed,
                    payload: object([("result", outcome), ("messages", messages)])
                        .as_object()
                        .unwrap()
                        .clone(),
                    error: None,
                });
            }
        }
        self.wait(&context.control, self.runtime.start(&messages))?;
        let captured = Arc::new(Mutex::new(None::<JsObject>));
        let saw_error = Arc::new(Mutex::new(false));
        let mut total: Option<JsValue> = None;
        let mut corrections = std::collections::HashSet::new();
        let mut terminal;
        loop {
            let prepared = self.wait(&context.control, self.runtime.prepare(&identity, None))?;
            if let Some(prepared) = prepared {
                let step = prepared.get("step").unwrap();
                if is(step, "contextMode", "new_session") {
                    return Err(port_error(SopError::new(
                        "SOP_NODE_SESSION_UNAVAILABLE",
                        "StaffDeck SOP node session isolation is not yet available in this host.",
                    )));
                }
                self.runtime
                    .turn
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .preflight = true;
                if let Some(node) = definitions::node(
                    &self.runtime.bundle,
                    &text(step, "skillId").unwrap_or_default(),
                    &text(step, "nodeId").unwrap_or_default(),
                ) {
                    if let Some((provider, model)) = node.get("model").and_then(definitions::model)
                    {
                        set(
                            &mut payload,
                            "modelOverride",
                            object([
                                ("provider", provider.as_str().into()),
                                ("model", model.as_str().into()),
                            ]),
                        );
                    }
                }
            }
            let mut override_context = payload
                .get("contextOverride")
                .cloned()
                .unwrap_or_else(|| object([]));
            set(&mut override_context, "messages", messages.clone());
            set(&mut payload, "contextOverride", override_context);
            set(&mut current.boundary, "payload", payload.clone());
            let event_capture = captured.clone();
            let error_capture = saw_error.clone();
            terminal = context.with_event_filter(
                Arc::new(move |_, event| {
                    if event.get("type") == Some(&"turn_completed".into()) {
                        *event_capture.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(event.clone());
                        return false;
                    }
                    if event.get("type") == Some(&"tool_result".into())
                        && event.get("result").is_some_and(|v| {
                            is(v, "type", "error")
                                && !v
                                    .get("error")
                                    .is_some_and(|v| is(v, "code", "invalid_tool_input"))
                        })
                    {
                        *error_capture.lock().unwrap_or_else(|e| e.into_inner()) = true;
                    }
                    true
                }),
                |context| self.inner.execute(&current, context),
            )?;
            let result = terminal.payload.get("result").cloned().ok_or_else(|| {
                port_error(SopError::new(
                    "SOP_NODE_SESSION_FAILED",
                    "Internal node turn did not produce a result.",
                ))
            })?;
            total = Some(merge(total.as_ref(), &result));
            messages = terminal
                .payload
                .get("messages")
                .cloned()
                .unwrap_or(messages);
            let state = self.wait(&context.control, self.runtime.store.read())?;
            let active = state
                .as_ref()
                .and_then(|v| v.get("state"))
                .is_some_and(|v| is(v, "status", "active"));
            let max_turns = payload
                .get("agent")
                .and_then(|v| v.get("maxTurns"))
                .or_else(|| payload.get("maxTurns"))
                .and_then(JsValue::as_number);
            let spent_turns = result
                .get("turns")
                .and_then(JsValue::as_number)
                .unwrap_or(0.0);
            if active
                && is(&result, "stopReason", "completed")
                && max_turns.is_some_and(|max| spent_turns >= max)
            {
                if let Some(total) = &mut total {
                    set(total, "type", "max_turns".into());
                    set(total, "stopReason", "max_turns".into());
                }
                break;
            }
            let submitted = result
                .get("structuredOutput")
                .is_some_and(crate::metadata::js_truthy);
            let needs_correction = active
                && is(&result, "stopReason", "completed")
                && !submitted
                && !*saw_error.lock().unwrap_or_else(|e| e.into_inner())
                && self
                    .runtime
                    .turn
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .submission
                    .is_none();
            if !active || (!submitted && !needs_correction) {
                break;
            }
            if needs_correction {
                let step = self.runtime.step().unwrap_or(JsValue::Undefined);
                let key = format!(
                    "{}:{}",
                    text(&step, "skillId").unwrap_or_default(),
                    text(&step, "nodeId").unwrap_or_default()
                );
                if !corrections.insert(key) {
                    return Err(port_error(SopError::new(
                        "SOP_STEP_RESULT_REQUIRED",
                        "SOP_STEP_RESULT_REQUIRED: model ended without submitting the active node result",
                    )));
                }
            }
            self.runtime
                .turn
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .correction = needs_correction;
            continue_payload(&mut payload, &terminal, &result);
        }
        let mut result = total.unwrap();
        let submission = self
            .runtime
            .turn
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .submission
            .clone();
        if let Some(submission) = submission {
            let reply = self.reply(context, &identity, request, &submission, false)?;
            append_once(&mut messages, reply.clone());
            set(&mut result, "finalMessage", reply);
            set(
                &mut result,
                "structuredOutput",
                object([("sop", submission)]),
            );
            emit_completion(context, &identity, &result)?;
            self.wait(
                &context.control,
                self.runtime.store.clear_reply(identity.turn_id.clone()),
            )?;
        } else if captured.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
            emit_completion(context, &identity, &result)?;
        }
        terminal.payload.insert("result".into(), result);
        terminal.payload.insert("messages".into(), messages);
        Ok(terminal)
    }
}
fn port_error(error: SopError) -> PortError {
    PortError::InvalidResponse {
        message: error.message.into(),
    }
}
fn reply_message(result: &JsValue, turn: &JsString) -> JsValue {
    object([
        ("role", "assistant".into()),
        (
            "content",
            JsValue::Array(vec![object([
                ("type", "text".into()),
                (
                    "text",
                    result.get("replyFragment").cloned().unwrap_or("".into()),
                ),
            ])]),
        ),
        (
            "metadata",
            object([
                ("purpose", "staffdeck_sop_reply".into()),
                (
                    "transientId",
                    format!(
                        "staffdeck-sop-reply:{}",
                        String::from_utf16_lossy(turn.units())
                    )
                    .as_str()
                    .into(),
                ),
            ]),
        ),
    ])
}
fn append_once(messages: &mut JsValue, message: JsValue) {
    if let JsValue::Array(rows) = messages {
        let id = message.get("metadata").and_then(|v| v.get("transientId"));
        if !rows
            .iter()
            .any(|v| v.get("metadata").and_then(|v| v.get("transientId")) == id)
        {
            rows.push(message);
        }
    }
}
fn emit_completion(
    context: &mut ExecutionContext,
    identity: &ExecutionIdentity,
    result: &JsValue,
) -> PortResult<()> {
    context.emit(
        "agent.turn_completed".into(),
        object([
            ("type", "turn_completed".into()),
            ("sessionId", JsValue::String(identity.session_id.clone())),
            ("turnId", JsValue::String(identity.turn_id.clone())),
            ("result", result.clone()),
        ])
        .as_object()
        .unwrap()
        .clone(),
    )
}
fn merge(previous: Option<&JsValue>, current: &JsValue) -> JsValue {
    let Some(previous) = previous else {
        return current.clone();
    };
    let mut next = current.clone();
    set(
        &mut next,
        "turns",
        JsValue::Number(
            previous
                .get("turns")
                .and_then(JsValue::as_number)
                .unwrap_or(0.0)
                + current
                    .get("turns")
                    .and_then(JsValue::as_number)
                    .unwrap_or(0.0),
        ),
    );
    if let Some(start) = previous.get("startedAt") {
        set(&mut next, "startedAt", start.clone());
    }
    let mut usage = current.get("usage").cloned().unwrap_or_else(|| object([]));
    if let Some(prior) = previous.get("usage").and_then(JsValue::as_object) {
        for (name, value) in prior.entries() {
            if let Some(number) = value.as_number() {
                let key = String::from_utf16_lossy(name.units());
                let sum = usage.get(&key).and_then(JsValue::as_number).unwrap_or(0.0) + number;
                set(&mut usage, &key, JsValue::Number(sum));
            }
        }
    }
    set(&mut next, "usage", usage);
    let mut denials = match previous.get("permissionDenials") {
        Some(JsValue::Array(values)) => values.clone(),
        _ => vec![],
    };
    if let Some(JsValue::Array(values)) = current.get("permissionDenials") {
        denials.extend(values.iter().cloned());
    }
    set(&mut next, "permissionDenials", JsValue::Array(denials));
    if let Some(mut budget) = current.get("budget").cloned() {
        let sum = previous
            .get("budget")
            .and_then(|v| v.get("turnSpentUsd"))
            .and_then(JsValue::as_number)
            .unwrap_or(0.0)
            + budget
                .get("turnSpentUsd")
                .and_then(JsValue::as_number)
                .unwrap_or(0.0);
        set(&mut budget, "turnSpentUsd", JsValue::Number(sum));
        set(&mut next, "budget", budget);
    }
    next
}
fn continue_payload(payload: &mut JsValue, terminal: &ExecutionTerminal, result: &JsValue) {
    for key in ["seedState", "modelState"] {
        if let Some(value) = terminal.payload.get(key) {
            set(payload, key, value.clone());
        }
    }
    let turns = result
        .get("turns")
        .and_then(JsValue::as_number)
        .unwrap_or(0.0);
    let mut agent = payload.get("agent").cloned().unwrap_or_else(|| object([]));
    if let Some(max) = agent
        .get("maxTurns")
        .or_else(|| payload.get("maxTurns"))
        .and_then(JsValue::as_number)
    {
        set(
            &mut agent,
            "maxTurns",
            JsValue::Number((max - turns).max(0.0)),
        );
    }
    set(payload, "agent", agent);
    let spent = result
        .get("budget")
        .and_then(|v| v.get("turnSpentUsd"))
        .and_then(JsValue::as_number)
        .unwrap_or(0.0);
    set(
        payload,
        "initialTaskBudgetSpentUsd",
        JsValue::Number(
            payload
                .get("initialTaskBudgetSpentUsd")
                .and_then(JsValue::as_number)
                .unwrap_or(0.0)
                + spent,
        ),
    );
    if let Some(max) = payload.get("maxBudgetUsd").and_then(JsValue::as_number) {
        set(
            payload,
            "maxBudgetUsd",
            JsValue::Number((max - spent).max(0.0)),
        );
    }
}
