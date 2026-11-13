//! Whole-batch safe-first scheduling; results retain caller order.
use super::{
    mode_constraints,
    registry::{self, ToolRegistry},
    *,
};
use crate::config::changes::{Operand, object_property, object_prototype_property};
use crate::host::{
    HostFuture,
    modules::{
        capability::{ToolCall, ToolCatalog, ToolDefinition, ToolExecution, ToolInvocationContext},
        permission::PermissionDecision,
        tool_context::ExecutionContext,
    },
};
use crate::js_value;
use std::{future::poll_fn, task::Poll};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SchedulerMode {
    Sequential,
    #[default]
    Concurrent,
}

/// Permission policy is injected explicitly. Lifecycle/plan observers are
/// separate service owners; this layer owns registry resolution and execution.
#[derive(Clone)]
pub struct ToolRuntime {
    pub registry: ToolRegistry,
    permission: Arc<dyn PermissionDecision>,
    pub mode: SchedulerMode,
    /// `ToolRuntimeOptions.eventEmitter`: agent events the runtime itself
    /// raises (`permission_denied`).
    events: Option<crate::host::modules::tool_context::EventSink>,
    /// `ToolRuntimeOptions.lifecycle`: the session's hooks around each call.
    lifecycle: Option<Arc<dyn super::ToolLifecycle>>,
    /// `context.fileUpdateNotifier`: the integration's own save notifier,
    /// which the runtime's `FileChanged` dispatch follows.
    file_update: Option<super::FileUpdateNotifier>,
}
impl ToolRuntime {
    pub fn new(
        registry: ToolRegistry,
        permission: Arc<dyn PermissionDecision>,
        mode: SchedulerMode,
    ) -> Self {
        Self {
            registry,
            permission,
            mode,
            events: None,
            lifecycle: None,
            file_update: None,
        }
    }
    pub fn with_file_update_notifier(mut self, notifier: super::FileUpdateNotifier) -> Self {
        self.file_update = Some(notifier);
        self
    }
    pub fn with_lifecycle(mut self, lifecycle: Arc<dyn super::ToolLifecycle>) -> Self {
        self.lifecycle = Some(lifecycle);
        self
    }
    /// `dispatchLifecycle`: an absent lifecycle is an empty result.
    async fn dispatch_lifecycle(
        &self,
        event: &'static str,
        name: &JsString,
        id: &JsString,
        input: &JsValue,
        context: &ToolRunContext,
        extra: JsObject,
    ) -> Result<Vec<crate::runtime::hooks::HookEffect>, HostError> {
        let Some(lifecycle) = &self.lifecycle else {
            return Ok(Vec::new());
        };
        Ok(lifecycle
            .dispatch(
                event,
                name,
                super::tool_event_payload(name, id, input, extra),
                context,
            )
            .await?
            .map(|result| result.effects)
            .unwrap_or_default())
    }
    pub fn with_event_emitter(
        mut self,
        events: crate::host::modules::tool_context::EventSink,
    ) -> Self {
        self.events = Some(events);
        self
    }
    pub async fn execute_batch(
        &self,
        calls: Vec<ToolCall>,
        context: ToolRunContext,
    ) -> Result<Vec<JsObject>, HostError> {
        let mut concurrent = Vec::new();
        let mut sequential = Vec::new();
        // Predicates are evaluated for the entire batch before any call starts.
        // Single-call batches bypass the predicate, exactly like TS.
        for (index, call) in calls.iter().enumerate() {
            // ConcurrentToolScheduler.ts / hostToolPort.ts:96-108: every
            // concurrencySafe call (including non-read-only ones such as
            // `agent`) runs in the concurrent phase, before the sequential
            // rest, whatever its position in the batch.
            let tool = self.registry.get(&call.name);
            let safe = self.mode == SchedulerMode::Concurrent
                && calls.len() > 1
                && tool
                    .as_ref()
                    .map(|t| t.is_concurrency_safe(&call.input))
                    .transpose()?
                    .is_some_and(|v| truthy(&v));
            if safe {
                concurrent.push(index);
            } else {
                sequential.push(index);
            }
        }
        let mut slots = vec![None; calls.len()];
        // Owned futures, not JoinHandles: host abort, timeout and Drop of the
        // whole batch synchronously destroy every unfinished sibling future.
        let mut pending: Vec<_> = concurrent
            .into_iter()
            .map(|index| {
                let runtime = self.clone();
                let call = calls[index].clone();
                let context = context.clone();
                (
                    index,
                    Some(Box::pin(
                        async move { runtime.execute(call, context).await },
                    )),
                )
            })
            .collect();
        // TS Promise.all rejects on the first error and lets the started
        // siblings finish detached: the rejection is reported at once and the
        // unfinished siblings (which may write, e.g. `agent`) are handed to
        // the runtime instead of being cut short. The pass that sees the error
        // still polls every sibling once, as Promise.all's synchronous map
        // runs each up to its first await.
        let outcome = poll_fn(|cx| {
            let mut unfinished = false;
            let mut first_error = None;
            for (index, slot) in &mut pending {
                let Some(future) = slot else {
                    continue;
                };
                match future.as_mut().poll(cx) {
                    Poll::Ready(Ok(result)) => {
                        slots[*index] = Some(result);
                        *slot = None;
                    }
                    Poll::Ready(Err(error)) => {
                        *slot = None;
                        first_error.get_or_insert(error);
                    }
                    Poll::Pending => unfinished = true,
                }
            }
            if let Some(error) = first_error {
                Poll::Ready(Err(error))
            } else if unfinished {
                Poll::Pending
            } else {
                Poll::Ready(Ok(()))
            }
        })
        .await;
        if let Err(error) = outcome {
            for future in pending.into_iter().filter_map(|(_, slot)| slot) {
                tokio::spawn(future);
            }
            return Err(error);
        }
        for index in sequential {
            slots[index] = Some(self.execute(calls[index].clone(), context.clone()).await?);
        }
        Ok(slots
            .into_iter()
            .map(|v| v.expect("every call has a result slot"))
            .collect())
    }
    /// `runtimeContext.executeTool` (ToolRuntime.ts:47-62): a nested call
    /// runs through this same runtime with the outer call's incoming
    /// context, whose checkpoint maps live in the shared host context.
    fn with_nested_executor(&self, mut context: ToolRunContext) -> ToolRunContext {
        if context.execute_tool.is_none() {
            let runtime = self.clone();
            let base = context.clone();
            context.execute_tool = Some(Arc::new(move |call| {
                let runtime = runtime.clone();
                let nested = base.clone();
                Box::pin(async move { runtime.execute(call, nested).await })
            }));
        }
        context
    }
    pub async fn execute(
        &self,
        call: ToolCall,
        context: ToolRunContext,
    ) -> Result<JsObject, HostError> {
        let context = self.with_nested_executor(context);
        let started = context.now_iso()?;
        let mut tool = self.registry.get(&call.name);
        let mut repaired = None;
        if tool.is_none() {
            let mut targets: Vec<_> = self
                .registry
                .list()?
                .iter()
                .map(|t| (registry::name(t.as_ref()), registry::aliases(t.as_ref())))
                .collect();
            targets.extend(
                self.registry
                    .unavailable_entries()?
                    .into_iter()
                    .map(|e| (e.diagnostic.name, e.aliases)),
            );
            let aliases = configured_aliases(context.fields.get("toolAliases"));
            repaired =
                registry::repair_tool_name(&call.name, &targets, aliases.as_ref())?.map(|r| r.name);
            tool = repaired.as_ref().and_then(|name| self.registry.get(name));
        }
        let name = tool
            .as_ref()
            .map(|t| registry::name(t.as_ref()))
            .unwrap_or_else(|| call.name.clone());
        let fail = |name: &JsString, error: ToolError| {
            create_tool_error_result(&call.id, name, &error, &started, &context)
        };
        if context.is_aborted() {
            return fail(&name, ToolError::aborted());
        }
        if tool.is_some() && self.registry.is_hidden(&name) {
            return fail(
                &call.name,
                ToolError::new(
                    "tool_not_found",
                    named(
                        "Tool ",
                        &call.name,
                        " is deferred for this session. Use search_tools to load it before calling it.",
                    ),
                ),
            );
        }
        let Some(tool) = tool else {
            if let Some(unavailable) = self
                .registry
                .get_unavailable(repaired.as_ref().unwrap_or(&call.name))
            {
                let mut error = ToolError::new(
                    if unavailable.code.equals_str("setup_required") {
                        "setup_required"
                    } else {
                        "tool_unavailable"
                    },
                    unavailable.reason.clone(),
                );
                error.details = Some(object([
                    ("availabilityCode", string(unavailable.code)),
                    ("reason", string(unavailable.reason)),
                ]));
                return fail(&call.name, error);
            }
            return fail(
                &call.name,
                ToolError::new(
                    "tool_not_found",
                    named("Tool ", &call.name, " does not exist."),
                ),
            );
        };
        // Run-mode gates precede schema validation and the permission
        // decision (ToolRuntime.ts:129-150).
        if let Some(message) = mode_constraints::plan_mode_violation(&name, &call.input, &context)?
        {
            return fail(&name, ToolError::new("plan_mode_violation", message));
        }
        if let Some(message) =
            mode_constraints::ask_mode_violation(tool.as_ref(), &name, &call.input, &context)?
        {
            return fail(&name, ToolError::new("ask_mode_violation", message));
        }
        let operation = async {
            let issues = validate_input(
                &call.input,
                tool.fields()
                    .get("inputSchema")
                    .unwrap_or(&JsValue::Undefined),
            )?;
            if !issues.is_empty() {
                return fail(&name, validation_error(&name, issues, &context)?);
            }
            let prompt = name.equals_str("enter_plan_mode")
                || tool.requires_user_interaction(&call.input).ok() == Some(JsValue::Bool(true));
            let can_elicit = name.equals_str("ask_user_question")
                && context.fields.get("canElicit") == Some(&JsValue::Bool(true))
                && context.host.elicitation.is_some();
            if context
                .fields
                .get("permissionContext")
                .and_then(|v| v.get("canPrompt"))
                == Some(&JsValue::Bool(false))
                && prompt
                && !can_elicit
            {
                return fail(
                    &name,
                    ToolError::new(
                        "unsupported_tool",
                        named(
                            "",
                            &name,
                            " requires user interaction, but this session is running with prompts disabled.",
                        ),
                    ),
                );
            }
            // `pre_tool_execute` / `post_tool_execute` (ToolRuntime.ts:189,
            // 395, 424): the host derives a running subagent's tool status
            // from this pair; for a root session's own tools they project to
            // nothing.
            let lifecycle = |kind: &str, success: Option<bool>| {
                if let Some(emit) = &self.events {
                    let identity = |key: &str| {
                        context
                            .fields
                            .get(key)
                            .cloned()
                            .unwrap_or(JsValue::Undefined)
                    };
                    let mut event = object([
                        ("type", kind.into()),
                        ("sessionId", identity("sessionId")),
                        ("turnId", identity("turnId")),
                        ("toolCallId", string(call.id.clone())),
                        ("toolName", string(name.clone())),
                    ]);
                    if let Some(success) = success {
                        event.insert("success".into(), JsValue::Bool(success));
                    }
                    emit(event);
                }
            };
            // `PreToolUse` (ToolRuntime.ts:187-221): a block or a `deny`
            // permission decision refuses the call, an updated input replaces
            // it and is validated again. An `allow` does not skip the
            // permission decision.
            let mut execute_input = call.input.clone();
            let pre = self
                .dispatch_lifecycle(
                    "PreToolUse",
                    &name,
                    &call.id,
                    &execute_input,
                    &context,
                    JsObject::new(),
                )
                .await?;
            lifecycle("pre_tool_execute", None);
            {
                use crate::runtime::hooks::HookEffect;
                let block = pre.iter().find_map(|effect| match effect {
                    HookEffect::Block { reason, .. } => Some(reason.clone()),
                    _ => None,
                });
                let decision = pre.iter().find_map(|effect| match effect {
                    HookEffect::PermissionDecision { behavior, reason } => {
                        Some((behavior.clone(), reason.clone()))
                    }
                    _ => None,
                });
                let deny = decision.filter(|(behavior, _)| behavior.equals_str("deny"));
                if block.is_some() || deny.is_some() {
                    let reason = block
                        .or_else(|| deny.and_then(|(_, reason)| reason))
                        .unwrap_or_else(|| named("PreToolUse hook denied ", &name, "."));
                    return fail(&name, ToolError::new("permission_denied", reason));
                }
                if let Some(updated) = pre.iter().find_map(|effect| match effect {
                    HookEffect::UpdatedToolInput { input } => Some(input.clone()),
                    _ => None,
                }) {
                    execute_input = JsValue::Object(updated);
                    let issues = validate_input(
                        &execute_input,
                        tool.fields()
                            .get("inputSchema")
                            .unwrap_or(&JsValue::Undefined),
                    )?;
                    if !issues.is_empty() {
                        let mut error = validation_error(&name, issues, &context)?;
                        error.message = named(
                            "PreToolUse hook produced invalid input for ",
                            &name,
                            &format!(".\n\n{}", text(&error.message)),
                        );
                        return fail(&name, error);
                    }
                }
            }
            match tool
                .validate_input(execute_input.clone(), context.clone())
                .await
            {
                Err(error) => return fail(&name, error),
                Ok(Some(issues)) => {
                    return fail(&name, validation_error(&name, issues, &context)?);
                }
                Ok(None) => {}
            }
            // `context.planTodo?.blockingMessageFor(name, isReadOnly)`: an
            // approved plan without a todo list blocks non-read-only tools.
            if let Some(plan_todo) = context.host.plan_todo.clone().and_then(|handle| {
                handle
                    .downcast::<crate::runtime::plan_todo::PlanTodoRuntime>()
                    .ok()
            }) {
                let read_only = tool.is_read_only(&execute_input).is_ok_and(|v| truthy(&v));
                if let Some(message) = plan_todo.blocking_message_for(&name, read_only) {
                    return fail(&name, ToolError::failed(message));
                }
            }
            // `auditRecorder.recordPermissionStarted` / `recordPermission` /
            // `recordPermissionFailed` around the decision.
            let audit = context
                .host
                .audit_recorder
                .clone()
                .and_then(|audit| audit.downcast::<super::ToolAudit>().ok());
            let audit_fields = || {
                object([
                    ("operationId", string(call.id.clone())),
                    ("toolCallId", string(call.id.clone())),
                    ("toolName", string(name.clone())),
                    (
                        "mode",
                        context
                            .fields
                            .get("permissionContext")
                            .and_then(|permission| permission.get("mode"))
                            .cloned()
                            .unwrap_or(JsValue::Undefined),
                    ),
                ])
            };
            if let Some(audit) = &audit {
                audit
                    .0
                    .record_permission("permission_started", audit_fields())
                    .await?;
            }
            let definition: Arc<dyn ToolDefinition> = tool.clone();
            let decision = match self
                .permission
                .decide(
                    definition,
                    execute_input.clone(),
                    Arc::new(context.clone()),
                    call.id.clone(),
                )
                .await
            {
                Ok(decision) => decision,
                Err(error) => {
                    if let Some(audit) = &audit {
                        let mut fields = audit_fields();
                        fields.insert("error".into(), string(error.message.clone()));
                        if audit
                            .0
                            .record_permission("permission_failed", fields)
                            .await
                            .is_err()
                        {
                            return Err(ModuleError::new(named(
                                "Permission decision failed and its audit could not be recorded for ",
                                &name,
                                ".",
                            ))
                            .into());
                        }
                    }
                    return Err(error);
                }
            };
            if let Some(audit) = &audit {
                let mut fields = audit_fields();
                for key in ["type", "reason"] {
                    fields.insert(
                        if key == "type" { "decision" } else { key }.into(),
                        decision.get(key).cloned().unwrap_or(JsValue::Undefined),
                    );
                }
                audit
                    .0
                    .record_permission("permission_completed", fields)
                    .await?;
            }
            let kind = decision.get("type").and_then(JsValue::as_string);
            let message = decision
                .get("message")
                .and_then(JsValue::as_string)
                .cloned()
                .unwrap_or_default();
            match kind.map(text).as_deref() {
                Some("deny") => {
                    self.dispatch_lifecycle(
                        "PermissionDenied",
                        &name,
                        &call.id,
                        &execute_input,
                        &context,
                        object([("reason", string(message.clone()))]),
                    )
                    .await?;
                    if let Some(emit) = &self.events {
                        emit(object([
                            ("type", "permission_denied".into()),
                            (
                                "sessionId",
                                context
                                    .fields
                                    .get("sessionId")
                                    .cloned()
                                    .unwrap_or(JsValue::Undefined),
                            ),
                            (
                                "turnId",
                                context
                                    .fields
                                    .get("turnId")
                                    .cloned()
                                    .unwrap_or(JsValue::Undefined),
                            ),
                            ("toolName", string(name.clone())),
                            ("reason", string(message.clone())),
                        ]));
                    }
                    let runtime = decision
                        .get("reason")
                        .and_then(|r| r.get("type"))
                        .and_then(JsValue::as_string)
                        .is_some_and(|s| s.equals_str("runtime"));
                    let prompt = decision
                        .get("reason")
                        .and_then(|r| r.get("message"))
                        .and_then(JsValue::as_string)
                        .is_some_and(|s| text(s).contains("prompt"));
                    return fail(
                        &name,
                        ToolError::new(
                            if runtime && prompt {
                                "permission_required"
                            } else {
                                "permission_denied"
                            },
                            message,
                        ),
                    );
                }
                Some("cancel") => {
                    return fail(&name, ToolError::new("permission_cancelled", message));
                }
                Some("ask") => {
                    let mut error = ToolError::new(
                        "permission_required",
                        named("Permission is required to run ", &name, "."),
                    );
                    error.details = Some(object([(
                        "request",
                        decision
                            .get("request")
                            .cloned()
                            .unwrap_or(JsValue::Undefined),
                    )]));
                    return fail(&name, error);
                }
                // TS executes for "allow" and for any other decision type.
                _ => {}
            }
            let input = decision
                .get("updatedInput")
                .filter(|v| !matches!(v, JsValue::Undefined | JsValue::Null))
                .cloned()
                .unwrap_or_else(|| execute_input.clone());
            let mut execution = context.clone();
            execution
                .fields
                .insert("currentToolCallId".into(), string(call.id.clone()));
            execution
                .fields
                .insert("currentPermissionDecision".into(), decision);
            // ToolRuntime.ts:367-376: the execute context's `progress` fills
            // an empty toolCallId / toolName with this call's id and tool.
            if let Some(progress) = execution.host.progress.clone() {
                let (id, tool_name) = (call.id.clone(), name.clone());
                let mut host = (*execution.host).clone();
                host.progress = Some(Arc::new(move |mut event: JsObject| {
                    let empty = |value: Option<&JsValue>| match value {
                        None | Some(JsValue::Undefined | JsValue::Null) => true,
                        Some(JsValue::String(text)) => text.is_empty(),
                        Some(JsValue::Bool(flag)) => !flag,
                        Some(JsValue::Number(number)) => *number == 0.0 || number.is_nan(),
                        Some(_) => false,
                    };
                    if empty(event.get("toolCallId")) {
                        event.insert("toolCallId".into(), JsValue::String(id.clone()));
                    }
                    if empty(event.get("toolName")) {
                        event.insert("toolName".into(), JsValue::String(tool_name.clone()));
                    }
                    progress(event)
                }));
                execution.host = Arc::new(host);
            }
            execution.file_saved = match &self.lifecycle {
                Some(lifecycle) => Some(file_changed_notifier(
                    lifecycle.clone(),
                    self.file_update.clone(),
                    self.events.clone(),
                    context.clone(),
                )),
                // Without a lifecycle the integration's notifier is used as is.
                None => self.file_update.clone().map(|notifier| {
                    let cwd = string_field_value(&context, "cwd");
                    Arc::new(move |update: super::FileUpdate| notifier(update, cwd.clone()))
                        as super::FileSavedNotifier
                }),
            };
            let execute_input = input.clone();
            match tool.execute(input, execution).await {
                Err(error) => {
                    self.dispatch_lifecycle(
                        "PostToolUseFailure",
                        &name,
                        &call.id,
                        &execute_input,
                        &context,
                        object([
                            ("error", string(error.message.clone())),
                            ("isInterrupt", JsValue::Bool(error.code == "tool_aborted")),
                        ]),
                    )
                    .await?;
                    lifecycle("post_tool_execute", Some(false));
                    fail(&name, error)
                }
                Ok(output) => {
                    let post = self
                        .dispatch_lifecycle(
                            "PostToolUse",
                            &name,
                            &call.id,
                            &execute_input,
                            &context,
                            object([(
                                "toolResponse",
                                output
                                    .data
                                    .clone()
                                    .unwrap_or_else(|| JsValue::Array(output.content.clone())),
                            )]),
                        )
                        .await?;
                    lifecycle("post_tool_execute", Some(true));
                    let mut metadata = output.metadata;
                    let max_bytes = tool
                        .fields()
                        .get("maxResultBytes")
                        .filter(|v| !matches!(v, JsValue::Null | JsValue::Undefined))
                        .or_else(|| context.fields.get("maxResultBytes"))
                        .and_then(JsValue::as_number);
                    // A throw here is caught by TS's execute try block and
                    // becomes an ordinary execution-failure result.
                    let limit = match preview_metadata(&output.content, max_bytes) {
                        Ok(limit) => limit,
                        Err(error) => return fail(&name, ToolError::failed(error.message)),
                    };
                    if let Some(limit) = limit {
                        metadata
                            .get_or_insert_with(JsObject::new)
                            .insert("previewLimit".into(), JsValue::Object(limit));
                    }
                    if let Some(lifecycle) = lifecycle_metadata(&post) {
                        metadata
                            .get_or_insert_with(JsObject::new)
                            .insert("lifecycle".into(), lifecycle);
                    }
                    let completed = context.now_iso()?;
                    // `context.planTodo?.markToolProgressChanged(name)` after a
                    // non-read-only tool other than `todo_write` succeeds.
                    if !name.equals_str("todo_write")
                        && !tool.is_read_only(&execute_input).is_ok_and(|v| truthy(&v))
                        && let Some(plan_todo) = context.host.plan_todo.clone().and_then(|handle| {
                            handle
                                .downcast::<crate::runtime::plan_todo::PlanTodoRuntime>()
                                .ok()
                        })
                    {
                        let turn = context
                            .fields
                            .get("turnId")
                            .and_then(JsValue::as_string)
                            .cloned()
                            .unwrap_or_default();
                        if let Err(error) = plan_todo.mark_tool_progress_changed(&name, &turn).await
                        {
                            return fail(&name, ToolError::failed(error.message));
                        }
                    }
                    Ok(object([
                        ("type", "success".into()),
                        ("toolCallId", string(call.id.clone())),
                        ("toolName", string(name.clone())),
                        ("content", JsValue::Array(output.content)),
                        (
                            "supplementalMessages",
                            output
                                .supplemental_messages
                                .map(JsValue::Array)
                                .unwrap_or(JsValue::Undefined),
                        ),
                        ("data", output.data.unwrap_or(JsValue::Undefined)),
                        (
                            "metadata",
                            metadata.map(JsValue::Object).unwrap_or(JsValue::Undefined),
                        ),
                        ("startedAt", string(started.clone())),
                        ("completedAt", string(completed)),
                    ]))
                }
            }
        };
        // Like TS, abort is cooperative: a started tool observes the signal and
        // settles itself. The batch future owns it, so dropping the batch (host
        // operation deadline) still destroys it without detached work.
        operation.await
    }
}
impl ToolCatalog for ToolRuntime {
    fn list(&self) -> HostFuture<Vec<Arc<dyn ToolDefinition>>> {
        let registry = self.registry.clone();
        Box::pin(async move {
            Ok(registry
                .list()?
                .into_iter()
                .map(|t| t as Arc<dyn ToolDefinition>)
                .collect())
        })
    }
}
impl ToolExecution for ToolRuntime {
    fn execute_all(
        &self,
        calls: Vec<ToolCall>,
        context: ToolInvocationContext,
    ) -> HostFuture<Vec<JsObject>> {
        let runtime = self.clone();
        Box::pin(async move {
            let host = context
                .runtime
                .downcast::<ToolRuntimeContext>()
                .map_err(|_| {
                    ModuleError::new("Tool runtime requires a host ToolRuntimeContext.")
                })?;
            let execution = context
                .execution
                .downcast::<ExecutionContext>()
                .map_err(|_| ModuleError::new("Tool runtime requires a host ExecutionContext."))?;
            let mut context = ToolRunContext::new(host);
            with_operation_deadline_budget(&mut context, &execution.fields)?;
            runtime.execute_batch(calls, context).await
        })
    }
}

