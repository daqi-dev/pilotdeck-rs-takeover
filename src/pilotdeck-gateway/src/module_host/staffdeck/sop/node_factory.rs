//! Run only one admitted node inside its own session. A correction remains
//! inside the same AgentSession turn, so no second user input is persisted.
use super::*;
use pilotdeck_agent::ports::{PortError, PortResult};
use pilotdeck_transport::server::{
    ExecuteRequest, ExecutionContext, ExecutionFactory, ExecutionTerminal,
};
use std::sync::{Arc, Mutex};

pub(super) struct NodeFactory {
    pub runtime: Arc<SopRuntime>,
    pub inner: Arc<dyn ExecutionFactory>,
}
impl ExecutionFactory for NodeFactory {
    fn execute(
        &self,
        request: &ExecuteRequest,
        context: &mut ExecutionContext,
    ) -> PortResult<ExecutionTerminal> {
        let mut current = request.clone();
        let mut payload = request
            .boundary
            .get("payload")
            .cloned()
            .unwrap_or(JsValue::Undefined);
        let mut config = payload
            .get("agent")
            .cloned()
            .unwrap_or_else(|| crate::metadata::object([]));
        set(&mut config, "stopOnStructuredOutput", JsValue::Bool(true));
        set(&mut payload, "agent", config);
        self.runtime.apply_checkpoints(&mut payload);
        let completion = Arc::new(Mutex::new(None::<JsObject>));
        let errors = Arc::new(Mutex::new(false));
        let mut total = None;
        let mut corrected = false;
        let mut terminal;
        loop {
            set(&mut current.boundary, "payload", payload.clone());
            let completion_sink = completion.clone();
            let error_sink = errors.clone();
            terminal = context.with_event_filter(
                Arc::new(move |_, event| {
                    if event.get("type") == Some(&"turn_completed".into()) {
                        *completion_sink.lock().unwrap_or_else(|e| e.into_inner()) =
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
                        *error_sink.lock().unwrap_or_else(|e| e.into_inner()) = true;
                    }
                    true
                }),
                |context| self.inner.execute(&current, context),
            )?;
            self.runtime.observe_checkpoints(&terminal.payload);
            let result = terminal.payload.get("result").cloned().ok_or_else(|| {
                PortError::InvalidResponse {
                    message: "SOP_NODE_SESSION_FAILED: internal node turn did not produce a result"
                        .into(),
                }
            })?;
            total = Some(factory::merge(total.as_ref(), &result));
            let max = payload
                .get("agent")
                .and_then(|v| v.get("maxTurns"))
                .or_else(|| payload.get("maxTurns"))
                .and_then(JsValue::as_number);
            let turns = result
                .get("turns")
                .and_then(JsValue::as_number)
                .unwrap_or(0.0);
            let submitted = result
                .get("structuredOutput")
                .is_some_and(crate::metadata::js_truthy);
            if *errors.lock().unwrap_or_else(|e| e.into_inner())
                || submitted
                || !is(&result, "stopReason", "completed")
                || max.is_some_and(|v| turns >= v)
            {
                break;
            }
            let selected = self
                .runtime
                .turn
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .selected
                .is_some();
            if !selected {
                break;
            }
            if corrected {
                return Err(PortError::InvalidResponse { message: "SOP_STEP_RESULT_REQUIRED: model ended without submitting the active node result".into() });
            }
            corrected = true;
            self.runtime
                .turn
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .correction = true;
            factory::continue_payload(&mut payload, &terminal, &result);
            let mut context = payload
                .get("contextOverride")
                .cloned()
                .unwrap_or_else(|| crate::metadata::object([]));
            set(
                &mut context,
                "messages",
                terminal
                    .payload
                    .get("messages")
                    .cloned()
                    .unwrap_or_else(|| JsValue::Array(vec![])),
            );
            set(&mut payload, "contextOverride", context);
        }
        let result = total.unwrap();
        if let Some(mut completed) = completion.lock().unwrap_or_else(|e| e.into_inner()).take() {
            completed.insert("result".into(), result.clone());
            context.emit("agent.turn_completed".into(), completed)?;
        }
        terminal.payload.insert("result".into(), result);
        Ok(terminal)
    }
}
