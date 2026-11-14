//! The turn's auxiliary model: the `context.model` a tool uses for a
//! secondary model call (`web_fetch`'s extraction).
//!
//! Baseline chain: `AgentLoop.createToolContext` puts
//! `capabilities.model.auxiliary.forTurn({ sessionId, turnId, projectPath:
//! config.cwd })` into the tool context (`AgentLoop.ts:2740-2790`); the native
//! composition builds it over the session's router
//! (`nativeAgentTurnCapabilitiesAdapter.ts:67-79`):
//! `router.stream(request, { sessionId, turnId, projectPath, abortSignal:
//! signal, isMainAgent: false })` — `decide` + `execute` like any other non-main
//! call, without the durable recorder, sticky state or an invocation audit
//! (none of those ride on that context).
//!
//! The router comes from the leased project generation, which the turn host
//! factory only sees after the tool context ports were composed, so the port
//! is late-bound: [`AuxiliaryModelSlot`] is handed to the tool context first
//! and bound to the router when the turn's handlers are assembled.
use super::{capability::RuntimeContext, tool_context::TurnModel};
use crate::runtime::{
    model::{
        http::{ModelStreamError, StreamOptions, YieldGate},
        router::{
            AbortFlag, RouterDecisionInput, RouterExecuteContext, RouterExecuteError, RouterRuntime,
        },
    },
    tools::{
        ToolRunContext,
        builtin::web::{
            SecondaryModel, SecondaryModelEvent, SecondaryNext, SecondaryStream, WebFetchModelPort,
        },
    },
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::{
    sync::{Arc, OnceLock},
    task::Poll,
};
use tokio::sync::mpsc;

/// The session router, bound once the turn's handlers are assembled.
#[derive(Clone, Default)]
pub struct AuxiliaryModelSlot(Arc<OnceLock<Arc<RouterRuntime>>>);

impl AuxiliaryModelSlot {
    pub fn bind(&self, router: Arc<RouterRuntime>) {
        let _ = self.0.set(router);
    }
}

impl TurnModel for AuxiliaryModelSlot {
    /// `forTurn({ sessionId, turnId, projectPath })`.
    fn for_turn(&self, identity: JsObject) -> Option<RuntimeContext> {
        let text = |key: &str| identity.get(key).and_then(JsValue::as_string).cloned();
        Some(Arc::new(WebFetchModelPort(Arc::new(
            AuxiliaryRouterModel {
                slot: self.clone(),
                turn: Some(TurnIdentity {
                    session_id: text("sessionId").unwrap_or_default(),
                    turn_id: text("turnId").unwrap_or_default(),
                    project_path: text("projectPath"),
                }),
            },
        ))))
    }
    /// The bare port: `stream: () => { throw new Error("Native auxiliary
    /// model requires a turn binding.") }`.
    fn unbound(&self) -> RuntimeContext {
        Arc::new(WebFetchModelPort(Arc::new(AuxiliaryRouterModel {
            slot: self.clone(),
            turn: None,
        })))
    }
}

#[derive(Clone)]
struct TurnIdentity {
    session_id: JsString,
    turn_id: JsString,
    project_path: Option<JsString>,
}

struct AuxiliaryRouterModel {
    slot: AuxiliaryModelSlot,
    turn: Option<TurnIdentity>,
}

fn thrown(message: &str) -> SecondaryModelEvent {
    SecondaryModelEvent::Throw {
        is_error: true,
        value: JsValue::Object(error_object(message.into(), &[])),
    }
}

fn error_object(message: JsString, extra: &[(&str, JsValue)]) -> JsObject {
    let mut object = JsObject::new();
    object.insert("message".into(), JsValue::String(message));
    for (key, value) in extra {
        object.insert((*key).into(), value.clone());
    }
    object
}

impl SecondaryModel for AuxiliaryRouterModel {
    fn stream(&self, request: JsValue, signal: Option<ToolRunContext>) -> Box<dyn SecondaryStream> {
        let (Some(turn), Some(router)) = (self.turn.clone(), self.slot.0.get().cloned()) else {
            let message = if self.turn.is_none() {
                "Native auxiliary model requires a turn binding."
            } else {
                "Auxiliary model router is not bound."
            };
            return Box::new(FailedStream(Some(thrown(message))));
        };
        Box::new(AuxiliaryStream {
            router,
            pending: Some((request, turn)),
            signal,
            events: None,
            outcome: None,
            awaiting_pull: false,
            abort: AbortFlag::new(),
            gate: YieldGate::new(),
            body: None,
            forwarder: None,
        })
    }
}

/// `model.stream(...)` threw synchronously: the first `next()` rethrows it.
struct FailedStream(Option<SecondaryModelEvent>);
impl SecondaryStream for FailedStream {
    fn next(&mut self) -> SecondaryNext<'_> {
        let item = self.0.take();
        Box::pin(async move { item })
    }
}