/// `createFileUpdateNotifier` / `dispatchFileChanged` (ToolRuntime.ts:513-604):
/// a committed save dispatches `FileChanged`, matched on the absolute path.
/// Its effects are observations only: a block and every hook error become
/// `warning` agent events, and the tool result is unchanged.
fn file_changed_notifier(
    lifecycle: Arc<dyn super::ToolLifecycle>,
    upstream: Option<super::FileUpdateNotifier>,
    events: Option<crate::host::modules::tool_context::EventSink>,
    context: ToolRunContext,
) -> super::FileSavedNotifier {
    Arc::new(move |update: super::FileUpdate| {
        let (lifecycle, upstream, events, context) = (
            lifecycle.clone(),
            upstream.clone(),
            events.clone(),
            context.clone(),
        );
        Box::pin(async move {
            // `await upstream?.didSave?.(update)` first.
            if let Some(upstream) = &upstream {
                upstream(update.clone(), string_field_value(&context, "cwd")).await?;
            }
            let change_kind = if update.created { "create" } else { "update" };
            let report = |code: &str, message: JsString, hook_name: Option<JsString>| {
                let Some(emit) = &events else {
                    return;
                };
                let mut metadata = object([
                    ("absolutePath", string(update.absolute_path.clone())),
                    ("relativePath", string(update.relative_path.clone())),
                    ("changeKind", change_kind.into()),
                ]);
                if let Some(name) = hook_name {
                    metadata.insert("hookName".into(), string(name));
                }
                let identity = |key: &str| {
                    context
                        .fields
                        .get(key)
                        .cloned()
                        .unwrap_or(JsValue::Undefined)
                };
                emit(object([
                    ("type", "warning".into()),
                    ("sessionId", identity("sessionId")),
                    ("turnId", identity("turnId")),
                    ("code", code.into()),
                    ("message", string(message)),
                    ("metadata", JsValue::Object(metadata)),
                ]));
            };
            let payload = object([
                ("absolutePath", string(update.absolute_path.clone())),
                ("relativePath", string(update.relative_path.clone())),
                ("changeKind", change_kind.into()),
            ]);
            match lifecycle
                .dispatch("FileChanged", &update.absolute_path, payload, &context)
                .await
            {
                Ok(Some(result)) => {
                    if let Some(reason) = result.effects.iter().find_map(|effect| match effect {
                        crate::runtime::hooks::HookEffect::Block { reason, .. } => {
                            Some(reason.clone())
                        }
                        _ => None,
                    }) {
                        report("file_changed_hook_blocked", reason, None);
                    }
                    for error in result
                        .blocking_errors
                        .iter()
                        .chain(result.non_blocking_errors.iter())
                    {
                        report(
                            "file_changed_hook_error",
                            error.message.clone(),
                            error.hook_name.clone(),
                        );
                    }
                }
                Ok(None) => {}
                Err(error) => report(
                    "file_changed_hook_error",
                    error.message.as_str().into(),
                    None,
                ),
            }
            Ok(())
        })
    })
}

