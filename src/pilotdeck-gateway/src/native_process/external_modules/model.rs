//! Fixed-baseline HTTP model consumer. Select these ports before durable wrapping.
//!
//! `runtimePorts.ts:99-104` uses `hostModelInvokerPort.ts`; the production
//! session wrapper (`AgentSessionRuntimeBundle.ts:193-199`) exposes execution.
//! Composition retains native config metadata separately. External preparations
//! use the prepared-execution materializer below, never native router decisions.
//! This is the authorized fix for the baseline native-materializer crash; its
//! replacement behavior follows `AgentLoop.ts:2508-2530`'s execution fallback.
#![allow(clippy::result_large_err)]

use super::http::{HttpModuleClient, HttpModuleError, ModuleCallInput};
use crate::host::ModuleError;
use crate::host::modules::model::{
    ModelExecution, ModelFuture, ModelStream, PrepareInput, PreparedInvocation,
    RequestMaterializer, StreamFuture, StreamInput,
};
use crate::host::modules::model_port::ComposedPorts;
use crate::runtime::network::Cancellation;
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::collections::VecDeque;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// Production selection bundle. The caller installs config metadata and durable
/// recording; this adapter supplies the materializer for external preparations.
pub fn ports(client: Arc<HttpModuleClient>, abort: Cancellation) -> ComposedPorts {
    ComposedPorts {
        execution: Arc::new(ExternalModelExecution::new(client, abort)),
        metadata: None,
        materializer: Some(Arc::new(PreparedExecutionMaterializer)),
    }
}

/// Preserve the prepared execution's provider controls while replacing the
/// canonical context window. It deliberately ignores `opaque`: external model
/// preparation state is not a native router decision, even if its shape happens
/// to resemble one. This materializer performs no remote module operation.
struct PreparedExecutionMaterializer;

impl RequestMaterializer for PreparedExecutionMaterializer {
    fn materialize(
        &self,
        prepared: PreparedInvocation,
        candidate: JsObject,
    ) -> ModelFuture<JsValue> {
        Box::pin(async move {
            let mut request = JsObject::new();
            if let Some(fields) = prepared.request.as_object() {
                request = fields.clone();
            } else if let JsValue::Array(items) = &prepared.request {
                // JS object spread also retains an array's enumerable indices.
                for (index, value) in items.iter().enumerate() {
                    request.insert(index.to_string().into(), value.clone());
                }
            } else if let JsValue::String(text) = &prepared.request {
                for (index, unit) in text.units().iter().enumerate() {
                    request.insert(
                        index.to_string().into(),
                        JsValue::String(JsString::from_utf16(vec![*unit])),
                    );
                }
            }
            for field in ["messages", "cacheBreakpoints", "cachePlan"] {
                // An absent candidate field becomes own undefined, clearing the
                // old prepared value just like the baseline object literal.
                request.insert(
                    field.into(),
                    candidate.get(field).cloned().unwrap_or(JsValue::Undefined),
                );
            }
            request.insert("provider".into(), prepared.provider);
            request.insert("model".into(), prepared.model);
            Ok(JsValue::Object(request))
        })
    }
}

/// A turn owns this consumer and its preparation identities. `opaque` carries
/// only local state; requests sent to the module never contain it.
#[derive(Clone)]
pub struct ExternalModelExecution {
    client: Arc<HttpModuleClient>,
    abort: Cancellation,
    owner: JsString,
    sequence: Arc<AtomicU64>,
}

impl ExternalModelExecution {
    pub fn new(client: Arc<HttpModuleClient>, abort: Cancellation) -> Self {
        Self {
            client,
            abort,
            owner: uuid::Uuid::new_v4().to_string().into(),
            sequence: Arc::new(AtomicU64::new(0)),
        }
    }

    fn supports(&self, method: &str) -> bool {
        self.client.binding().supports(method)
    }

    fn preparation_id(&self) -> JsString {
        format!(
            "prepared-{}-{}",
            String::from_utf16_lossy(self.owner.units()),
            self.sequence.fetch_add(1, Ordering::Relaxed) + 1
        )
        .into()
    }

    fn remember(&self, prepared: &mut PreparedInvocation, id: JsString, context: JsObject) {
        prepared.opaque = Some(object([
            ("externalModelOwner", JsValue::String(self.owner.clone())),
            ("preparationId", JsValue::String(id)),
            ("context", JsValue::Object(context)),
        ]));
    }

