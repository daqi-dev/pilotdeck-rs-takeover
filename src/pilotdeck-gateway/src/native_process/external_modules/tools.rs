//! Configured HTTP capability execution and frozen external/host ownership.
//!
//! The selected raw port belongs inside permission and durable wrappers. It
//! never interprets an external descriptor as host authority.
use super::http::{HttpModuleClient, HttpModuleError, ModuleCallInput};
use crate::{
    host::{
        HostError, HostFuture, ModuleError,
        modules::{
            capability::{
                ToolCall, ToolCatalog, ToolDefinition, ToolExecution, ToolInvocationContext,
            },
            tool_context::{ExecutionContext, ToolRuntimeContext},
        },
    },
    runtime::network::Cancellation,
};
use pilotdeck_protocol::{JsObject, JsString, JsValue, admission::ExecutionIdentity};
use std::{
    future::poll_fn,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::Poll,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone)]
struct StaticTool {
    fields: JsObject,
    read_only: bool,
    safe: bool,
    interaction: bool,
}
impl StaticTool {
    fn new(descriptor: JsObject) -> Self {
        let mut fields = JsObject::new();
        for key in ["name", "description", "inputSchema"] {
            if let Some(value) = descriptor.get(key) {
                fields.insert(key.into(), value.clone());
            }
        }
        fields.insert(
            "kind".into(),
            descriptor
                .get("kind")
                .cloned()
                .filter(|v| !matches!(v, JsValue::Null | JsValue::Undefined))
                .unwrap_or_else(|| "custom".into()),
        );
        Self {
            fields,
            read_only: descriptor.get("readOnly").and_then(JsValue::as_bool) == Some(true),
            safe: descriptor.get("concurrencySafe").and_then(JsValue::as_bool) == Some(true),
            interaction: descriptor
                .get("requiresUserInteraction")
                .and_then(JsValue::as_bool)
                == Some(true),
        }
    }
}
impl ToolDefinition for StaticTool {
    fn fields(&self) -> &JsObject {
        &self.fields
    }
    fn is_read_only(&self, _: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(self.read_only))
    }
    fn is_concurrency_safe(&self, _: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(self.safe))
    }
    fn requires_user_interaction(&self, _: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(self.interaction))
    }
}

