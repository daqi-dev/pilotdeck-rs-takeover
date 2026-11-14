//! `modules.modelProvider` bound to an external HTTP model module (PilotDeck
//! cc7c4e6 `composition/runtimePorts.ts:99-104` `createExternalModelPort`,
//! `agent/modules/llm/hostModelInvokerPort.ts:66-310`,
//! `composition/HttpModuleClient.ts`).
//!
//! The port replaces only the session's model *execution*
//! (`AgentSessionRuntimeBundle.ts:184-193`: `externalPorts.model ??
//! createRouterModelInvokerPort(router, ...)`). Everything else the router
//! provides stays with the router: the durable recorder wraps this port the
//! same way, sticky routing, metadata lookups and request materialization
//! still read the router, and title generation, compaction summaries, the
//! auxiliary model and one-shot subagents call the router directly.
//!
//! Wire shape (one HTTP `module_call` per operation, `module: "model"`):
//!
//! - `prepare` (always advertised; the registry requires it): payload
//!   `{operation, request, preparationId, context}`; the module answers
//!   `payload.prepared = {provider, model, request, maxContextTokens?,
//!   maxOutputTokens?}`.
//! - `stream` (eager): payload `{operation, request, preparationId, context}`;
//!   the module answers `payload.events` — every canonical event of the call.
//! - `stream_next` (pull, when advertised): the same payload each time; the
//!   module answers `{events, done}` until `done`. A stream that ends before
//!   `done` (consumer return, error, abort) sends `close_stream` with
//!   `{operation, preparationId}` when that method is advertised.
//!
//! The HTTP envelope, manifest handshake, correlation checks and transport
//! error codes are the shared module client's ([`ExternalSkillsPort`]).
#![allow(clippy::result_large_err)]

use crate::{
    host::{
        ModuleError,
        modules::model::{
            ModelExecution, ModelFuture, ModelStream, PrepareInput, PreparedInvocation,
            StreamFuture, StreamInput,
        },
    },
    runtime::model::router::AbortFlag,
    skills::{
        ExternalSkillBinding, ExternalSkillsPort, HyperModuleTransport, ModuleCall, SkillError,
    },
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    task::Poll,
};
use tokio::sync::oneshot;

/// One `createExternalModelPort(binding)`: one module client (manifest cache,
/// HTTP message ids) and one preparation counter per session.
pub(super) struct ExternalModelPort {
    client: ExternalSkillsPort<HyperModuleTransport>,
    binding: JsValue,
    /// `options.methods?.includes("prepare")`.
    prepare: bool,
    /// `options.methods?.includes("stream_next")`.
    pull: bool,
    /// `options.methods?.includes("close_stream")`.
    close: bool,
    /// `++preparationSequence`.
    preparations: AtomicU64,
}

impl ExternalModelPort {
    /// `snapshot.config.modules.modelProvider` as the registry resolved it.
    /// Only an external HTTP binding (`isExternalModuleBinding`) composes a
    /// port; a built-in or disabled slot keeps the router.
    pub(super) fn from_binding(binding: &JsValue) -> Option<Self> {
        let object = binding.as_object()?;
        if !object.contains_key("implementationId")
            || !object
                .get("transport")
                .and_then(JsValue::as_string)
                .is_some_and(|transport| transport.equals_str("module-http-v2"))
        {
            return None;
        }
        let parsed = ExternalSkillBinding::from_value(binding).ok()?;
        let has = |name: &str| parsed.methods.iter().any(|method| method == name);
        Some(Self {
            prepare: has("prepare"),
            pull: has("stream_next"),
            close: has("close_stream"),
            client: ExternalSkillsPort::new(parsed, Arc::new(HyperModuleTransport::new())),
            binding: binding.clone(),
            preparations: AtomicU64::new(0),
        })
    }

    /// A child session's own port over the same binding.
    pub(super) fn fresh(&self) -> Option<Self> {
        Self::from_binding(&self.binding)
    }