/// `lifecycleMetadata` (ToolRuntime.ts:773-787): what a `PostToolUse` hook
/// tells the loop through the result's metadata.
fn lifecycle_metadata(effects: &[crate::runtime::hooks::HookEffect]) -> Option<JsValue> {
    use crate::runtime::hooks::HookEffect;
    let blocking = effects.iter().find_map(|effect| match effect {
        HookEffect::Block {
            reason,
            stop_reason,
        } => Some((reason.clone(), stop_reason.clone())),
        _ => None,
    });
    let additional: Vec<JsValue> = effects
        .iter()
        .filter_map(|effect| match effect {
            HookEffect::AdditionalContext { content, .. } => Some(JsValue::String(content.clone())),
            _ => None,
        })
        .collect();
    let updated = effects.iter().find_map(|effect| match effect {
        HookEffect::UpdatedMcpToolOutput { output } => Some(output.clone()),
        _ => None,
    });
    if blocking.is_none() && additional.is_empty() && updated.is_none() {
        return None;
    }
    let blocked = match blocking {
        Some((reason, stop_reason)) => JsValue::Object(object([
            ("reason", JsValue::String(reason)),
            (
                "stopReason",
                stop_reason.map_or(JsValue::Undefined, JsValue::String),
            ),
        ])),
        None => JsValue::Undefined,
    };
    Some(JsValue::Object(object([
        ("blocked", blocked),
        ("additionalContext", JsValue::Array(additional)),
        (
            "updatedMcpToolOutput",
            updated.unwrap_or(JsValue::Undefined),
        ),
    ])))
}