/// One turn's raw external port. The catalog is configuration data, not a
/// request to execute local implementations of the advertised names.
#[derive(Clone)]
pub struct ExternalToolExecution {
    client: Arc<HttpModuleClient>,
    tools: Vec<Arc<dyn ToolDefinition>>,
    identity: ExecutionIdentity,
    signal: Cancellation,
    sequence: Arc<AtomicU64>,
}
impl ExternalToolExecution {
    pub fn new(
        client: Arc<HttpModuleClient>,
        tools: Vec<JsObject>,
        identity: ExecutionIdentity,
        signal: Cancellation,
        sequence: Arc<AtomicU64>,
    ) -> Self {
        Self {
            client,
            tools: tools
                .into_iter()
                .map(|tool| Arc::new(StaticTool::new(tool)) as Arc<dyn ToolDefinition>)
                .collect(),
            identity,
            signal,
            sequence,
        }
    }
    pub fn catalog(&self) -> Vec<Arc<dyn ToolDefinition>> {
        self.tools.clone()
    }
    fn request(&self, payload: JsValue, execution: &JsObject, batch: bool) -> ModuleCallInput {
        let field = |key| execution.get(key).and_then(JsValue::as_string).cloned();
        ModuleCallInput {
            run_id: field("runId").unwrap_or_else(|| self.identity.run_id.clone()),
            operation_id: field("operationId")
                .unwrap_or_else(|| self.identity.operation_id.clone()),
            request_id: format!(
                "tool-{}{}",
                if batch { "batch-" } else { "" },
                self.sequence.fetch_add(1, Ordering::Relaxed) + 1
            )
            .into(),
            module: "capability".into(),
            payload,
            idempotency_key: field("idempotencyKey"),
            abort: Some(self.signal.clone()),
        }
    }
    async fn execute_one(
        &self,
        call: ToolCall,
        context: &JsObject,
        execution: &JsObject,
    ) -> Result<JsObject, HostError> {
        if self.signal.is_cancelled() {
            return Err(cancelled());
        }
        let payload = object([
            ("name", JsValue::String(call.name.clone())),
            ("arguments", call.input.clone()),
            ("toolCallId", JsValue::String(call.id.clone())),
            ("context", JsValue::Object(context.clone())),
            ("execution", JsValue::Object(execution.clone())),
        ]);
        // A single execute payload deliberately has no operation field.
        let response = match self
            .client
            .call(self.request(JsValue::Object(payload), execution, false))
            .await
        {
            Ok(response) => response,
            Err(error) => {
                return if self.signal.is_cancelled() || error.code.equals_str("MODULE_ABORTED") {
                    Err(cancelled())
                } else {
                    Ok(transport_failure(&call, error))
                };
            }
        };
        if self.signal.is_cancelled() || response_cancelled(&response) {
            return Err(cancelled());
        }
        reject_unknown(&response)?;
        if response.get("ok").and_then(JsValue::as_bool) == Some(true)
            && let Some(payload) = response.get("payload").and_then(JsValue::as_object)
            && payload.get("type").is_some()
        {
            return Ok(payload.clone());
        }
        Ok(module_failure(&call, &response))
    }
    async fn execute_batch(
        &self,
        calls: &[ToolCall],
        context: &JsObject,
        execution: &JsObject,
    ) -> Result<Vec<JsObject>, HostError> {
        let payload = object([
            ("operation", "execute_batch".into()),
            (
                "calls",
                JsValue::Array(
                    calls
                        .iter()
                        .map(|call| {
                            JsValue::Object(object([
                                ("name", JsValue::String(call.name.clone())),
                                ("arguments", call.input.clone()),
                                ("toolCallId", JsValue::String(call.id.clone())),
                            ]))
                        })
                        .collect(),
                ),
            ),
            ("context", JsValue::Object(context.clone())),
            ("execution", JsValue::Object(execution.clone())),
        ]);
        let response = match self
            .client
            .call(self.request(JsValue::Object(payload), execution, true))
            .await
        {
            Ok(response) => response,
            Err(error) => {
                return if self.signal.is_cancelled() || error.code.equals_str("MODULE_ABORTED") {
                    Err(cancelled())
                } else {
                    Ok(calls
                        .iter()
                        .map(|call| transport_failure(call, error.clone()))
                        .collect())
                };
            }
        };
        reject_unknown(&response)?;
        if response.get("ok").and_then(JsValue::as_bool) != Some(true) {
            return Ok(calls
                .iter()
                .map(|call| module_failure(call, &response))
                .collect());
        }
        let Some(JsValue::Array(results)) = response.get("payload").and_then(|v| v.get("results"))
        else {
            return Err(ModuleError::new(
                "Capability batch response must contain one result for every tool call.",
            )
            .into());
        };
        if results.len() != calls.len() {
            return Err(ModuleError::new(
                "Capability batch response must contain one result for every tool call.",
            )
            .into());
        }
        results
            .iter()
            .zip(calls)
            .enumerate()
            .map(|(index, (result, call))| {
                let result = result
                    .as_object()
                    .filter(|v| v.get("type").is_some())
                    .ok_or_else(|| {
                        ModuleError::new(format!("Capability batch result {index} is invalid."))
                    })?;
                if result.get("toolCallId") != Some(&JsValue::String(call.id.clone())) {
                    return Err(ModuleError::new(format!(
                        "Capability batch result {index} does not match tool call {}.",
                        text(&call.id)
                    ))
                    .into());
                }
                Ok(result.clone())
            })
            .collect()
    }
}
impl ToolCatalog for ExternalToolExecution {
    fn list(&self) -> HostFuture<Vec<Arc<dyn ToolDefinition>>> {
        let tools = self.tools.clone();
        Box::pin(async move { Ok(tools) })
    }
}
impl ToolExecution for ExternalToolExecution {
    fn refresh(&self) -> HostFuture<()> {
        // The configured adapter has no descriptor decoder. The baseline rejects
        // advertising this optional method rather than silently fetching it.
        let advertised = self.client.binding().supports("list_tools");
        Box::pin(async move {
            if advertised {
                Err(ModuleError::new(
                    "Host advertised list_tools without a sidecar tool descriptor decoder.",
                )
                .into())
            } else {
                Ok(())
            }
        })
    }
    fn execute_all(
        &self,
        calls: Vec<ToolCall>,
        context: ToolInvocationContext,
    ) -> HostFuture<Vec<JsObject>> {
        let port = self.clone();
        Box::pin(async move {
            let runtime = context
                .runtime
                .downcast::<ToolRuntimeContext>()
                .map_err(|_| {
                    ModuleError::new("Tool runtime requires a host ToolRuntimeContext.")
                })?;
            let execution = context
                .execution
                .downcast::<ExecutionContext>()
                .map_err(|_| ModuleError::new("Tool runtime requires a host ExecutionContext."))?;
            let context = project(
                &runtime.fields,
                &[
                    "sessionId",
                    "turnId",
                    "cwd",
                    "permissionMode",
                    "permissionContext",
                    "runMode",
                    "currentToolCallId",
                    "maxResultBytes",
                    "outputTruncated",
                ],
            );
            let execution = project(
                &execution.fields,
                &[
                    "runId",
                    "turnId",
                    "operationId",
                    "idempotencyKey",
                    "operationDeadline",
                ],
            );
            if port.client.binding().supports("execute_batch") && !calls.is_empty() {
                return port.execute_batch(&calls, &context, &execution).await;
            }
            let mut slots = vec![None; calls.len()];
            let mut safe = Vec::new();
            let mut sequential = Vec::new();
            for (index, call) in calls.into_iter().enumerate() {
                if concurrency_safe(&port.tools, &call) {
                    safe.push((index, call));
                } else {
                    sequential.push((index, call));
                }
            }
            let pending = safe
                .into_iter()
                .map(|(index, call)| {
                    let (port, context, execution) =
                        (port.clone(), context.clone(), execution.clone());
                    Box::pin(async move {
                        port.execute_one(call, &context, &execution)
                            .await
                            .map(|result| (index, result))
                    }) as HostFuture<_>
                })
                .collect();
            let results = run_started_futures(pending).await?;
            for (index, result) in results {
                slots[index] = Some(result);
            }
            for (index, call) in sequential {
                slots[index] = Some(port.execute_one(call, &context, &execution).await?);
            }
            collect_slots(slots)
        })
    }
}