    fn identity(&self, prepared: &PreparedInvocation) -> Option<(JsString, JsObject)> {
        let state = prepared.opaque.as_ref()?;
        if state.get("externalModelOwner").and_then(JsValue::as_string) != Some(&self.owner) {
            return None;
        }
        Some((
            state.get("preparationId")?.as_string()?.clone(),
            state.get("context")?.as_object()?.clone(),
        ))
    }

    async fn call(
        &self,
        operation: &str,
        payload: JsValue,
        context: &JsObject,
        abort: Option<Cancellation>,
    ) -> Result<JsValue, ModuleError> {
        let field = |name: &str| context.get(name).and_then(JsValue::as_string).cloned();
        self.client
            .call(ModuleCallInput {
                run_id: field("runId").unwrap_or_default(),
                operation_id: field("operationId")
                    .or_else(|| field("turnId"))
                    .unwrap_or_default(),
                request_id: format!("{operation}-{}", uuid::Uuid::new_v4()).into(),
                module: "model".into(),
                payload,
                idempotency_key: field("idempotencyKey"),
                abort,
            })
            .await
            .map_err(http_error)
    }
}

impl ModelExecution for ExternalModelExecution {
    fn prepare(&self, input: PrepareInput) -> ModelFuture<PreparedInvocation> {
        let port = self.clone();
        Box::pin(async move {
            let mut prepared = PreparedInvocation::new(
                input.request.clone(),
                input
                    .request
                    .get("provider")
                    .cloned()
                    .unwrap_or(JsValue::Undefined),
                input
                    .request
                    .get("model")
                    .cloned()
                    .unwrap_or(JsValue::Undefined),
            );
            let id = port.preparation_id();
            if port.supports("prepare") {
                let response = port
                    .call(
                        "model-prepare",
                        object([
                            ("operation", "prepare".into()),
                            ("request", input.request),
                            ("preparationId", JsValue::String(id.clone())),
                            ("context", serializable_context(&input.context)),
                        ]),
                        &input.context,
                        Some(port.abort.clone()),
                    )
                    .await?;
                assert_response(&response, "Model prepare failed")?;
                prepared = read_prepared(&response)?;
            }
            port.remember(&mut prepared, id, input.context);
            Ok(prepared)
        })
    }

    fn stream(&self, input: StreamInput) -> ModelFuture<Box<dyn ModelStream>> {
        let port = self.clone();
        Box::pin(async move {
            let id = port
                .identity(&input.prepared)
                .map(|(id, _)| id)
                .unwrap_or_else(|| port.preparation_id());
            // Like an unstarted async generator, construction/close cannot
            // dispatch a request. The first next() starts the exchange.
            Ok(Box::new(ExternalModelStream {
                pull: port.supports("stream_next"),
                port,
                request: input.prepared.request,
                context: input.context,
                abort: input.abort,
                id,
                events: VecDeque::new(),
                pending: None,
                cleanup: None,
                failure: None,
                done: false,
                batch_done: false,
                started: false,
                closed: false,
            }) as Box<dyn ModelStream>)
        })
    }
}

/// The low-level host adapter supports this optional method, but `ports` does
/// does not expose this remote capability: production uses the independent
/// prepared-execution fallback instead.
impl RequestMaterializer for ExternalModelExecution {
    fn materialize(
        &self,
        prepared: PreparedInvocation,
        candidate: JsObject,
    ) -> ModelFuture<JsValue> {
        let port = self.clone();
        Box::pin(async move {
            let Some((id, context)) = port
                .identity(&prepared)
                .filter(|_| port.supports("materialize_prepared_request"))
            else {
                return Err(invalid(
                    "INVALID_MODEL_RESPONSE",
                    "Prepared invocation is not bound to a host model request.",
                ));
            };
            let response = port
                .call(
                    "model-materialize",
                    object([
                        ("operation", "materialize_prepared_request".into()),
                        ("preparationId", JsValue::String(id)),
                        ("request", JsValue::Object(candidate)),
                    ]),
                    &context,
                    None,
                )
                .await?;
            assert_response(&response, "Model request materialization failed")?;
            response
                .get("payload")
                .and_then(|payload| payload.get("request"))
                .filter(|request| is_js_object(request))
                .cloned()
                .ok_or_else(|| {
                    invalid(
                        "INVALID_MODEL_RESPONSE",
                        "Model materialization response must contain a canonical request.",
                    )
                })
        })
    }
}