    /// `prepared-${uuid()}-${++preparationSequence}`.
    fn next_preparation_id(&self) -> JsString {
        let sequence = self.preparations.fetch_add(1, Ordering::SeqCst) + 1;
        format!(
            "prepared-{}-{sequence}",
            super::module_tools::random_suffix()
        )
        .into()
    }

    /// The turn's view: `context.abortSignal` cancels the in-flight module
    /// request of a `prepare` (the stream carries its own).
    ///
    /// `in_process`: the gateway's own loop, which the baseline runs in
    /// process. There `AgentLoop.ts:711-841` hands `prepare` and `stream` one
    /// and the same `modelContext`, sticky route metadata included; the
    /// module wire's stream call carries no route metadata (the host's
    /// sticky wrapper adds it to `prepare` only, as `bindTurn` does for an
    /// external loop), so the stream reuses the context its preparation saw.
    pub(super) fn for_turn(
        self: &Arc<Self>,
        abort: Option<AbortFlag>,
        in_process: bool,
    ) -> Arc<dyn ModelExecution> {
        Arc::new(TurnModelPort {
            port: self.clone(),
            abort,
            prepared_contexts: in_process.then(Default::default),
        })
    }

    /// `callModule(...)` with the transport-only `abortSignal`: an abort
    /// drops the HTTP exchange and rejects like `MODULE_ABORTED`.
    async fn call(
        &self,
        call: ModuleCall,
        abort: Option<&AbortFlag>,
    ) -> Result<JsValue, ModuleError> {
        if abort.is_some_and(AbortFlag::is_aborted) {
            return Err(aborted("Module request was cancelled before dispatch."));
        }
        let response = match abort {
            Some(abort) => tokio::select! {
                biased;
                () = abort.cancelled() => return Err(aborted("Module request was cancelled.")),
                response = self.client.module_call(call) => response,
            },
            None => self.client.module_call(call).await,
        };
        response.map_err(client_error)
    }

    fn model_call(&self, context: &JsObject, request_id: String, payload: JsObject) -> ModuleCall {
        ModuleCall {
            run_id: text(context.get("runId")),
            // `context.operationId ?? context.turnId`.
            operation_id: match context.get("operationId") {
                Some(value) if !matches!(value, JsValue::Undefined | JsValue::Null) => {
                    text(Some(value))
                }
                _ => text(context.get("turnId")),
            },
            idempotency_key: context
                .get("idempotencyKey")
                .and_then(JsValue::as_string)
                .cloned(),
            request_id,
            module: "model",
            payload,
        }
    }
}

/// Each preparation id with the execution context its `prepare` saw.
type PreparedContexts = Arc<Mutex<Vec<(JsString, JsObject)>>>;

struct TurnModelPort {
    port: Arc<ExternalModelPort>,
    abort: Option<AbortFlag>,
    /// In process only: each preparation's execution context.
    prepared_contexts: Option<PreparedContexts>,
}

impl ModelExecution for TurnModelPort {
    fn prepare(&self, input: PrepareInput) -> ModelFuture<PreparedInvocation> {
        let port = self.port.clone();
        let abort = self.abort.clone();
        let contexts = self.prepared_contexts.clone();
        Box::pin(async move {
            let provider = input
                .request
                .get("provider")
                .cloned()
                .unwrap_or(JsValue::Undefined);
            let model = input
                .request
                .get("model")
                .cloned()
                .unwrap_or(JsValue::Undefined);
            let preparation_id = port.next_preparation_id();
            if let Some(contexts) = &contexts {
                contexts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((preparation_id.clone(), input.context.clone()));
            }
            let mut fallback = PreparedInvocation::new(input.request.clone(), provider, model);
            fallback.preparation_id = Some(preparation_id.clone());
            if !port.prepare {
                return Ok(fallback);
            }
            let payload = object([
                ("operation", "prepare".into()),
                ("request", input.request),
                ("preparationId", JsValue::String(preparation_id.clone())),
                (
                    "context",
                    JsValue::Object(serialize_context(&input.context)),
                ),
            ]);
            let call = port.model_call(
                &input.context,
                format!("model-prepare-{}", super::module_tools::random_suffix()),
                payload,
            );
            let response = port.call(call, abort.as_ref()).await?;
            let mut prepared = read_prepared(&response)?;
            prepared.preparation_id = Some(preparation_id);
            Ok(prepared)
        })
    }