/// repairToolName.ts: a falsy `toolAliases` is ignored; otherwise it is read
/// with `Object.entries`, which yields index keys for strings (UTF-16 code
/// units) and arrays, and nothing for other primitives.
fn configured_aliases(value: Option<&JsValue>) -> Option<JsObject> {
    let value = value.filter(|v| truthy(v))?;
    let mut aliases = JsObject::new();
    match value {
        JsValue::Object(o) => return Some(o.clone()),
        JsValue::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                aliases.insert(i.to_string().into(), item.clone());
            }
        }
        JsValue::String(s) => {
            for (i, unit) in s.units().iter().enumerate() {
                aliases.insert(
                    i.to_string().into(),
                    JsValue::String(JsString::from_utf16(vec![*unit])),
                );
            }
        }
        _ => {}
    }
    Some(aliases)
}

/// A plain object reached by validateObject. Property reads reuse the config
/// model (`config::changes`): own keys, the explicit prototype chain (an
/// explicit `null` prototype ends it without Object.prototype; an array
/// prototype adds `length`, indices and Array.prototype), then Object.prototype.
#[derive(Clone, Copy)]
enum Plain<'a> {
    Own(&'a JsObject),
    ObjectPrototype,
}
impl<'a> Plain<'a> {
    /// `key in object`.
    fn has(self, key: &JsString) -> bool {
        self.get(key).is_some()
    }
    /// `object[key]` when `key in object`; `Function` is an inherited method:
    /// it equals no enum member, matches no JSON type and is not a plain object.
    fn get(self, key: &JsString) -> Option<Operand<'a>> {
        match self {
            Plain::Own(o) => object_property(o, key),
            Plain::ObjectPrototype => object_prototype_property(key),
        }
    }
    /// `Object.keys`: integer keys ascending, then insertion order.
    fn keys(self) -> Vec<&'a JsString> {
        match self {
            Plain::Own(o) => js_entries(o).into_iter().map(|(k, _)| k).collect(),
            Plain::ObjectPrototype => vec![],
        }
    }
}
/// `schema.<key>` as a normal JS property read: own keys, then the explicit
/// prototype chain (schemas built programmatically or by config merges).
fn schema_field<'a>(schema: &'a JsValue, key: &str) -> Option<&'a JsValue> {
    match schema {
        JsValue::Object(o) => match object_property(o, &key.into())? {
            Operand::Value(v) => Some(v),
            _ => None,
        },
        _ => None,
    }
}
/// Own enumerable entries in JS `Object.keys`/`Object.entries` order.
fn js_entries(object: &JsObject) -> Vec<&(JsString, JsValue)> {
    let mut entries: Vec<_> = object.entries().iter().collect();
    entries.sort_by_key(|(key, _)| registry::array_index(key).map_or((1, 0), |n| (0, n)));
    entries
}
/// `String(value)` via the shared bounded conversion (`crate::js_value`):
/// nesting past `MAX_ARRAY_DEPTH` is V8's RangeError, raised as a host error
/// the way the TS call rejects, never a Rust stack overflow.
fn js_string(value: &JsValue) -> Result<JsString, HostError> {
    js_value::to_string_units(value)
        .map(JsString::from_utf16)
        .map_err(range_error)
}
fn js_display(value: &JsValue) -> Result<String, HostError> {
    js_value::display(value).map_err(range_error)
}
fn range_error(_: js_value::StackOverflow) -> HostError {
    ModuleError::new(js_value::StackOverflow::MESSAGE).into()
}
fn type_error(message: String) -> HostError {
    ModuleError::new(message).into()
}
fn not_iterable(value: &JsValue) -> HostError {
    let kind = match value {
        JsValue::Number(n) => format!("number {}", js_value::number_string(*n)),
        JsValue::Bool(b) => format!("boolean {b}"),
        _ => "object".into(),
    };
    type_error(format!(
        "{kind} is not iterable (cannot read property Symbol(Symbol.iterator))"
    ))
}