struct EventBatch {
    events: VecDeque<JsValue>,
    done: bool,
}

struct ExternalModelStream {
    port: ExternalModelExecution,
    request: JsValue,
    context: JsObject,
    abort: Option<Cancellation>,
    id: JsString,
    events: VecDeque<JsValue>,
    // The durable consumer polls a temporary next() future once and may drop
    // it at Pending. Exchanges and cleanup therefore belong to the stream,
    // never to that borrowing future. A later poll resumes the same request.
    pending: Option<ModelFuture<EventBatch>>,
    cleanup: Option<ModelFuture<()>>,
    failure: Option<ModuleError>,
    pull: bool,
    done: bool,
    batch_done: bool,
    started: bool,
    closed: bool,
}

impl ExternalModelStream {
    fn begin_cleanup(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.pending = None;
        self.events.clear();
        if self.started && self.pull && !self.done && self.port.supports("close_stream") {
            let port = self.port.clone();
            let context = self.context.clone();
            let id = self.id.clone();
            self.cleanup = Some(Box::pin(async move {
                // Cleanup has no cancelled turn signal. Its failure must never
                // mask the original error, like the TS finally/catch.
                let _ = port
                    .call(
                        "model-close-stream",
                        object([
                            ("operation", "close_stream".into()),
                            ("preparationId", JsValue::String(id)),
                        ]),
                        &context,
                        None,
                    )
                    .await;
                Ok(())
            }));
        }
    }

    fn fetch(&self) -> ModelFuture<EventBatch> {
        let port = self.port.clone();
        let request = self.request.clone();
        let context = self.context.clone();
        let id = self.id.clone();
        let pull = self.pull;
        let abort = self.abort.clone().unwrap_or_else(|| port.abort.clone());
        Box::pin(async move {
            let operation = if pull { "stream_next" } else { "stream" };
            let request_prefix = if pull { "model-stream-next" } else { "model" };
            let response = port
                .call(
                    request_prefix,
                    object([
                        ("operation", operation.into()),
                        ("request", request),
                        ("preparationId", JsValue::String(id)),
                        ("context", serializable_context(&context)),
                    ]),
                    &context,
                    Some(abort),
                )
                .await?;
            assert_response(
                &response,
                if pull {
                    "Model stream failed"
                } else {
                    "Model module failed"
                },
            )?;
            let payload = response.get("payload");
            let events = payload
                .and_then(|payload| payload.get("events"))
                .and_then(|value| match value {
                    JsValue::Array(values) => Some(values),
                    _ => None,
                });
            let done = payload
                .and_then(|payload| payload.get("done"))
                .and_then(JsValue::as_bool);
            if events.is_none() || (pull && done.is_none()) {
                return Err(invalid(
                    "INVALID_MODEL_RESPONSE",
                    if pull {
                        "Model stream_next response must contain events and done."
                    } else {
                        "Model module response must contain canonical events."
                    },
                ));
            }
            Ok(EventBatch {
                events: events.expect("checked events").iter().cloned().collect(),
                done: !pull || done == Some(true),
            })
        })
    }
}

impl ModelStream for ExternalModelStream {
    fn next(&mut self) -> StreamFuture<'_, Option<JsValue>> {
        Box::pin(async move {
            loop {
                if let Some(cleanup) = self.cleanup.as_mut() {
                    let _ = cleanup.await;
                    self.cleanup = None;
                }
                if let Some(error) = self.failure.take() {
                    return Err(error);
                }
                if self.closed {
                    return Ok(None);
                }
                if let Some(event) = self.events.pop_front() {
                    return Ok(Some(event));
                }
                if self.batch_done {
                    self.done = true;
                    self.closed = true;
                    return Ok(None);
                }
                if self.pending.is_none() {
                    self.started = true;
                    self.pending = Some(self.fetch());
                }
                let batch = self.pending.as_mut().expect("pending model exchange").await;
                self.pending = None;
                match batch {
                    Ok(batch) => {
                        self.events = batch.events;
                        // Apply done only after every event has been yielded;
                        // an early return at the last yield still closes pull.
                        self.batch_done = batch.done;
                    }
                    Err(error) => {
                        self.failure = Some(error);
                        self.begin_cleanup();
                    }
                }
            }
        })
    }

    fn close(&mut self) -> StreamFuture<'_, ()> {
        Box::pin(async move {
            self.begin_cleanup();
            if let Some(cleanup) = self.cleanup.as_mut() {
                let _ = cleanup.await;
                self.cleanup = None;
            }
            self.failure = None;
            Ok(())
        })
    }
}