    fn stream(&self, input: StreamInput) -> ModelFuture<Box<dyn ModelStream>> {
        let port = self.port.clone();
        let contexts = self.prepared_contexts.clone();
        Box::pin(async move {
            let preparation_id = match &input.prepared.preparation_id {
                Some(id) => id.clone(),
                None => port.next_preparation_id(),
            };
            let context = contexts
                .and_then(|contexts| {
                    contexts
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .iter()
                        .find(|(id, _)| id == &preparation_id)
                        .map(|(_, context)| context.clone())
                })
                .unwrap_or(input.context);
            Ok(Box::new(ExternalModelStream {
                port,
                request: input.prepared.request,
                context,
                abort: input.abort,
                preparation_id,
                queue: VecDeque::new(),
                phase: Phase::Open,
                inflight: None,
            }) as Box<dyn ModelStream>)
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// More events may come from the module.
    Open,
    /// The module reported `done` (or the eager response was consumed).
    Done,
    /// The generator body ended without `done`: its `finally` already ran.
    Finished,
}

/// One module round trip's outcome: the events and whether the module is
/// drained. A failure has already run the pull loop's `finally`.
type Fetched = Result<(Vec<JsValue>, bool), ModuleError>;

/// The `async *stream` generator body: the module is called on the first
/// pull (an unstarted generator never runs), each response's events are
/// yielded in order.
///
/// A pull may be dropped and re-created at any await (the durable wrapper
/// re-polls a fresh `next()` each time), so every module round trip runs as
/// its own task and the stream polls its completion in place: a dropped
/// pull neither cancels nor repeats the request.
struct ExternalModelStream {
    port: Arc<ExternalModelPort>,
    request: JsValue,
    context: JsObject,
    abort: Option<AbortFlag>,
    preparation_id: JsString,
    queue: VecDeque<JsValue>,
    phase: Phase,
    inflight: Option<oneshot::Receiver<Fetched>>,
}

impl ExternalModelStream {
    fn spawn_fetch(&self) -> oneshot::Receiver<Fetched> {
        let port = self.port.clone();
        let pull = port.pull;
        let operation = if pull { "stream_next" } else { "stream" };
        let request_id = if pull {
            format!("model-stream-next-{}", super::module_tools::random_suffix())
        } else {
            format!("model-{}", super::module_tools::random_suffix())
        };
        let payload = object([
            ("operation", operation.into()),
            ("request", self.request.clone()),
            (
                "preparationId",
                JsValue::String(self.preparation_id.clone()),
            ),
            ("context", JsValue::Object(serialize_context(&self.context))),
        ]);
        let call = port.model_call(&self.context, request_id, payload);
        let close = (pull && port.close).then(|| self.close_call());
        let abort = self.abort.clone();
        let (sender, receiver) = oneshot::channel();
        tokio::spawn(async move {
            let fetched = fetch(&port, call, abort.as_ref()).await;
            // `finally { if (!done && supportsCloseStream) close_stream }`:
            // a failed pull leaves the module undrained.
            if fetched.is_err()
                && let Some(close) = close
            {
                let _ = port.call(close, None).await;
            }
            let _ = sender.send(fetched);
        });
        receiver
    }

    fn close_call(&self) -> ModuleCall {
        let payload = object([
            ("operation", "close_stream".into()),
            (
                "preparationId",
                JsValue::String(self.preparation_id.clone()),
            ),
        ]);
        self.port.model_call(
            &self.context,
            format!(
                "model-close-stream-{}",
                super::module_tools::random_suffix()
            ),
            payload,
        )
    }

    /// Apply a settled round trip.
    fn settle(
        &mut self,
        fetched: Result<Fetched, oneshot::error::RecvError>,
    ) -> Result<(), ModuleError> {
        self.inflight = None;
        match fetched {
            Ok(Ok((events, done))) => {
                self.queue.extend(events);
                if done {
                    self.phase = Phase::Done;
                }
                Ok(())
            }
            Ok(Err(error)) => {
                self.phase = Phase::Finished;
                Err(error)
            }
            Err(_) => {
                self.phase = Phase::Finished;
                Err(ModuleError::new(
                    "Model module request ended without completing.",
                ))
            }
        }
    }
}

/// One `stream` / `stream_next` round trip and its response checks.
async fn fetch(port: &ExternalModelPort, call: ModuleCall, abort: Option<&AbortFlag>) -> Fetched {
    let pull = port.pull;
    let response = port.call(call, abort).await?;
    if response.get("ok") != Some(&JsValue::Bool(true)) {
        return Err(module_failure(
            &response,
            if pull {
                "Model stream failed"
            } else {
                "Model module failed"
            },
        ));
    }
    let payload = response.get("payload");
    let events = payload.and_then(|payload| payload.get("events"));
    let Some(JsValue::Array(events)) = events else {
        return Err(invalid_response(if pull {
            "Model stream_next response must contain events and done."
        } else {
            "Model module response must contain canonical events."
        }));
    };
    let done = if pull {
        match payload.and_then(|payload| payload.get("done")) {
            Some(JsValue::Bool(done)) => *done,
            _ => {
                return Err(invalid_response(
                    "Model stream_next response must contain events and done.",
                ));
            }
        }
    } else {
        true
    };
    Ok((events.to_vec(), done))
}

impl ModelStream for ExternalModelStream {
    fn next(&mut self) -> StreamFuture<'_, Option<JsValue>> {
        Box::pin(std::future::poll_fn(move |cx| {
            loop {
                if let Some(event) = self.queue.pop_front() {
                    return Poll::Ready(Ok(Some(event)));
                }
                if self.phase != Phase::Open {
                    return Poll::Ready(Ok(None));
                }
                if self.inflight.is_none() {
                    self.inflight = Some(self.spawn_fetch());
                }
                let inflight = self.inflight.as_mut().expect("in-flight fetch");
                let fetched = match Pin::new(inflight).poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(fetched) => fetched,
                };
                if let Err(error) = self.settle(fetched) {
                    return Poll::Ready(Err(error));
                }
            }
        }))
    }

    fn close(&mut self) -> StreamFuture<'_, ()> {
        Box::pin(async move {
            // A `return()` queued behind a pending pull settles after it.
            if let Some(inflight) = self.inflight.as_mut() {
                let fetched = inflight.await;
                let _ = self.settle(fetched);
            }
            self.queue.clear();
            if self.phase == Phase::Open {
                self.phase = Phase::Finished;
                if self.port.pull && self.port.close {
                    let port = self.port.clone();
                    let call = self.close_call();
                    let _ = tokio::spawn(async move {
                        let _ = port.call(call, None).await;
                    })
                    .await;
                }
            }
            Ok(())
        })
    }
}