/// Gate the raw external owner through the host's existing decision service.
/// Batch support changes only grouping, never permission authority.
pub fn permission_aware_tool_port(
    port: Arc<dyn ToolExecution>,
    permission: Arc<dyn crate::host::modules::permission::PermissionDecision>,
    preserve_batch: bool,
) -> Arc<dyn ToolExecution> {
    Arc::new(PermissionAwareToolExecution {
        port,
        permission,
        preserve_batch,
    })
}
#[derive(Clone)]
struct PermissionAwareToolExecution {
    port: Arc<dyn ToolExecution>,
    permission: Arc<dyn crate::host::modules::permission::PermissionDecision>,
    preserve_batch: bool,
}
impl ToolCatalog for PermissionAwareToolExecution {
    fn list(&self) -> HostFuture<Vec<Arc<dyn ToolDefinition>>> {
        self.port.list()
    }
}
impl PermissionAwareToolExecution {
    async fn authorized(
        &self,
        call: ToolCall,
        tools: &[Arc<dyn ToolDefinition>],
        context: &ToolInvocationContext,
    ) -> Result<Result<ToolCall, JsObject>, HostError> {
        let Some(tool) = tools
            .iter()
            .find(|tool| tool.fields().get("name") == Some(&JsValue::String(call.name.clone())))
        else {
            return Ok(Ok(call));
        };
        let host = context
            .runtime
            .clone()
            .downcast::<ToolRuntimeContext>()
            .map_err(|_| ModuleError::new("Tool runtime requires a host ToolRuntimeContext."))?;
        let run = crate::runtime::tools::ToolRunContext::new(host);
        let decision = self
            .permission
            .decide(
                tool.clone(),
                call.input.clone(),
                Arc::new(run.clone()),
                call.id.clone(),
            )
            .await?;
        let kind = decision.get("type").and_then(JsValue::as_string);
        if kind.is_some_and(|kind| kind.equals_str("allow")) {
            let mut call = call;
            if let Some(input) = decision
                .get("updatedInput")
                .filter(|v| !matches!(v, JsValue::Undefined | JsValue::Null))
            {
                call.input = input.clone();
            }
            return Ok(Ok(call));
        }
        let deny = kind.is_some_and(|kind| kind.equals_str("deny"));
        let ask = kind.is_some_and(|kind| kind.equals_str("ask"));
        let runtime_prompt = decision.get("reason").is_some_and(|reason| {
            reason.get("type") == Some(&"runtime".into())
                && reason
                    .get("message")
                    .and_then(JsValue::as_string)
                    .is_some_and(|v| text(v).contains("prompt"))
        });
        let code = if deny {
            if runtime_prompt {
                "permission_required"
            } else {
                "permission_denied"
            }
        } else if kind.is_some_and(|kind| kind.equals_str("cancel")) {
            "permission_cancelled"
        } else {
            "permission_required"
        };
        let message = if ask {
            format!("Permission is required to run {}.", text(&call.name)).into()
        } else {
            decision
                .get("message")
                .and_then(JsValue::as_string)
                .cloned()
                .unwrap_or_default()
        };
        let mut error = crate::runtime::tools::ToolError::new(code, message);
        if ask {
            error.details = Some(object([(
                "request",
                decision
                    .get("request")
                    .cloned()
                    .unwrap_or(JsValue::Undefined),
            )]));
        }
        let started = run.now_iso()?;
        Ok(Err(crate::runtime::tools::create_tool_error_result(
            &call.id, &call.name, &error, &started, &run,
        )?))
    }
    async fn execute_group(
        &self,
        entries: Vec<(usize, ToolCall)>,
        tools: &[Arc<dyn ToolDefinition>],
        context: &ToolInvocationContext,
        parallel: bool,
    ) -> Result<Vec<(usize, JsObject)>, HostError> {
        let outcomes = if parallel {
            let pending = entries
                .into_iter()
                .map(|(index, call)| {
                    let (port, tools, context) =
                        (self.clone(), tools.to_vec(), clone_context(context));
                    Box::pin(async move {
                        port.authorized(call, &tools, &context)
                            .await
                            .map(|outcome| (index, outcome))
                    }) as HostFuture<_>
                })
                .collect();
            run_started_futures(pending).await?
        } else {
            let mut outcomes = Vec::new();
            for (index, call) in entries {
                outcomes.push((index, self.authorized(call, tools, context).await?));
            }
            outcomes
        };
        let mut slots = Vec::new();
        let mut executable = Vec::new();
        for (index, outcome) in outcomes {
            match outcome {
                Ok(call) => executable.push((index, call)),
                Err(result) => slots.push((index, result)),
            }
        }
        if !executable.is_empty() {
            let results = self
                .port
                .execute_all(
                    executable.iter().map(|(_, call)| call.clone()).collect(),
                    clone_context(context),
                )
                .await?;
            if results.len() != executable.len() {
                return Err(ModuleError::new(
                    "Tool port returned an incomplete authorized result.",
                )
                .into());
            }
            slots.extend(
                executable
                    .into_iter()
                    .zip(results)
                    .map(|((index, _), result)| (index, result)),
            );
        }
        Ok(slots)
    }
}
impl ToolExecution for PermissionAwareToolExecution {
    fn refresh(&self) -> HostFuture<()> {
        self.port.refresh()
    }
    fn execute_all(
        &self,
        calls: Vec<ToolCall>,
        context: ToolInvocationContext,
    ) -> HostFuture<Vec<JsObject>> {
        let port = self.clone();
        Box::pin(async move {
            let tools = port.port.list().await?;
            let mut slots = vec![None; calls.len()];
            let entries: Vec<_> = calls.into_iter().enumerate().collect();
            if port.preserve_batch {
                for (index, result) in port.execute_group(entries, &tools, &context, false).await? {
                    slots[index] = Some(result);
                }
            } else {
                let (safe, sequential): (Vec<_>, Vec<_>) = entries
                    .into_iter()
                    .partition(|(_, call)| concurrency_safe(&tools, call));
                for (index, result) in port.execute_group(safe, &tools, &context, true).await? {
                    slots[index] = Some(result);
                }
                for (index, result) in port
                    .execute_group(sequential, &tools, &context, false)
                    .await?
                {
                    slots[index] = Some(result);
                }
            }
            collect_slots(slots)
        })
    }
}