/// `router.stream(...)` as an async generator: the body (`decide` + `execute`)
/// starts on the first pull and runs as its own task feeding a queue; the
/// [`YieldGate`] holds the provider read at each yield until the consumer asks
/// for the next event, like the generator's suspension.
struct AuxiliaryStream {
    router: Arc<RouterRuntime>,
    pending: Option<(JsValue, TurnIdentity)>,
    signal: Option<ToolRunContext>,
    events: Option<mpsc::UnboundedReceiver<JsValue>>,
    outcome: Option<mpsc::UnboundedReceiver<Result<(), RouterExecuteError>>>,
    awaiting_pull: bool,
    /// The per-stream interrupt of the provider request.
    abort: AbortFlag,
    gate: YieldGate,
    body: Option<tokio::task::JoinHandle<()>>,
    forwarder: Option<tokio::task::JoinHandle<()>>,
}

impl AuxiliaryStream {
    /// The tool call's `AbortSignal` as the router's turn flag, when it is one.
    fn turn_abort(&self) -> Option<AbortFlag> {
        self.signal
            .as_ref()
            .and_then(|context| context.cancellation().cloned())
    }

    fn aborted(&self) -> bool {
        self.signal.as_ref().is_some_and(ToolRunContext::is_aborted)
    }

    fn start(&mut self) {
        let Some((request, turn)) = self.pending.take() else {
            return;
        };
        let turn_abort = self.turn_abort();
        // The abort listener sits on the provider request from the start, not
        // on the consumer's pulls.
        if let Some(context) = self.signal.clone() {
            if context.is_aborted() {
                self.abort.abort();
            } else {
                let child = self.abort.clone();
                self.forwarder = Some(tokio::spawn(async move {
                    context.cancelled().await;
                    child.abort();
                }));
            }
        }
        let ctx = RouterExecuteContext {
            session_id: turn.session_id.clone(),
            turn_id: turn.turn_id,
            project_path: turn.project_path,
            turn_abort: turn_abort.clone(),
            stream_options: StreamOptions {
                abort: Some(self.abort.clone()),
                yield_gate: Some(self.gate.clone()),
                ..Default::default()
            },
            ..Default::default()
        };
        let input = RouterDecisionInput {
            request,
            session_id: turn.session_id,
            is_main_agent: false,
            abort: turn_abort,
            metadata: None,
        };
        let (sender, receiver) = mpsc::unbounded_channel();
        let (outcome_sender, outcome) = mpsc::unbounded_channel();
        let router = self.router.clone();
        let gate = self.gate.clone();
        self.events = Some(receiver);
        self.outcome = Some(outcome);
        self.body = Some(tokio::spawn(async move {
            let mut sink = move |event: JsValue| {
                gate.deliver();
                let _ = sender.send(event);
            };
            let _ = outcome_sender.send(router.stream(&input, &ctx, &mut sink).await);
        }));
    }

    /// What the generator threw, as the value `web_fetch`'s `catch` sees.
    fn thrown(&self, error: RouterExecuteError) -> SecondaryModelEvent {
        // Once the signal fired, the router rethrows the abort: the provider
        // read rejects with `signal.reason` and an event after the abort
        // throws `createAbortError(reason)` (`RouterRuntime.ts:1187-1199`,
        // `1274-1278`); either way the message is the reason text.
        if self.aborted() {
            let reason = self.signal.as_ref().and_then(ToolRunContext::abort_reason);
            return match reason {
                Some(reason) if !reason.is_empty() => SecondaryModelEvent::Throw {
                    is_error: false,
                    value: JsValue::String(reason),
                },
                _ => thrown("This operation was aborted"),
            };
        }
        SecondaryModelEvent::Throw {
            is_error: true,
            value: JsValue::Object(thrown_value(error)),
        }
    }
}

