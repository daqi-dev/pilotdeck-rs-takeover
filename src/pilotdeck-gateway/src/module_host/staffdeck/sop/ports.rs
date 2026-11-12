use super::*;
use crate::{
    host::{
        HostFuture, ModuleError,
        modules::{capability, context, model},
    },
    metadata::object,
};
use pilotdeck_protocol::admission::ExecutionIdentity;
use std::sync::Arc;

pub(super) struct Context {
    pub runtime: Arc<SopRuntime>,
    pub identity: ExecutionIdentity,
    pub inner: Arc<dyn context::ContextRuntime>,
}
impl context::ContextRuntime for Context {
    fn supports(&self, operation: context::Operation) -> bool {
        self.inner.supports(operation)
    }
    fn prepare_for_model(&self, mut input: context::ContextInput) -> HostFuture<JsValue> {
        let runtime = self.runtime.clone();
        let inner = self.inner.clone();
        let identity = self.identity.clone();
        Box::pin(async move {
            let preflight = {
                let mut turn = runtime.turn.lock().unwrap_or_else(|e| e.into_inner());
                if turn.preflight {
                    turn.preflight = false;
                    turn.prepared.clone()
                } else {
                    None
                }
            };
            let prepared = match preflight {
                Some(prepared) => Some(prepared),
                None => runtime
                    .prepare(&identity, input.fields.get("stepId").cloned())
                    .await
                    .map_err(ModuleError::from)?,
            };
            if let Some(prepared) = prepared {
                let current = runtime.store.read().await.map_err(ModuleError::from)?;
                let mut instruction = prompt::instruction(&prepared, current.as_ref());
                if runtime
                    .turn
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .correction
                {
                    let step = prepared.get("step").unwrap();
                    let correction = if step.get("isTerminal") == Some(&JsValue::Bool(true))
                        && step.get("declaresHandoff") != Some(&JsValue::Bool(true))
                        && strings(step, "expectedUserInfo").is_empty()
                    {
                        "SOP_STEP_RESULT_REQUIRED: The previous assistant text was only a draft. The final SOP node is still active. Call submit_step_result now with status completed, the final answer in replyFragment, slotUpdates {}, and no nextStepId. Do not repeat the draft as plain assistant text."
                    } else {
                        "SOP_STEP_RESULT_REQUIRED: Your previous response did not submit the current SOP node result. Text cannot create an approval or missing-field wait. Follow the current node contract and call submit_step_result with your actual result before ending. Do not claim a persisted wait or completion without a successful tool receipt."
                    };
                    instruction.push_str("\n\n");
                    instruction.push_str(correction);
                }
                let existing = input
                    .fields
                    .get("appendSystemPrompt")
                    .and_then(JsValue::as_string)
                    .and_then(|v| v.to_utf8().ok())
                    .filter(|v| !crate::metadata::js_trim(v).is_empty());
                input.fields.insert(
                    "appendSystemPrompt".into(),
                    existing
                        .map(|v| format!("{v}\n\n{instruction}"))
                        .unwrap_or(instruction)
                        .as_str()
                        .into(),
                );
            }
            inner.prepare_for_model(input).await
        })
    }
    fn apply_tool_results(&self, input: context::ContextInput) -> HostFuture<JsValue> {
        self.inner.apply_tool_results(input)
    }
    fn recover_from_model_error(&self, input: context::ContextInput) -> HostFuture<JsValue> {
        self.inner.recover_from_model_error(input)
    }
    fn capture_turn(&self, input: context::ContextInput) -> HostFuture<JsValue> {
        self.inner.capture_turn(input)
    }
    fn try_auto_compact(&self, input: context::ContextInput) -> HostFuture<JsValue> {
        self.inner.try_auto_compact(input)
    }
}
pub(super) struct Model {
    pub runtime: Arc<SopRuntime>,
    pub inner: Arc<dyn model::ModelExecution>,
}
impl model::ModelExecution for Model {
    fn prepare(
        &self,
        mut input: model::PrepareInput,
    ) -> model::ModelFuture<model::PreparedInvocation> {
        apply_model_tools(&mut input.request, self.runtime.step().as_ref());
        self.inner.prepare(input)
    }
    fn stream(
        &self,
        mut input: model::StreamInput,
    ) -> model::ModelFuture<Box<dyn model::ModelStream>> {
        apply_model_tools(&mut input.prepared.request, self.runtime.step().as_ref());
        self.inner.stream(input)
    }
}
fn apply_model_tools(request: &mut JsValue, step: Option<&JsValue>) {
    let Some(step) = step else { return };
    let required = !strings(step, "requiredToolNames").is_empty();
    let knowledge = step.get("optionalKnowledge").is_some();
    set(
        request,
        "toolChoice",
        if required
            || knowledge
            || step
                .get("node")
                .is_some_and(|v| is(v, "type", "knowledge_query"))
        {
            "required".into()
        } else {
            object([
                ("type", "tool".into()),
                ("name", "submit_step_result".into()),
            ])
        },
    );
    if !required && let Some(JsValue::Array(tools)) = request.get("tools") {
        let tools = tools
            .iter()
            .filter(|v| {
                is(v, "name", "submit_step_result")
                    || (knowledge && is(v, "name", "knowledge_query"))
            })
            .cloned()
            .collect();
        set(request, "tools", JsValue::Array(tools));
    }
}
pub(super) struct Tools {
    pub runtime: Arc<SopRuntime>,
    pub identity: ExecutionIdentity,
    pub inner: Arc<dyn capability::ToolExecution>,
}
impl capability::ToolCatalog for Tools {
    fn list(&self) -> HostFuture<Vec<Arc<dyn capability::ToolDefinition>>> {
        let runtime = self.runtime.clone();
        let inner = self.inner.clone();
        Box::pin(async move {
            let mut tools = inner.list().await?;
            if tools
                .iter()
                .any(|v| v.fields().get("name") == Some(&"submit_step_result".into()))
            {
                return Err(ModuleError::new(
                    "submit_step_result is reserved by the StaffDeck SOP runtime.",
                )
                .into());
            }
            let selected = runtime
                .turn
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .selected
                .clone();
            if let Some(selected) = &selected {
                definitions::required_tools(
                    &runtime.bundle,
                    selected,
                    &tools
                        .iter()
                        .filter_map(|v| {
                            v.fields().get("name").and_then(JsValue::as_string).cloned()
                        })
                        .collect::<Vec<_>>(),
                )
                .map_err(ModuleError::from)?;
            }
            if selected.is_some()
                && let Some(step) = runtime.step()
            {
                if strings(&step, "requiredToolNames").is_empty() {
                    tools.clear();
                }
                tools.push(Arc::new(ControlDefinition::new()));
            }
            Ok(tools)
        })
    }
}
impl capability::ToolExecution for Tools {
    fn refresh(&self) -> HostFuture<()> {
        self.inner.refresh()
    }
    fn execute_all(
        &self,
        calls: Vec<capability::ToolCall>,
        context: capability::ToolInvocationContext,
    ) -> HostFuture<Vec<JsObject>> {
        let runtime = self.runtime.clone();
        let inner = self.inner.clone();
        let identity = self.identity.clone();
        Box::pin(async move {
            let ordinary = calls
                .iter()
                .filter(|v| !v.name.equals_str("submit_step_result"))
                .cloned()
                .collect::<Vec<_>>();
            let step = runtime.step();
            let selected = runtime
                .turn
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .selected
                .is_some();
            let results = if !ordinary.is_empty()
                && selected
                && step
                    .as_ref()
                    .is_some_and(|v| strings(v, "requiredToolNames").is_empty())
            {
                ordinary
                    .iter()
                    .map(|call| {
                        control_error(
                            call,
                            SopError::new(
                                "tool_execution_failed",
                                "SOP_AUTHORITY_TOOL_UNAVAILABLE",
                            ),
                        )
                    })
                    .collect()
            } else if !ordinary.is_empty() {
                inner.execute_all(ordinary.clone(), context).await?
            } else {
                vec![]
            };
            if selected {
                let names = results
                    .iter()
                    .filter(|v| v.get("type") == Some(&"success".into()))
                    .filter_map(|v| v.get("toolName").and_then(JsValue::as_string).cloned())
                    .collect::<Vec<_>>();
                if !names.is_empty() {
                    runtime
                        .store
                        .record_successful_tools(names)
                        .await
                        .map_err(ModuleError::from)?;
                }
            }
            let mut ordered = Vec::new();
            for call in &calls {
                if !call.name.equals_str("submit_step_result") {
                    ordered.push(
                        results
                            .iter()
                            .find(|v| {
                                v.get("toolCallId").and_then(JsValue::as_string) == Some(&call.id)
                            })
                            .cloned()
                            .unwrap_or_else(|| {
                                control_error(
                                    call,
                                    SopError::new(
                                        "tool_execution_failed",
                                        "Tool execution produced no result.",
                                    ),
                                )
                            }),
                    );
                } else if !ordinary.is_empty() {
                    ordered.push(control_error(call, SopError::new("tool_execution_failed", "Call submit_step_result only after the preceding tool results are available to you.")));
                } else {
                    ordered.push(match submit(&runtime, call, &identity).await {
                        Ok(result) => result,
                        Err(error) => control_error(call, error),
                    });
                }
            }
            Ok(ordered)
        })
    }
}
async fn submit(
    runtime: &SopRuntime,
    call: &capability::ToolCall,
    identity: &ExecutionIdentity,
) -> Result<JsObject, SopError> {
    let selected = runtime
        .turn
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .selected
        .clone()
        .ok_or_else(|| {
            SopError::new(
                "tool_execution_failed",
                "No StaffDeck SOP is selected for this session.",
            )
        })?;
    let mut proposal = proposal::parse(&call.input).ok_or_else(|| SopError::new("invalid_tool_input", "submit_step_result requires a valid status, non-empty replyFragment, and correctly typed known fields."))?;
    let persisted = runtime
        .store
        .load_or_create(runtime.bundle.clone(), selected.as_str().into())
        .await?;
    let state = persisted.get("state").unwrap();
    if let Some(step) = runtime.step() {
        let mut turn = runtime.turn.lock().unwrap_or_else(|e| e.into_inner());
        let user = turn.user_input.clone();
        proposal::validate(
            &mut proposal,
            &step,
            state,
            &user,
            &mut turn.collect_checked,
        )?;
    }
    let body = object([
        ("bundle", persisted.get("bundle").unwrap().clone()),
        ("state", state.clone()),
        ("proposal", proposal),
        (
            "successfulToolNames",
            JsValue::Array(
                strings(state, "successful_tool_names")
                    .iter()
                    .filter(|v| !v.is_empty())
                    .map(|v| v.as_str().into())
                    .collect(),
            ),
        ),
    ]);
    let context = object([
        ("runId", JsValue::String(identity.run_id.clone())),
        (
            "operationId",
            JsValue::String(identity.operation_id.clone()),
        ),
        (
            "requestId",
            format!("sop.submit:{}", String::from_utf16_lossy(call.id.units()))
                .as_str()
                .into(),
        ),
        ("sessionId", JsValue::String(runtime.session.clone())),
        ("turnId", JsValue::String(identity.turn_id.clone())),
        ("idempotencyKey", JsValue::String(call.id.clone())),
        (
            "expectedRevision",
            persisted.get("revision").unwrap().clone(),
        ),
    ])
    .as_object()
    .unwrap()
    .clone();
    let submitted = runtime
        .client
        .submit(body, context, &runtime.signal())
        .await?;
    let next_state = submitted.get("state").unwrap();
    let result = submitted.get("result").unwrap();
    runtime
        .store
        .commit(
            persisted.get("bundle").unwrap().clone(),
            next_state.clone(),
            persisted
                .get("revision")
                .and_then(JsValue::as_number)
                .unwrap(),
            identity.turn_id.clone(),
            result.clone(),
        )
        .await?;
    if is(next_state, "status", "active") {
        runtime.store.clear_reply(identity.turn_id.clone()).await?;
    } else {
        runtime
            .turn
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .submission = Some(result.clone());
    }
    let mut control = object([
        (
            "submittedStepId",
            state
                .get("active_step_id")
                .cloned()
                .unwrap_or(JsValue::Null),
        ),
        ("status", result.get("status").unwrap().clone()),
        ("resultStatus", result.get("status").unwrap().clone()),
        (
            "activeStepId",
            next_state
                .get("active_step_id")
                .cloned()
                .unwrap_or(JsValue::Null),
        ),
        (
            "sopStatus",
            next_state.get("status").cloned().unwrap_or(JsValue::Null),
        ),
        (
            "nextStepId",
            result.get("nextStepId").cloned().unwrap_or(JsValue::Null),
        ),
    ]);
    if is(next_state, "status", "active") {
        set(
            &mut control,
            "nextStepRequiresOwnSubmission",
            JsValue::Bool(true),
        );
    }
    Ok(object([
        ("type", "success".into()),
        ("toolCallId", JsValue::String(call.id.clone())),
        ("toolName", "submit_step_result".into()),
        (
            "content",
            JsValue::Array(vec![object([("type", "json".into()), ("value", control)])]),
        ),
        ("data", result.clone()),
        (
            "metadata",
            object([("structuredOutput", JsValue::Bool(true))]),
        ),
        ("startedAt", now()),
        ("completedAt", now()),
    ])
    .as_object()
    .unwrap()
    .clone())
}
fn control_error(call: &capability::ToolCall, error: SopError) -> JsObject {
    let direct = [
        "invalid_tool_input",
        "tool_aborted",
        "tool_execution_failed",
    ]
    .contains(&error.code.as_str());
    let code = if direct {
        error.code.as_str()
    } else if error.code == "SOP_RUNTIME_CANCELLED" {
        "tool_aborted"
    } else {
        "tool_execution_failed"
    };
    let message = if direct {
        error.message.clone()
    } else {
        format!("[{}] {}", error.code, error.message)
    };
    let mut failure = object([("code", code.into()), ("message", message.as_str().into())]);
    if direct {
        if let Some(details) = error.details {
            set(&mut failure, "details", *details);
        }
    } else {
        let mut details = error
            .details
            .as_deref()
            .cloned()
            .unwrap_or_else(|| object([]));
        set(
            &mut details,
            "sopRuntime",
            object([
                ("code", error.code.as_str().into()),
                ("message", error.message.as_str().into()),
                (
                    "retryability",
                    error.retryability.as_deref().unwrap_or("unsafe").into(),
                ),
                (
                    "details",
                    error
                        .details
                        .map(|value| *value)
                        .unwrap_or_else(|| object([])),
                ),
            ]),
        );
        set(&mut failure, "details", details);
    }
    object([
        ("type", "error".into()),
        ("toolCallId", JsValue::String(call.id.clone())),
        ("toolName", JsValue::String(call.name.clone())),
        ("error", failure),
        (
            "content",
            JsValue::Array(vec![object([
                ("type", "text".into()),
                ("text", message.as_str().into()),
            ])]),
        ),
        ("startedAt", now()),
        ("completedAt", now()),
    ])
    .as_object()
    .unwrap()
    .clone()
}
struct ControlDefinition(JsObject);
impl ControlDefinition {
    fn new() -> Self {
        Self(pilotdeck_protocol::parse_json(r#"{"name":"submit_step_result","title":"Submit SOP step result","description":"Submit the current StaffDeck SOP step result after all required PilotDeck tools have returned successfully.","kind":"structured_output","inputSchema":{"type":"object","properties":{"status":{"type":"string","enum":["completed","awaiting_user","handoff","failed","blocked","waiting_external_task"]},"replyFragment":{"type":"string"},"slotUpdates":{"type":"object","additionalProperties":true,"description":"Persist every known user field using the Required user information keys. Required even for awaiting_user: missing other fields must not discard fields already supplied. Use {} only when no new fields are known."},"taskSummary":{"type":"string"},"structuredResult":{},"nextStepId":{"type":"string"}},"required":["status","replyFragment","slotUpdates"],"additionalProperties":false}}"#).unwrap().as_object().unwrap().clone())
    }
}
impl capability::ToolDefinition for ControlDefinition {
    fn fields(&self) -> &JsObject {
        &self.0
    }
    fn is_read_only(&self, _: &JsValue) -> Result<JsValue, crate::host::HostError> {
        Ok(JsValue::Bool(true))
    }
}