#[derive(Clone)]
struct ComposedToolExecution {
    primary: Arc<dyn ToolExecution>,
    fallback: Arc<dyn ToolExecution>,
    primary_names: Vec<JsString>,
    tools: Vec<Arc<dyn ToolDefinition>>,
}
/// Freeze the public names before dispatch. The native port passed here must
/// not already be wrapped in a durable recorder.
#[cfg(test)]
pub async fn compose_tool_ports(
    primary: Arc<dyn ToolExecution>,
    fallback: Arc<dyn ToolExecution>,
) -> Result<Arc<dyn ToolExecution>, HostError> {
    let primary_tools = primary.list().await?;
    let fallback_tools = fallback.list().await?;
    compose_tool_ports_with_catalogs(primary, primary_tools, fallback, fallback_tools)
}
/// Synchronous seam for a turn factory that already owns the native registry.
pub fn compose_tool_ports_with_catalogs(
    primary: Arc<dyn ToolExecution>,
    primary_tools: Vec<Arc<dyn ToolDefinition>>,
    fallback: Arc<dyn ToolExecution>,
    fallback_tools: Vec<Arc<dyn ToolDefinition>>,
) -> Result<Arc<dyn ToolExecution>, HostError> {
    let primary_names: Vec<_> = primary_tools
        .iter()
        .filter_map(|tool| {
            tool.fields()
                .get("name")
                .and_then(JsValue::as_string)
                .cloned()
        })
        .collect();
    if let Some(name) = fallback_tools
        .iter()
        .filter_map(|tool| tool.fields().get("name").and_then(JsValue::as_string))
        .find(|name| primary_names.contains(name))
    {
        return Err(ModuleError::new(format!(
            "External tool binding conflicts with host tool '{}'.",
            text(name)
        ))
        .into());
    }
    Ok(Arc::new(ComposedToolExecution {
        primary,
        fallback,
        primary_names,
        tools: primary_tools.into_iter().chain(fallback_tools).collect(),
    }))
}
impl ToolCatalog for ComposedToolExecution {
    fn list(&self) -> HostFuture<Vec<Arc<dyn ToolDefinition>>> {
        let tools = self.tools.clone();
        Box::pin(async move { Ok(tools) })
    }
}
impl ToolExecution for ComposedToolExecution {
    fn execute_all(
        &self,
        calls: Vec<ToolCall>,
        context: ToolInvocationContext,
    ) -> HostFuture<Vec<JsObject>> {
        let port = self.clone();
        Box::pin(async move {
            let mut slots = vec![None; calls.len()];
            let mut groups: Vec<(bool, Vec<(usize, ToolCall)>)> = Vec::new();
            let mut sequential = Vec::new();
            for (index, call) in calls.into_iter().enumerate() {
                let primary = port.primary_names.contains(&call.name);
                if concurrency_safe(&port.tools, &call) {
                    if let Some((_, calls)) = groups.iter_mut().find(|(owner, _)| *owner == primary)
                    {
                        calls.push((index, call));
                    } else {
                        groups.push((primary, vec![(index, call)]));
                    }
                } else {
                    sequential.push((primary, index, call));
                }
            }
            let pending = groups
                .into_iter()
                .map(|(primary, entries)| {
                    let owner = if primary {
                        port.primary.clone()
                    } else {
                        port.fallback.clone()
                    };
                    let context = clone_context(&context);
                    Box::pin(async move {
                        let results = owner
                            .execute_all(
                                entries.iter().map(|(_, call)| call.clone()).collect(),
                                context,
                            )
                            .await?;
                        if results.len() != entries.len() {
                            return Err(incomplete());
                        }
                        Ok(entries
                            .into_iter()
                            .zip(results)
                            .map(|((index, _), result)| (index, result))
                            .collect::<Vec<_>>())
                    }) as HostFuture<_>
                })
                .collect();
            let results = run_started_futures(pending).await?;
            for group in results {
                for (index, result) in group {
                    slots[index] = Some(result);
                }
            }
            for (primary, index, call) in sequential {
                let owner = if primary {
                    &port.primary
                } else {
                    &port.fallback
                };
                let mut results = owner
                    .execute_all(vec![call], clone_context(&context))
                    .await?;
                if results.len() != 1 {
                    return Err(incomplete());
                }
                slots[index] = results.pop();
            }
            collect_slots(slots)
        })
    }
}
/// Promise.all starts every safe operation and reports its first rejection
/// immediately. A rejected sibling does not revoke already dispatched work.
/// Like ToolRuntime::execute_batch, transfer unfinished owned futures to the
/// runtime on that path so their side effects/hooks can settle; do not wait for
/// them before returning the error. Dropping the whole batch before an outcome
/// still drops its owned pending work through the normal host cancellation path.
async fn run_started_futures<T: Send + 'static>(
    futures: Vec<HostFuture<T>>,
) -> Result<Vec<T>, HostError> {
    let mut slots: Vec<Option<T>> = (0..futures.len()).map(|_| None).collect();
    let mut pending: Vec<_> = futures.into_iter().map(Some).collect();
    let outcome = poll_fn(|cx| {
        let mut unfinished = false;
        let mut first_error = None;
        // Poll every entry even after one rejects, matching Promise.all's
        // synchronous map which starts every Promise before reporting failure.
        for (index, slot) in pending.iter_mut().enumerate() {
            let Some(future) = slot else {
                continue;
            };
            match future.as_mut().poll(cx) {
                Poll::Ready(Ok(result)) => {
                    slots[index] = Some(result);
                    *slot = None;
                }
                Poll::Ready(Err(error)) => {
                    first_error.get_or_insert(error);
                    *slot = None;
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
        for future in pending.into_iter().flatten() {
            // The task owns the operation until completion, including its
            // resources and cleanup. Its later error cannot replace the first.
            tokio::spawn(async move {
                let _ = future.await;
            });
        }
        return Err(error);
    }
    slots
        .into_iter()
        .map(|slot| slot.ok_or_else(incomplete))
        .collect()
}

fn clone_context(context: &ToolInvocationContext) -> ToolInvocationContext {
    ToolInvocationContext {
        runtime: context.runtime.clone(),
        execution: context.execution.clone(),
    }
}
fn concurrency_safe(tools: &[Arc<dyn ToolDefinition>], call: &ToolCall) -> bool {
    // The baseline Map uses the last definition of an external duplicate.
    tools
        .iter()
        .rev()
        .find(|tool| tool.fields().get("name") == Some(&JsValue::String(call.name.clone())))
        .and_then(|tool| tool.is_concurrency_safe(&call.input).ok())
        .is_some_and(|v| truthy(&v))
}
fn truthy(value: &JsValue) -> bool {
    match value {
        JsValue::Undefined | JsValue::Null => false,
        JsValue::Bool(v) => *v,
        JsValue::Number(v) => *v != 0.0 && !v.is_nan(),
        JsValue::String(v) => !v.is_empty(),
        _ => true,
    }
}
fn incomplete() -> HostError {
    ModuleError::new("Composed tool port returned an incomplete result set.").into()
}
fn collect_slots(slots: Vec<Option<JsObject>>) -> Result<Vec<JsObject>, HostError> {
    slots
        .into_iter()
        .map(|slot| slot.ok_or_else(incomplete))
        .collect()
}
fn object<const N: usize>(fields: [(&str, JsValue); N]) -> JsObject {
    let mut result = JsObject::new();
    for (key, value) in fields {
        result.insert(key.into(), value);
    }
    result
}
fn project(source: &JsObject, keys: &[&str]) -> JsObject {
    let mut result = JsObject::new();
    for key in keys {
        if let Some(value) = source.get(key).filter(|v| !matches!(v, JsValue::Undefined)) {
            result.insert((*key).into(), value.clone());
        }
    }
    result
}
fn text(value: &JsString) -> String {
    String::from_utf16_lossy(value.units())
}
fn cancelled() -> HostError {
    ModuleError::new("Tool execution cancelled.").into()
}
fn reject_unknown(response: &JsValue) -> Result<(), HostError> {
    if response.get("ok").and_then(JsValue::as_bool) == Some(false)
        && response.get("outcome") == Some(&"result_unknown".into())
    {
        let message = response
            .get("error")
            .and_then(|v| v.get("message"))
            .and_then(JsValue::as_string)
            .cloned()
            .unwrap_or_else(|| "Host capability execution outcome is unknown.".into());
        let mut error = ModuleError::new(message);
        error.code = Some("RESULT_UNKNOWN".into());
        return Err(error.into());
    }
    Ok(())
}
fn cancellation_error(error: Option<&JsValue>) -> bool {
    let code = error
        .and_then(|v| v.get("code"))
        .and_then(JsValue::as_string)
        .map(text)
        .unwrap_or_default();
    let message = error
        .and_then(|v| v.get("message"))
        .and_then(JsValue::as_string)
        .map(text)
        .unwrap_or_default();
    if ["CANCELLED", "ABORTED", "TOOL_ABORTED"].contains(&code.to_uppercase().as_str()) {
        return true;
    }
    format!("{code} {message}")
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|word| ["cancel", "cancelled", "cancellation"].contains(&word))
}
fn response_cancelled(response: &JsValue) -> bool {
    if response.get("ok").and_then(JsValue::as_bool) == Some(false) {
        let mut error = response
            .get("error")
            .and_then(JsValue::as_object)
            .cloned()
            .unwrap_or_default();
        if let Some(code) = response.get("code") {
            error.insert("code".into(), code.clone());
        }
        return cancellation_error(Some(&JsValue::Object(error)));
    }
    response.get("payload").is_some_and(|payload| {
        payload.get("type") == Some(&"error".into()) && cancellation_error(payload.get("error"))
    })
}
fn timestamp() -> JsValue {
    JsValue::String(
        pilotdeck_transport::system_clock::format_epoch_iso(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |v| v.as_millis() as i64),
        )
        .into(),
    )
}
fn failure_result(
    call: &ToolCall,
    code: JsValue,
    message: JsValue,
    details: Option<JsValue>,
) -> JsObject {
    let mut error = object([("code", code), ("message", message.clone())]);
    if let Some(details) = details {
        error.insert("details".into(), details);
    }
    object([
        ("type", "error".into()),
        ("toolCallId", JsValue::String(call.id.clone())),
        ("toolName", JsValue::String(call.name.clone())),
        ("error", JsValue::Object(error)),
        (
            "content",
            JsValue::Array(vec![JsValue::Object(object([
                ("type", "text".into()),
                ("text", message),
            ]))]),
        ),
        ("startedAt", timestamp()),
        ("completedAt", timestamp()),
    ])
}
fn transport_failure(call: &ToolCall, error: HttpModuleError) -> JsObject {
    let code = if error.code.equals_str("MODULE_TIMEOUT") {
        "tool_timeout"
    } else {
        "tool_execution_failed"
    };
    failure_result(
        call,
        code.into(),
        JsValue::String(error.message),
        Some(JsValue::Object(object([
            ("moduleCode", JsValue::String(error.code)),
            ("outcome", "result_unknown".into()),
            ("retryability", "unsafe".into()),
        ]))),
    )
}
fn module_failure(call: &ToolCall, response: &JsValue) -> JsObject {
    let code = response.get("code").and_then(JsValue::as_string);
    let known = [
        "tool_not_found",
        "tool_unavailable",
        "invalid_tool_input",
        "permission_denied",
        "permission_cancelled",
        "permission_required",
        "tool_execution_failed",
        "tool_aborted",
        "tool_timeout",
        "result_too_large",
        "path_not_allowed",
        "file_not_found",
        "file_conflict",
        "unsupported_tool",
        "setup_required",
        "plan_mode_violation",
        "ask_mode_violation",
    ];
    let tool_code = code
        .filter(|code| known.iter().any(|known| code.equals_str(known)))
        .cloned()
        .unwrap_or_else(|| "tool_execution_failed".into());
    let module_error = response.get("error");
    let message = module_error
        .and_then(|v| v.get("message"))
        .cloned()
        .unwrap_or_else(|| "Capability module failed.".into());
    let mut details = JsObject::new();
    if let Some(code) = code {
        details.insert("moduleCode".into(), JsValue::String(code.clone()));
    }
    if let Some(error) = module_error {
        details.insert("moduleError".into(), error.clone());
    }
    failure_result(
        call,
        JsValue::String(tool_code),
        message,
        (!details.entries().is_empty()).then(|| JsValue::Object(details)),
    )
}

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tests;