fn serializable_context(context: &JsObject) -> JsValue {
    let mut serialized = JsObject::new();
    for (key, value) in context.entries() {
        if !key.equals_str("abortSignal") && !key.equals_str("invocationLogSink") {
            serialized.insert(key.clone(), value.clone());
        }
    }
    JsValue::Object(serialized)
}

fn read_prepared(response: &JsValue) -> Result<PreparedInvocation, ModuleError> {
    let Some(prepared) = response
        .get("payload")
        .and_then(|payload| payload.get("prepared"))
        .filter(|value| is_js_object(value))
    else {
        return Err(invalid(
            "INVALID_MODEL_PREPARATION",
            "Model prepare response must contain a prepared invocation.",
        ));
    };
    let provider = prepared
        .get("provider")
        .filter(|value| value.as_string().is_some());
    let model = prepared
        .get("model")
        .filter(|value| value.as_string().is_some());
    let request = prepared.get("request").filter(|value| is_js_object(value));
    let (Some(provider), Some(model), Some(request)) = (provider, model, request) else {
        return Err(invalid(
            "INVALID_MODEL_PREPARATION",
            "Model prepare response contains an invalid prepared invocation.",
        ));
    };
    let positive_integer = |field: &str| {
        prepared
            .get(field)
            .filter(|value| {
                value.as_number().is_some_and(|number| {
                    number > 0.0 && number <= 9_007_199_254_740_991.0 && number.fract() == 0.0
                })
            })
            .cloned()
    };
    Ok(PreparedInvocation {
        request: request.clone(),
        provider: provider.clone(),
        model: model.clone(),
        max_context_tokens: positive_integer("maxContextTokens"),
        max_output_tokens: positive_integer("maxOutputTokens"),
        opaque: None,
    })
}

fn is_js_object(value: &JsValue) -> bool {
    matches!(value, JsValue::Object(_) | JsValue::Array(_))
}

fn assert_response(response: &JsValue, fallback: &str) -> Result<(), ModuleError> {
    if response.get("ok").and_then(JsValue::as_bool) == Some(true) {
        return Ok(());
    }
    let error = response.get("error");
    if let Some(canonical) = error
        .and_then(|error| error.get("canonical"))
        .filter(|canonical| {
            canonical.get("code").and_then(JsValue::as_string).is_some()
                && canonical
                    .get("message")
                    .and_then(JsValue::as_string)
                    .is_some()
        })
    {
        let mut failure = ModuleError::new(
            canonical
                .get("message")
                .and_then(JsValue::as_string)
                .expect("canonical message")
                .clone(),
        );
        failure.canonical = canonical.as_object().cloned();
        return Err(failure);
    }
    let message = error
        .and_then(|error| error.get("message"))
        .or_else(|| response.get("code"))
        .and_then(JsValue::as_string)
        .cloned()
        .unwrap_or_else(|| fallback.into());
    let mut failure = ModuleError::new(message);
    failure.code = response.get("code").and_then(JsValue::as_string).cloned();
    failure.retryable = error
        .and_then(|error| error.get("retryable"))
        .and_then(JsValue::as_bool);
    failure.retry_after_ms = error
        .and_then(|error| error.get("retryAfterMs"))
        .and_then(JsValue::as_number);
    Err(failure)
}

// Like the production TS callback projection, transport outcome/details stay
// on the typed HTTP error; they are not synthesized into canonical errors.
fn http_error(error: HttpModuleError) -> ModuleError {
    let mut failure = ModuleError::new(error.message);
    failure.code = Some(error.code);
    failure
}

fn invalid(code: &str, message: &str) -> ModuleError {
    let mut error = ModuleError::new(message);
    error.code = Some(code.into());
    error
}

fn object<const N: usize>(fields: [(&str, JsValue); N]) -> JsValue {
    let mut object = JsObject::new();
    for (key, value) in fields {
        object.insert(key.into(), value);
    }
    JsValue::Object(object)
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