/// `readPreparedInvocation` (hostModelInvokerPort.ts:250-274).
fn read_prepared(response: &JsValue) -> Result<PreparedInvocation, ModuleError> {
    if response.get("ok") != Some(&JsValue::Bool(true)) {
        return Err(module_failure(response, "Model prepare failed"));
    }
    let preparation = |message: &str| ModuleError {
        code: Some("INVALID_MODEL_PREPARATION".into()),
        ..ModuleError::new(message)
    };
    let Some(projection) = response
        .get("payload")
        .and_then(|payload| payload.get("prepared"))
        .filter(|value| matches!(value, JsValue::Object(_) | JsValue::Array(_)))
    else {
        return Err(preparation(
            "Model prepare response must contain a prepared invocation.",
        ));
    };
    let provider = projection
        .get("provider")
        .filter(|value| value.as_string().is_some());
    let model = projection
        .get("model")
        .filter(|value| value.as_string().is_some());
    let request = projection
        .get("request")
        .filter(|value| matches!(value, JsValue::Object(_) | JsValue::Array(_)));
    let (Some(provider), Some(model), Some(request)) = (provider, model, request) else {
        return Err(preparation(
            "Model prepare response contains an invalid prepared invocation.",
        ));
    };
    let mut prepared = PreparedInvocation::new(request.clone(), provider.clone(), model.clone());
    prepared.max_context_tokens = positive_integer(projection.get("maxContextTokens"));
    prepared.max_output_tokens = positive_integer(projection.get("maxOutputTokens"));
    Ok(prepared)
}