/// validateToolInput.ts, including the TypeErrors it throws for schema shapes
/// that are not JSON Schema (null schema, non-array enum, non-iterable
/// `required`, primitive `properties`).
fn validate_input(value: &JsValue, schema: &JsValue) -> Result<Vec<JsObject>, HostError> {
    fn issue(out: &mut Vec<JsObject>, path: &str, code: &str, message: String) {
        out.push(object([
            ("path", path.into()),
            ("code", code.into()),
            ("message", string(message)),
        ]));
    }
    /// matchesType: `type.some(...)` recursing into nested type arrays,
    /// depth-first like TS, iterative. Bounded one level above the shared
    /// String() limit: V8 survives this recursion deeper than String()
    /// (measured 3000 OK, 20000 RangeError), so the outer `type` array plus a
    /// member at the String() limit must still reach the description join.
    fn matches(value: Operand, kind: &JsValue) -> Result<bool, HostError> {
        let mut frames: Vec<std::slice::Iter<'_, JsValue>> = Vec::new();
        let mut next = Some(kind);
        loop {
            match next.take() {
                Some(JsValue::Array(kinds)) => {
                    if frames.len() > js_value::MAX_ARRAY_DEPTH {
                        return Err(range_error(js_value::StackOverflow));
                    }
                    frames.push(kinds.iter());
                }
                Some(kind) if matches_one(value, kind) => return Ok(true),
                _ => {}
            }
            let Some(kinds) = frames.last_mut() else {
                return Ok(false);
            };
            match kinds.next() {
                Some(kind) => next = Some(kind),
                None => {
                    frames.pop();
                }
            }
        }
    }
    fn matches_one(value: Operand, kind: &JsValue) -> bool {
        let number;
        let value = match value {
            Operand::Value(v) => v,
            Operand::Number(n) => {
                number = JsValue::Number(n);
                &number
            }
            Operand::Function(_) => return false,
            Operand::ObjectPrototype => {
                return kind.as_string().is_some_and(|s| s.equals_str("object"));
            }
        };
        match kind.as_string().map(text).as_deref() {
            Some("null") => matches!(value, JsValue::Null),
            Some("object") => matches!(value, JsValue::Object(_)),
            Some("array") => matches!(value, JsValue::Array(_)),
            Some("string") => value.as_string().is_some(),
            Some("boolean") => value.as_bool().is_some(),
            Some("number") => value.as_number().is_some_and(f64::is_finite),
            Some("integer") => value
                .as_number()
                .is_some_and(|n| n.is_finite() && n.fract() == 0.0),
            _ => false,
        }
    }
    /// `schema.properties ?? {}` as validateObject uses it.
    enum Properties<'a> {
        Object(&'a JsObject),
        Array(&'a [JsValue]),
        Primitive(&'a JsValue),
    }
    impl Properties<'_> {
        fn has(&self, key: &JsString) -> Result<bool, HostError> {
            match self {
                Properties::Object(o) => Ok(Plain::Own(o).has(key)),
                Properties::Array(a) => Ok(registry::array_index(key)
                    .is_some_and(|n| (n as usize) < a.len())
                    || key.equals_str("length")
                    || crate::config::changes::is_array_prototype_method(key)
                    || crate::config::builtin_property(key)),
                Properties::Primitive(v) => Err(type_error(format!(
                    "Cannot use 'in' operator to search for '{}' in {}",
                    text(key),
                    text(&js_string(v)?)
                ))),
            }
        }
        fn entries(&self) -> Vec<(JsString, JsValue)> {
            match self {
                Properties::Object(o) => js_entries(o).into_iter().cloned().collect(),
                Properties::Array(a) => a
                    .iter()
                    .enumerate()
                    .map(|(i, v)| (i.to_string().into(), v.clone()))
                    .collect(),
                Properties::Primitive(JsValue::String(s)) => s
                    .units()
                    .iter()
                    .enumerate()
                    .map(|(i, u)| {
                        (
                            i.to_string().into(),
                            JsValue::String(JsString::from_utf16(vec![*u])),
                        )
                    })
                    .collect(),
                Properties::Primitive(_) => vec![],
            }
        }
    }
    /// `for (const key of schema.required ?? [])`, keys coerced by `in`.
    fn required(schema: &JsValue) -> Result<Vec<JsString>, HostError> {
        match schema_field(schema, "required") {
            None | Some(JsValue::Null | JsValue::Undefined) => Ok(vec![]),
            Some(JsValue::Array(keys)) => keys.iter().map(js_string).collect(),
            Some(JsValue::String(s)) => {
                // String iteration yields code points; lone surrogates stand alone.
                let units = s.units();
                let mut keys = vec![];
                let mut i = 0;
                while i < units.len() {
                    let pair = (0xD800..0xDC00).contains(&units[i])
                        && units
                            .get(i + 1)
                            .is_some_and(|u| (0xDC00..0xE000).contains(u));
                    let width = if pair { 2 } else { 1 };
                    keys.push(JsString::from_utf16(units[i..i + width].to_vec()));
                    i += width;
                }
                Ok(keys)
            }
            Some(other) => Err(not_iterable(other)),
        }
    }
    fn walk(
        value: Operand,
        schema: &JsValue,
        path: &str,
        out: &mut Vec<JsObject>,
    ) -> Result<(), HostError> {
        match schema {
            JsValue::Null => {
                return Err(type_error(
                    "Cannot read properties of null (reading 'enum')".into(),
                ));
            }
            JsValue::Undefined => {
                return Err(type_error(
                    "Cannot read properties of undefined (reading 'enum')".into(),
                ));
            }
            _ => {}
        }
        match schema_field(schema, "enum") {
            Some(JsValue::Array(enumeration)) => {
                let equal = |v: &JsValue| match (value, v) {
                    (Operand::Value(JsValue::Number(a)), JsValue::Number(b)) => {
                        a.to_bits() == b.to_bits() || a.is_nan() && b.is_nan()
                    }
                    (Operand::Number(a), JsValue::Number(b)) => a.to_bits() == b.to_bits(),
                    (
                        Operand::Function(_)
                        | Operand::ObjectPrototype
                        | Operand::Number(_)
                        | Operand::Value(JsValue::Object(_) | JsValue::Array(_)),
                        _,
                    ) => false,
                    (Operand::Value(value), _) => v == value,
                };
                if !enumeration.iter().any(equal) {
                    issue(
                        out,
                        path,
                        "invalid_enum",
                        format!(
                            "{path} must be one of {}.",
                            enumeration
                                .iter()
                                .map(js_display)
                                .collect::<Result<Vec<_>, _>>()?
                                .join(", ")
                        ),
                    );
                    return Ok(());
                }
            }
            Some(enumeration) if truthy(enumeration) => {
                return Err(type_error("schema.enum.some is not a function".into()));
            }
            _ => {}
        }
        let kind = schema_field(schema, "type");
        if let Some(kind) = kind
            && !matches(value, kind)?
        {
            let description = match kind {
                // Array#join prints null and undefined members as "".
                JsValue::Array(kinds) => kinds
                    .iter()
                    .map(|k| match k {
                        JsValue::Null | JsValue::Undefined => Ok(String::new()),
                        k => js_display(k),
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .join(" or "),
                kind => js_display(kind)?,
            };
            issue(
                out,
                path,
                "invalid_type",
                format!("{path} must be {description}."),
            );
            return Ok(());
        }
        let effective = match kind {
            Some(JsValue::Array(kinds)) => kinds
                .iter()
                .find(|k| k.as_string().is_none_or(|s| !s.equals_str("null"))),
            kind => kind,
        };
        let plain = match value {
            Operand::Value(JsValue::Object(o)) => Some(Plain::Own(o)),
            Operand::ObjectPrototype => Some(Plain::ObjectPrototype),
            _ => None,
        };
        if (effective.is_none()
            || effective
                .and_then(JsValue::as_string)
                .is_some_and(|s| s.equals_str("object")))
            && let Some(value) = plain
        {
            let empty = JsObject::new();
            let properties = match schema_field(schema, "properties") {
                None | Some(JsValue::Null | JsValue::Undefined) => Properties::Object(&empty),
                Some(JsValue::Object(o)) => Properties::Object(o),
                Some(JsValue::Array(a)) => Properties::Array(a),
                Some(other) => Properties::Primitive(other),
            };
            for key in required(schema)? {
                if !value.has(&key) {
                    let path = format!("{path}.{}", text(&key));
                    issue(out, &path, "required", format!("{path} is required."));
                }
            }
            if schema_field(schema, "additionalProperties") == Some(&JsValue::Bool(false)) {
                for key in value.keys() {
                    if !properties.has(key)? {
                        let path = format!("{path}.{}", text(key));
                        issue(
                            out,
                            &path,
                            "unknown_property",
                            format!("{path} is not allowed."),
                        );
                    }
                }
            }
            for (key, schema) in properties.entries() {
                if let Some(property) = value.get(&key) {
                    walk(property, &schema, &format!("{path}.{}", text(&key)), out)?;
                }
            }
        }
        if effective
            .and_then(JsValue::as_string)
            .is_some_and(|s| s.equals_str("array"))
            && let (Operand::Value(JsValue::Array(values)), Some(schema)) =
                (value, schema_field(schema, "items").filter(|s| truthy(s)))
        {
            for (i, value) in values.iter().enumerate() {
                walk(Operand::Value(value), schema, &format!("{path}[{i}]"), out)?;
            }
        }
        Ok(())
    }
    let mut issues = vec![];
    walk(Operand::Value(value), schema, "$", &mut issues)?;
    Ok(issues)
}
fn validation_error(
    name: &JsString,
    issues: Vec<JsObject>,
    context: &ToolRunContext,
) -> Result<ToolError, HostError> {
    let tool = text(name);
    let mut parts = vec![];
    for issue in &issues {
        let path = text(&string_field(issue, "path"));
        let param = path
            .strip_prefix('$')
            .map_or(path.as_str(), |rest| rest.strip_prefix('.').unwrap_or(rest));
        let message = text(&string_field(issue, "message"));
        parts.push(match text(&string_field(issue, "code")).as_str() {
            "required" => format!("The required parameter `{param}` is missing"),
            "invalid_type" => format!("The parameter `{param}` has an invalid type: {message}"),
            "unknown_property" => format!("An unexpected parameter `{param}` was provided"),
            "invalid_enum" => format!("The parameter `{param}` has an invalid value: {message}"),
            _ => message,
        });
    }
    if parts.is_empty() {
        return Ok(ToolError::new(
            "invalid_tool_input",
            format!("Tool {tool} input is invalid."),
        )
        .with_details(issues));
    }
    let mut message = format!(
        "{tool} failed due to the following {}:\n{}",
        if parts.len() > 1 { "issues" } else { "issue" },
        parts.join("\n")
    );
    let required = issues
        .iter()
        .any(|i| string_field(i, "code").equals_str("required"));
    if required && ["write_file", "edit_file", "edit_notebook", "bash"].contains(&tool.as_str()) {
        if context.fields.get("outputTruncated").is_some_and(truthy) {
            let tokens = context
                .fields
                .get("maxOutputTokens")
                .filter(|v| truthy(v))
                .map(js_display)
                .transpose()?
                .map(|v| format!(" (current max_output_tokens: {v})"))
                .unwrap_or_default();
            message += &format!(
                "\n\nThis was caused by your output being truncated (output token limit reached{tokens}). Keep each tool call's arguments well within the output token budget."
            );
        } else {
            message += "\n\nPlease ensure all required parameters are provided in the tool call.";
        }
    }
    if tool == "write_file"
        && issues.iter().any(|i| {
            string_field(i, "code").equals_str("required")
                && text(&string_field(i, "path")).contains("content")
        })
    {
        message += "\n\nHint: For large files, recover by creating a smaller but valid draft workspace file first. Use write_file with a complete content field that fits comfortably within the output budget. After the draft exists, read it with read_file and extend or patch it using small focused write_file/edit_file calls. Do not use shell heredocs or paths outside the workspace.";
    }
    Ok(ToolError::new("invalid_tool_input", message).with_details(issues))
}
pub(crate) fn content_text(v: &JsValue) -> Result<String, HostError> {
    let field = |key| {
        v.get(key)
            .map_or_else(|| Ok("undefined".into()), js_display)
    };
    let optional = |key| {
        v.get(key)
            .filter(|v| truthy(v))
            .map(|v| js_display(v).map(|v| format!(", {v}")))
            .transpose()
            .map(Option::unwrap_or_default)
    };
    Ok(
        match v
            .get("type")
            .and_then(JsValue::as_string)
            .map(text)
            .as_deref()
        {
            Some("text") => field("text")?,
            Some("json") => {
                pilotdeck_protocol::stringify_json(v.get("value").unwrap_or(&JsValue::Undefined))
                    .ok()
                    .flatten()
                    .unwrap_or_default()
            }
            Some("file") => format!(
                "[File: {}{}{}]",
                field("path")?,
                optional("mimeType")?,
                optional("description")?
            ),
            _ => String::new(),
        },
    )
}
/// applyResultSizeLimit's metadata. `Err` is the RangeError a too-deep
/// String() raises there; TS throws it inside the execute try block.
fn preview_metadata(content: &[JsValue], max: Option<f64>) -> Result<Option<JsObject>, HostError> {
    let Some(max) = max else {
        return Ok(None);
    };
    // TS `applyResultSizeLimit` tests `maxBytes < 0` (false for NaN) and then
    // reaches head/tail truncation with NaN budgets. The runtime only consumes
    // `.metadata`, so NaN reports a truncated body sized from the separator.
    if max < 0.0 {
        return Ok(None);
    }
    let binary = |v: &JsValue| {
        v.get("type")
            .and_then(JsValue::as_string)
            .is_some_and(|s| s.equals_str("image") || s.equals_str("pdf"))
    };
    let mut original = 0;
    for v in content {
        original += if binary(v) {
            v.get("data")
                .and_then(JsValue::as_string)
                .map_or(0, |s| s.units().len())
        } else {
            content_text(v)?.len()
        };
    }
    if content.iter().any(binary) {
        return Ok(Some(object([(
            "originalBytes",
            JsValue::Number(original as f64),
        )])));
    }
    if max.is_nan() {
        let suffix = "\n[Tool output truncated: head and tail shown.]";
        let separator = "\n... [middle omitted] ...\n";
        return Ok(Some(object([
            ("truncated", JsValue::Bool(true)),
            ("originalBytes", JsValue::Number(original as f64)),
            (
                "returnedBytes",
                JsValue::Number((separator.len() + original + suffix.len()) as f64),
            ),
        ])));
    }
    if original as f64 <= max {
        return Ok(None);
    }
    let suffix = "\n[Tool output truncated: head and tail shown.]";
    let budget = (max - suffix.len() as f64).max(0.0) as usize;
    let value = content
        .iter()
        .map(content_text)
        .collect::<Result<Vec<_>, _>>()?
        .join("\n");
    let prefix = |budget: usize| {
        let mut end = budget.min(value.len());
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        &value[..end]
    };
    let returned = if value.len() <= budget {
        value.len()
    } else if budget <= 64 {
        prefix(budget).len()
    } else {
        let separator = "\n... [middle omitted] ...\n";
        let budget = budget.saturating_sub(separator.len());
        let mut start = value.len().saturating_sub(budget / 2);
        while !value.is_char_boundary(start) {
            start += 1;
        }
        prefix(budget.div_ceil(2)).len() + separator.len() + value.len() - start
    } + suffix.len();
    Ok(Some(object([
        ("truncated", JsValue::Bool(true)),
        ("originalBytes", JsValue::Number(original as f64)),
        ("returnedBytes", JsValue::Number(returned as f64)),
    ])))
}

/// A string field of the run context (`context.cwd`), empty when absent.
fn string_field_value(context: &ToolRunContext, key: &str) -> JsString {
    context
        .fields
        .get(key)
        .and_then(JsValue::as_string)
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod preview_tests {
    use super::*;

    fn text_content(value: &str) -> Vec<JsValue> {
        vec![JsValue::Object(object([
            ("type", JsValue::from("text")),
            ("text", JsValue::from(value)),
        ]))]
    }

    #[test]
    fn preview_metadata_negative_and_nan_limits_match_ts() {
        let content = text_content("abcdefghijklmnopqrstuvwxyz");
        assert!(
            preview_metadata(&content, Some(-1.0)).unwrap().is_none(),
            "maxBytes < 0 disables the limit, including the context fallback"
        );
        let metadata = preview_metadata(&content, Some(f64::NAN))
            .unwrap()
            .expect("NaN still runs head/tail truncation in TS");
        assert_eq!(
            pilotdeck_protocol::stringify_json(&JsValue::Object(metadata))
                .unwrap()
                .unwrap(),
            r#"{"truncated":true,"originalBytes":26,"returnedBytes":98}"#
        );
        let image = vec![JsValue::Object(object([
            ("type", JsValue::from("image")),
            ("data", JsValue::from("AAAA")),
        ]))];
        let metadata = preview_metadata(&image, Some(f64::NAN))
            .unwrap()
            .expect("binary metadata");
        assert_eq!(
            pilotdeck_protocol::stringify_json(&JsValue::Object(metadata))
                .unwrap()
                .unwrap(),
            r#"{"originalBytes":4}"#
        );
    }
}