/// The `Error` a router failure is in the baseline, with the own properties
/// `web_fetch` reads (`message`, `code`, the nested canonical `error`).
fn thrown_value(error: RouterExecuteError) -> JsObject {
    match error {
        // `RouterRuntimeError` / `ModelRequestError`: `code` + message.
        RouterExecuteError::Router(error) => {
            error_object(error.message, &[("code", JsValue::from(error.code))])
        }
        RouterExecuteError::Model(ModelStreamError::Request { code, message, .. }) => {
            error_object(message, &[("code", JsValue::String(code))])
        }
        // `ModelProviderError`: the canonical error rides on `.error`.
        RouterExecuteError::Model(ModelStreamError::Provider(canonical)) => {
            let message = canonical
                .get("message")
                .and_then(JsValue::as_string)
                .cloned()
                .unwrap_or_default();
            error_object(message, &[("error", JsValue::Object(canonical))])
        }
        RouterExecuteError::Model(ModelStreamError::Thrown { message, .. }) => {
            error_object(message, &[])
        }
    }
}

impl Drop for AuxiliaryStream {
    fn drop(&mut self) {
        self.abort.abort();
        if let Some(forwarder) = self.forwarder.take() {
            forwarder.abort();
        }
        if let Some(body) = self.body.take() {
            body.abort();
        }
    }
}

impl SecondaryStream for AuxiliaryStream {
    fn next(&mut self) -> SecondaryNext<'_> {
        Box::pin(async move {
            self.start();
            let result = std::future::poll_fn(|cx| {
                if self.awaiting_pull {
                    self.awaiting_pull = false;
                    self.gate.pull();
                }
                if let Some(events) = self.events.as_mut() {
                    match events.poll_recv(cx) {
                        Poll::Ready(Some(event)) => {
                            self.awaiting_pull = true;
                            return Poll::Ready(Ok(Some(event)));
                        }
                        Poll::Ready(None) => self.events = None,
                        Poll::Pending => return Poll::Pending,
                    }
                }
                match self.outcome.as_mut() {
                    Some(outcome) => match outcome.poll_recv(cx) {
                        Poll::Ready(Some(result)) => {
                            self.outcome = None;
                            Poll::Ready(result.map(|()| None))
                        }
                        Poll::Ready(None) => {
                            self.outcome = None;
                            Poll::Ready(Ok(None))
                        }
                        Poll::Pending => Poll::Pending,
                    },
                    None => Poll::Ready(Ok(None)),
                }
            })
            .await;
            match result {
                Ok(Some(event)) => Some(SecondaryModelEvent::Event(event)),
                Ok(None) => None,
                Err(error) => Some(self.thrown(error)),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::model::router::RouterRuntimeError;

    #[tokio::test]
    async fn unbound_port_throws_like_the_native_adapter() {
        let port = AuxiliaryModelSlot::default().unbound();
        let port = port
            .downcast_ref::<WebFetchModelPort>()
            .expect("web_fetch port");
        let mut stream = port.0.stream(JsValue::Undefined, None);
        let Some(SecondaryModelEvent::Throw { is_error, value }) = stream.next().await else {
            panic!("expected a throw");
        };
        assert!(is_error);
        assert_eq!(
            value.get("message").and_then(JsValue::as_string),
            Some(&JsString::from(
                "Native auxiliary model requires a turn binding."
            ))
        );
        assert!(stream.next().await.is_none());
    }

    #[test]
    fn router_errors_keep_code_and_canonical_error() {
        let router = thrown_value(RouterExecuteError::Router(RouterRuntimeError::new(
            "SDK_MANAGED_MODEL_DENIED",
            "denied",
        )));
        assert_eq!(
            router.get("code"),
            Some(&JsValue::from("SDK_MANAGED_MODEL_DENIED"))
        );
        assert_eq!(router.get("message"), Some(&JsValue::from("denied")));
        let mut canonical = JsObject::new();
        canonical.insert("message".into(), "boom".into());
        canonical.insert("code".into(), "provider_error".into());
        let provider = thrown_value(RouterExecuteError::Model(ModelStreamError::Provider(
            canonical.clone(),
        )));
        assert_eq!(provider.get("message"), Some(&JsValue::from("boom")));
        assert_eq!(provider.get("error"), Some(&JsValue::Object(canonical)));
        let thrown = thrown_value(RouterExecuteError::Model(ModelStreamError::Thrown {
            name: "TypeError".into(),
            message: "terminated".into(),
        }));
        assert_eq!(thrown.get("message"), Some(&JsValue::from("terminated")));
        assert_eq!(thrown.get("name"), None);
    }
}