/// `moduleFailure` (hostModelInvokerPort.ts:276-288): a canonical model
/// error becomes a `ModelProviderError`; anything else a plain error with the
/// module's code and retry hints.
fn module_failure(response: &JsValue, fallback: &str) -> ModuleError {
    let error = response.get("error");
    if let Some(canonical) = error
        .and_then(|error| error.get("canonical"))
        .and_then(JsValue::as_object)
        .filter(|canonical| {
            canonical.get("code").and_then(JsValue::as_string).is_some()
                && canonical
                    .get("message")
                    .and_then(JsValue::as_string)
                    .is_some()
        })
    {
        return ModuleError {
            message: canonical
                .get("message")
                .and_then(JsValue::as_string)
                .cloned()
                .unwrap_or_default(),
            code: canonical.get("code").and_then(JsValue::as_string).cloned(),
            retryable: canonical.get("retryable").and_then(JsValue::as_bool),
            retry_after_ms: canonical.get("retryAfterMs").and_then(JsValue::as_number),
            canonical: Some(canonical.clone()),
        };
    }
    // `String(response.error?.message ?? response.code ?? fallback)`.
    let message = error
        .and_then(|error| error.get("message"))
        .filter(|value| !matches!(value, JsValue::Undefined | JsValue::Null))
        .or_else(|| {
            response
                .get("code")
                .filter(|value| !matches!(value, JsValue::Undefined | JsValue::Null))
        })
        .map(|value| text(Some(value)))
        .unwrap_or_else(|| fallback.into());
    ModuleError {
        message,
        code: response.get("code").and_then(JsValue::as_string).cloned(),
        canonical: None,
        retryable: error
            .and_then(|error| error.get("retryable"))
            .and_then(JsValue::as_bool),
        retry_after_ms: error
            .and_then(|error| error.get("retryAfterMs"))
            .and_then(JsValue::as_number),
    }
}

fn invalid_response(message: &str) -> ModuleError {
    ModuleError {
        code: Some("INVALID_MODEL_RESPONSE".into()),
        ..ModuleError::new(message)
    }
}

/// A module client failure (`HttpModuleClient.ts`): the error's code and
/// message, like the thrown `Error & { code }`.
fn client_error(error: SkillError) -> ModuleError {
    ModuleError {
        code: (!error.code.is_empty()).then(|| error.code.clone()),
        ..ModuleError::new(error.message.clone())
    }
}

fn aborted(message: &str) -> ModuleError {
    ModuleError {
        code: Some("MODULE_ABORTED".into()),
        ..ModuleError::new(message)
    }
}

/// `serializeModelExecutionContext`: the context without host-local sinks.
/// The in-process context is already plain data.
fn serialize_context(context: &JsObject) -> JsObject {
    let mut serialized = JsObject::new();
    for (key, value) in context.entries() {
        if key.equals_str("abortSignal") || key.equals_str("invocationLogSink") {
            continue;
        }
        serialized.insert(key.clone(), value.clone());
    }
    serialized
}

fn positive_integer(value: Option<&JsValue>) -> Option<JsValue> {
    let number = value?.as_number()?;
    (number.fract() == 0.0 && number > 0.0 && number <= 9_007_199_254_740_991.0)
        .then_some(JsValue::Number(number))
}

fn text(value: Option<&JsValue>) -> JsString {
    value
        .and_then(|value| crate::js_value::to_string_units(value).ok())
        .map(JsString::from_utf16)
        .unwrap_or_default()
}

fn object<const N: usize>(entries: [(&str, JsValue); N]) -> JsObject {
    let mut object = JsObject::new();
    for (key, value) in entries {
        object.insert(key.into(), value);
    }
    object
}
