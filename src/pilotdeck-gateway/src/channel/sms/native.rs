//! Production bridge. Legacy HTTP/helper tests keep their injected ports;
//! native adapters own every accepted turn, handler and provider exchange.
use super::*;
use crate::channel::{im_interaction::ResponderRef, protocol};
use futures_util::FutureExt;
use std::{future::Future, panic::AssertUnwindSafe};
use tokio::{sync::oneshot, task::JoinHandle};

type Stream = Arc<Mutex<Option<Box<dyn protocol::TurnEventStream>>>>;
struct Work {
    accepting: bool,
    tasks: Vec<JoinHandle<()>>,
}
struct Attempt {
    result: watch::Receiver<Option<Result<(), ChannelError>>>,
    _owner: JoinHandle<()>,
}
pub(super) struct NativeRuntime {
    responder: ResponderRef,
    gateway: StdMutex<Option<Arc<dyn protocol::TurnGateway>>>,
    streams: StdMutex<Vec<Stream>>,
    work: StdMutex<Work>,
    stop: watch::Sender<bool>,
    cleanup: StdMutex<Option<Attempt>>,
    fatal: StdMutex<Vec<ChannelError>>,
}
impl NativeRuntime {
    pub(super) fn new(responder: ResponderRef) -> Arc<Self> {
        Arc::new(Self {
            responder,
            gateway: StdMutex::new(None),
            streams: StdMutex::new(Vec::new()),
            work: StdMutex::new(Work {
                accepting: false,
                tasks: Vec::new(),
            }),
            stop: watch::channel(false).0,
            cleanup: StdMutex::new(None),
            fatal: StdMutex::new(Vec::new()),
        })
    }
    pub(super) fn bind(
        &self,
        gateway: Option<Arc<dyn protocol::TurnGateway>>,
    ) -> Result<(), ChannelError> {
        if let Some(attempt) = self.cleanup.lock().unwrap().as_ref()
            && !attempt.result.borrow().as_ref().is_some_and(Result::is_ok)
        {
            return Err(ChannelError::error("sms: prior stop has not drained"));
        }
        *self.cleanup.lock().unwrap() = None;
        *self.gateway.lock().unwrap() = gateway;
        self.stop.send_replace(false);
        self.work.lock().unwrap().accepting = true;
        Ok(())
    }
    pub(super) fn is_stopping(&self) -> bool {
        *self.stop.borrow()
    }
    async fn stopped(&self) {
        let mut rx = self.stop.subscribe();
        if !*rx.borrow() {
            let _ = rx.changed().await;
        }
    }
    pub(super) fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) -> bool {
        let mut work = self.work.lock().unwrap();
        if !work.accepting {
            return false;
        }
        // Finished owners are observed before release, including panic errors.
        let mut index = 0;
        while index < work.tasks.len() {
            if work.tasks[index].is_finished() {
                let mut task = work.tasks.swap_remove(index);
                if let Some(Err(error)) = (&mut task).now_or_never() {
                    self.fatal.lock().unwrap().push(ChannelError::error(format!(
                        "sms: owned worker failed: {error}"
                    )));
                }
            } else {
                index += 1;
            }
        }
        work.tasks.push(tokio::spawn(future));
        true
    }
    pub(super) async fn send(
        self: &Arc<Self>,
        client: Arc<dyn SmsSender>,
        message: SmsOutgoingMessage,
    ) -> Result<bool, ChannelError> {
        let (tx, rx) = oneshot::channel();
        let owned = self.clone();
        if !self.spawn(async move {
            let result=tokio::select! { biased; _=owned.stopped()=>Ok(false), result=client.send(message)=>result.map(|()|true) };
            let _=tx.send(result);
        }) { return Ok(false); }
        rx.await
            .map_err(|_| ChannelError::error("sms: outbound owner terminated"))?
    }
    async fn drain(&self) -> Result<(), ChannelError> {
        let mut errors = Vec::new();
        let tasks = std::mem::take(&mut self.work.lock().unwrap().tasks);
        for task in tasks {
            if let Err(error) = task.await {
                self.fatal.lock().unwrap().push(ChannelError::error(format!(
                    "sms: owned worker failed: {error}"
                )));
            }
        }
        let streams = self.streams.lock().unwrap().clone();
        for stream in streams {
            let mut stream = stream.lock().await;
            if let Some(owner) = stream.as_mut() {
                match owner.cancel_and_drain("sms channel stopped".into()).await {
                    Ok(()) => {
                        *stream = None;
                    }
                    Err(error) => errors.push(error),
                }
            }
        }
        errors.extend(self.fatal.lock().unwrap().clone());
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ChannelError::Aggregate {
                message: "sms: owned cleanup failed".into(),
                errors,
            })
        }
    }
    pub(super) fn stop_owned(
        self: Arc<Self>,
        shared: Arc<SmsShared>,
        server: Option<ServerEntry>,
        reason: String,
    ) -> BoxFuture<Result<(), ChannelError>> {
        self.work.lock().unwrap().accepting = false;
        self.stop.send_replace(true);
        if let Some(server) = &server {
            server.shutdown.send_replace(true);
        }
        let mut cleanup = self.cleanup.lock().unwrap();
        let retry = cleanup
            .as_ref()
            .is_some_and(|a| a.result.borrow().as_ref().is_some_and(Result::is_err));
        if cleanup.is_none() || retry {
            let (tx, rx) = watch::channel(None);
            let owned = self.clone();
            let owner = tokio::spawn(async move {
                let result = AssertUnwindSafe(async {
                    shared.log("info", format!("sms: stopping ({reason})"));
                    if let Some(server) = server
                        && let Err(error) = server.task.await
                    {
                        owned
                            .fatal
                            .lock()
                            .unwrap()
                            .push(ChannelError::error(format!(
                                "sms: HTTP owner failed: {error}"
                            )));
                    }
                    let result = owned.drain().await;
                    if result.is_ok() {
                        let mut runtime = shared.runtime.lock().await;
                        runtime.client = None;
                        runtime.elicitation = ImElicitationHelper::default();
                        *shared.lock_permissions() = ImPermissionHelper::default();
                    }
                    result
                })
                .catch_unwind()
                .await
                .unwrap_or_else(|_| {
                    let error = ChannelError::error("sms: cleanup owner panicked");
                    owned
                        .fatal
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(error.clone());
                    Err(error)
                });
                tx.send_replace(Some(result));
            });
            *cleanup = Some(Attempt {
                result: rx,
                _owner: owner,
            });
        }
        let mut result = cleanup.as_ref().unwrap().result.clone();
        Box::pin(async move {
            loop {
                if let Some(result) = result.borrow().clone() {
                    return result;
                }
                result
                    .changed()
                    .await
                    .map_err(|_| ChannelError::error("sms: cleanup owner exited without result"))?;
            }
        })
    }
}
struct Events {
    stream: Stream,
    native: Arc<NativeRuntime>,
}
impl gateway::TurnEventStream for Events {
    fn next_event(&mut self) -> BoxFuture<Option<Result<JsValue, ChannelError>>> {
        let stream = self.stream.clone();
        let native = self.native.clone();
        Box::pin(async move {
            let mut guard = stream.lock().await;
            let owner = guard.as_mut()?;
            let event = tokio::select! { biased; _=native.stopped()=>return None, result=owner.next()=>result };
            match event {
                Ok(None) => {
                    *guard = None;
                    None
                }
                Ok(Some(event)) => Some(Ok(event)),
                Err(error) => {
                    if owner
                        .cancel_and_drain("sms turn stream failed".into())
                        .await
                        .is_ok()
                    {
                        *guard = None;
                    }
                    Some(Err(error))
                }
            }
        })
    }
}
impl SmsGateway for Arc<NativeRuntime> {
    fn submit_turn(
        &self,
        request: SubmitTurnRequest,
    ) -> BoxFuture<Result<Box<dyn gateway::TurnEventStream>, ChannelError>> {
        let native = self.clone();
        let gateway = self.gateway.lock().unwrap().clone();
        Box::pin(async move {
            let gateway = gateway.ok_or_else(|| ChannelError::error("Gateway not ready"))?;
            let request = protocol::TurnRequest {
                session_key: request.session_key,
                channel_key: ChannelKey::new(request.channel_key),
                message: request.message,
                ..Default::default()
            };
            let owner = tokio::select! {biased; _=native.stopped()=>return Err(ChannelError::error("sms channel stopped")), result=gateway.submit(request)=>result?};
            let stream = Arc::new(Mutex::new(Some(owner)));
            {
                let mut streams = native.streams.lock().unwrap();
                streams.retain(|stream| match stream.try_lock() {
                    Ok(owner) => owner.is_some(),
                    Err(_) => true,
                });
                streams.push(stream.clone());
            }
            Ok(Box::new(Events { stream, native }) as Box<dyn gateway::TurnEventStream>)
        })
    }
    fn respond_elicitation(
        &self,
        response: ElicitationResponse,
    ) -> BoxFuture<Result<(), ChannelError>> {
        let native = self.clone();
        Box::pin(async move {
            tokio::select! {biased; _=native.stopped()=>Err(ChannelError::error("sms channel stopped")),result=native.responder.respond_elicitation(response)=>result}
        })
    }
    fn permission_decide(
        &self,
        decision: PermissionDecision,
    ) -> BoxFuture<Result<PermissionDecisionOutcome, ChannelError>> {
        let native = self.clone();
        Box::pin(async move {
            tokio::select! {biased; _=native.stopped()=>Err(ChannelError::error("sms channel stopped")),result=native.responder.permission_decide(decision)=>result}
        })
    }
}
