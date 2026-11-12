//! Native Home Assistant WebSocket channel, fixed `cc7c4e6` behavior.
//! The official loader supplies only `url` and `token`; direct constructors
//! additionally support the baseline's prefixes, title and session mapper.
pub mod mapper;
pub mod render;
mod socket;
#[cfg(test)]
mod tests;

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::FutureExt;
use pilotdeck_protocol::{JsObject, JsString, JsValue, stringify_json};
use tokio::sync::{mpsc, oneshot, watch};

use super::{
    im_cron::{CronResultDelivery, deliver_chat_cron_result},
    im_interaction::{JsInteractionPort, ResponderRef, js_helper_interaction},
    network::ChannelNetwork,
    protocol::{
        BoxFuture, ChannelAdapter, ChannelError, ChannelHandle, ChannelKey, ChannelLog,
        ChannelStartDeps, TurnEventStream, TurnGateway, TurnRequest,
    },
    sms::{EnvLookup, js_string_of, nullish, trim_units, truthy},
};
pub use mapper::HomeAssistantSessionMapper;

const AUTH_TIMEOUT: Duration = Duration::from_secs(20);
const SERVICE_TIMEOUT: Duration = Duration::from_secs(10);
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct HomeAssistantChannelOptions {
    pub url: Option<JsValue>,
    pub token: Option<JsValue>,
    pub watch_prefixes: Option<Vec<JsString>>,
    pub notification_title: Option<JsString>,
    pub mapper: Option<HomeAssistantSessionMapper>,
    pub env: Option<EnvLookup>,
}

struct Runtime {
    mapper: HomeAssistantSessionMapper,
    active: HashSet<JsString>,
    read_batch_depth: usize,
    completed_in_batch: HashSet<JsString>,
    first_repolls: Vec<oneshot::Receiver<()>>,
    seen_chats: HashSet<JsString>,
    socket: Option<mpsc::UnboundedSender<String>>,
    pending: HashMap<u64, oneshot::Sender<bool>>,
    next_id: f64,
    ready: bool,
    closed: bool,
    auth: Option<oneshot::Sender<bool>>,
    stop: Option<watch::Sender<bool>>,
    logger: Option<ChannelLog>,
    gateway: Option<Arc<dyn TurnGateway>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    outbound: Vec<tokio::task::JoinHandle<()>>,
    network: Option<tokio::task::JoinHandle<()>>,
    failed_streams: Vec<(Box<dyn TurnEventStream>, ChannelError)>,
    stop_errors: Vec<ChannelError>,
}

struct Shared {
    runtime: Mutex<Runtime>,
    url: JsValue,
    token: JsValue,
    prefixes: Vec<JsString>,
    title: Option<JsString>,
    interactions: Arc<dyn JsInteractionPort>,
    drain: tokio::sync::Mutex<()>,
    admission: Mutex<()>,
    shutdown: Mutex<Option<ShutdownJob>>,
    network: Arc<ChannelNetwork>,
}

struct ShutdownJob {
    result: watch::Receiver<Option<Result<(), ChannelError>>>,
    _worker: tokio::task::JoinHandle<()>,
}

struct ShutdownCompletion {
    shared: Arc<Shared>,
    sender: watch::Sender<Option<Result<(), ChannelError>>>,
    completed: bool,
}
impl ShutdownCompletion {
    fn finish(mut self, result: Result<(), ChannelError>) {
        self.sender.send_replace(Some(result));
        self.completed = true;
    }
}
impl Drop for ShutdownCompletion {
    fn drop(&mut self) {
        if !self.completed {
            let error = ChannelError::error(
                "homeassistant: shutdown worker ended before cleanup completed",
            );
            self.shared
                .runtime
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .stop_errors
                .push(error.clone());
            self.sender.send_replace(Some(Err(error)));
        }
    }
}

impl Shared {
    fn read_batch(self: &Arc<Self>) -> ReadBatch {
        self.runtime.lock().expect("HA runtime").read_batch_depth += 1;
        ReadBatch(self.clone())
    }
    async fn finish_read_batch(&self, batch: ReadBatch) {
        // A compressed frame's asynchronous decode boundary ends the Node
        // callback batch. Let each admitted continuation run its first poll,
        // including empty ready streams, before consuming the next batch.
        // A held stream signals after returning Pending, not after completion.
        drop(batch);
        let first_repolls = {
            let mut runtime = self.runtime.lock().expect("HA runtime");
            if runtime.read_batch_depth == 0 {
                std::mem::take(&mut runtime.first_repolls)
            } else {
                Vec::new()
            }
        };
        for receipt in first_repolls {
            // The registered JoinHandle retains ownership. A panic drops the
            // sender, releases this wait, and remains visible to stop/reaping.
            let _ = receipt.await;
        }
    }
    fn log(&self, level: &str, message: impl AsRef<str>) {
        let logger = self.runtime.lock().expect("HA runtime").logger.clone();
        if let Some(logger) = logger {
            logger(level, message.as_ref());
        }
    }

    fn next_id(&self) -> f64 {
        let mut runtime = self.runtime.lock().expect("HA runtime");
        let id = runtime.next_id;
        runtime.next_id += 1.0;
        id
    }

    fn settle_auth(&self, ok: bool) {
        if let Some(sender) = self.runtime.lock().expect("HA runtime").auth.take() {
            let _ = sender.send(ok);
        }
    }

    fn send_json(&self, value: JsValue) -> bool {
        let Ok(Some(raw)) = stringify_json(&value) else {
            return false;
        };
        self.runtime
            .lock()
            .expect("HA runtime")
            .socket
            .as_ref()
            .is_some_and(|socket| socket.send(raw).is_ok())
    }

    fn disconnect(&self) -> bool {
        let mut runtime = self.runtime.lock().expect("HA runtime");
        runtime.socket = None;
        for (_, pending) in runtime.pending.drain() {
            let _ = pending.send(false);
        }
        if let Some(auth) = runtime.auth.take() {
            let _ = auth.send(false);
        }
        runtime.ready && !runtime.closed
    }

    fn stop(&self) {
        let _admission = self.admission.lock().expect("HA admission");
        let mut runtime = self.runtime.lock().expect("HA runtime");
        runtime.closed = true;
        runtime.ready = false;
        runtime.socket = None;
        if let Some(stop) = runtime.stop.take() {
            let _ = stop.send(true);
        }
        if let Some(auth) = runtime.auth.take() {
            let _ = auth.send(false);
        }
        for (_, pending) in runtime.pending.drain() {
            let _ = pending.send(false);
        }
    }

    async fn stop_and_drain(self: &Arc<Self>) -> Result<(), ChannelError> {
        self.stop();
        let mut result = {
            let mut shutdown = self.shutdown.lock().expect("HA shutdown");
            // A dropped caller relinquishes only its receipt. The cleanup
            // worker keeps all join handles and exact stream ownership.
            if let Some(job) = shutdown.as_ref() {
                let settled = job.result.borrow().clone();
                if matches!(settled, Some(Ok(()))) {
                    return Ok(());
                }
                if settled.is_none() {
                    Some(job.result.clone())
                } else {
                    None
                }
            } else {
                None
            }
            .unwrap_or_else(|| {
                let (sender, result) = watch::channel(None);
                let completion = ShutdownCompletion {
                    shared: self.clone(),
                    sender,
                    completed: false,
                };
                let shared = self.clone();
                let worker = tokio::spawn(async move {
                    let outcome = shared.drain_once().await;
                    completion.finish(outcome);
                });
                *shutdown = Some(ShutdownJob {
                    result: result.clone(),
                    _worker: worker,
                });
                result
            })
        };
        loop {
            if let Some(outcome) = result.borrow().clone() {
                return outcome;
            }
            if result.changed().await.is_err() {
                return Err(ChannelError::error(
                    "homeassistant: shutdown worker completion unavailable",
                ));
            }
        }
    }

    async fn drain_once(&self) -> Result<(), ChannelError> {
        self.stop();
        let _drain = self.drain.lock().await;
        let retry = std::mem::take(&mut self.runtime.lock().expect("HA runtime").failed_streams);
        for (mut stream, _) in retry {
            if let Err(error) = stream
                .cancel_and_drain("homeassistant channel stopped".into())
                .await
            {
                self.runtime
                    .lock()
                    .expect("HA runtime")
                    .failed_streams
                    .push((stream, error));
            }
        }
        let tasks = std::mem::take(&mut self.runtime.lock().expect("HA runtime").tasks);
        for task in tasks {
            if let Err(error) = task.await {
                self.log(
                    "error",
                    format!("homeassistant: message task failed: {error}"),
                );
                self.runtime
                    .lock()
                    .expect("HA runtime")
                    .stop_errors
                    .push(ChannelError::error(format!(
                        "homeassistant message task failed: {error}"
                    )));
            }
        }
        let outbound = std::mem::take(&mut self.runtime.lock().expect("HA runtime").outbound);
        for operation in outbound {
            if let Err(error) = operation.await {
                self.runtime
                    .lock()
                    .expect("HA runtime")
                    .stop_errors
                    .push(ChannelError::error(format!(
                        "homeassistant outbound task failed: {error}"
                    )));
            }
        }
        let network = self.runtime.lock().expect("HA runtime").network.take();
        if let Some(network) = network {
            if let Err(error) = network.await {
                self.runtime
                    .lock()
                    .expect("HA runtime")
                    .stop_errors
                    .push(ChannelError::error(format!(
                        "homeassistant socket task failed: {error}"
                    )));
            }
        }
        let chats = {
            let mut runtime = self.runtime.lock().expect("HA runtime");
            let errors: Vec<_> = runtime
                .stop_errors
                .iter()
                .cloned()
                .chain(
                    runtime
                        .failed_streams
                        .iter()
                        .map(|(_, error)| error.clone()),
                )
                .collect();
            if !errors.is_empty() {
                return Err(ChannelError::Aggregate {
                    message: "homeassistant: stop failed".into(),
                    errors,
                });
            }
            std::mem::take(&mut runtime.seen_chats)
        };
        for chat in chats {
            self.interactions.clear_chat_for_shutdown(&chat);
        }
        Ok(())
    }

    fn dispatch(self: &Arc<Self>, chat: JsString, text: JsString) {
        let _admission = self.admission.lock().expect("HA admission");
        if self.stopping() {
            return;
        }
        self.runtime
            .lock()
            .expect("HA runtime")
            .seen_chats
            .insert(chat.clone());
        // JS invokes the async handler synchronously until its first await.
        // Admit/map the first frame before a later frame can win on another
        // runtime worker. The spawned task repolls and installs its own waker.
        let mut incoming = Box::pin(self.clone().incoming(chat, text));
        if let Poll::Pending = incoming
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            let mut runtime = self.runtime.lock().expect("HA runtime");
            let errors = reap_finished(&mut runtime.tasks, "message");
            runtime.stop_errors.extend(errors);
            let mut first_repoll = if runtime.read_batch_depth > 0 {
                let (sender, receipt) = oneshot::channel();
                runtime.first_repolls.push(receipt);
                Some(sender)
            } else {
                None
            };
            runtime
                .tasks
                .push(tokio::spawn(futures_util::future::poll_fn(
                    move |context| {
                        let result = incoming.as_mut().poll(context);
                        if let Some(sender) = first_repoll.take() {
                            let _ = sender.send(());
                        }
                        result
                    },
                )));
        }
    }

    fn stopping(&self) -> bool {
        self.runtime.lock().expect("HA runtime").closed
    }

    fn send_reply(self: &Arc<Self>, chat: JsString, text: JsString) -> BoxFuture<bool> {
        // Registration and stop's closed gate use this same mutex. A caller
        // owns only the receipt; this adapter owns the WS acknowledgement job.
        let mut runtime = self.runtime.lock().expect("HA runtime");
        let Some(socket) = runtime.socket.clone().filter(|_| !runtime.closed) else {
            drop(runtime);
            self.log(
                "warn",
                format!(
                    "homeassistant: not connected, cannot send to {}",
                    display(&chat)
                ),
            );
            return Box::pin(async { false });
        };
        let title = self.title.clone().unwrap_or_else(|| {
            JsString::from_utf16(
                "Gateway · "
                    .encode_utf16()
                    .chain(chat.units().iter().copied())
                    .collect(),
            )
        });
        let id = runtime.next_id;
        runtime.next_id += 1.0;
        let (sender, receiver) = oneshot::channel();
        runtime.pending.insert(id.to_bits(), sender);
        let deadline = tokio::time::Instant::now() + SERVICE_TIMEOUT;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let frame = object([
            ("id", JsValue::Number(id)),
            ("type", "call_service".into()),
            ("domain", "persistent_notification".into()),
            ("service", "create".into()),
            (
                "service_data",
                object([
                    ("title", JsValue::String(title)),
                    ("message", JsValue::String(text)),
                    (
                        "notification_id",
                        JsValue::String(format!("gw_{now}").into()),
                    ),
                ]),
            ),
        ]);
        let sent = stringify_json(&frame)
            .ok()
            .flatten()
            .is_some_and(|raw| socket.send(raw).is_ok());
        if !sent {
            runtime.pending.remove(&id.to_bits());
            return Box::pin(async { false });
        }
        let errors = reap_finished(&mut runtime.outbound, "outbound");
        runtime.stop_errors.extend(errors);
        let (done, receipt) = oneshot::channel();
        let guard = PendingGuard {
            shared: self.clone(),
            id: id.to_bits(),
        };
        runtime.outbound.push(tokio::spawn(async move {
            let delivered = tokio::time::timeout_at(deadline, receiver)
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(false);
            drop(guard);
            let _ = done.send(delivered);
        }));
        Box::pin(async move { receipt.await.unwrap_or(false) })
    }

    async fn incoming(self: Arc<Self>, chat: JsString, text: JsString) {
        let (gateway, mut stopped) = {
            let runtime = self.runtime.lock().expect("HA runtime");
            if runtime.closed {
                return;
            }
            (
                runtime.gateway.clone(),
                runtime.stop.as_ref().expect("started HA").subscribe(),
            )
        };
        if self.interactions.elicitation_pending(&chat) && gateway.is_some() {
            let answer = tokio::select! { _ = stopped.wait_for(|value| *value) => return, answer = self.interactions.elicitation_answer(&chat, &text) => answer };
            match answer {
                Ok(Some(confirmation)) if !confirmation.is_empty() => {
                    self.send_reply(chat, confirmation).await;
                }
                Ok(_) => {}
                Err(error) => self.log(
                    "error",
                    format!("homeassistant: elicitation answer error: {error}"),
                ),
            }
            return;
        }
        if self.interactions.permission_pending(&chat) && gateway.is_some() {
            tokio::select! { _ = stopped.wait_for(|value| *value) => {}, _ = self.answer_permission(&chat, &text) => {} }
            return;
        }
        let mapped = {
            let mut runtime = self.runtime.lock().expect("HA runtime");
            if runtime.closed {
                return;
            }
            if runtime.active.contains(&chat) {
                drop(runtime);
                self.log(
                    "info",
                    format!(
                        "homeassistant: chat {} already active, skipping",
                        display(&chat)
                    ),
                );
                return;
            }
            let mapped = runtime.mapper.resolve(&chat, &text);
            if !mapped.message.is_empty() {
                runtime.active.insert(chat.clone());
            }
            mapped
        };
        if mapped.command_new && mapped.message.is_empty() {
            self.send_reply(chat, "已创建新会话。".into()).await;
            return;
        }
        if mapped.message.is_empty() {
            return;
        }
        let _active = ActiveGuard {
            shared: self.clone(),
            chat: chat.clone(),
        };
        let Some(gateway) = gateway else {
            return;
        };
        if self.stopping() {
            return;
        }
        let mut reply = Vec::new();
        let outcome = async {
            let submission = gateway.submit(TurnRequest {
                session_key: mapped.session_key.clone(),
                channel_key: ChannelKey::new("homeassistant"),
                message: mapped.message,
                ..Default::default()
});
            // `for await` yields even for an immediately completed async
            // iterator. Keep the admitted chat active through that boundary.
            tokio::task::yield_now().await;
            let mut stream = tokio::select! {
                biased;
                _ = stopped.wait_for(|value| *value) => return Ok(()),
                stream = submission => stream?,
            };
            loop {
                let next = tokio::select! {
                    biased;
                    _ = stopped.wait_for(|value| *value).map(drop) => {
                        if let Err(error) = stream.cancel_and_drain("homeassistant channel stopped".into()).await {
                            self.runtime.lock().expect("HA runtime").failed_streams.push((stream, error.clone()));
                            return Err(error);
                        }
                        break;
                    }
                    event = stream.next() => event?,
                };
                let Some(event) = next else {
                    break;
                };
                if is_kind(&event, "elicitation_request") {
                    let question =
                        self.interactions
                            .elicitation_capture(&chat, &mapped.session_key, &event);
                    self.send_reply(chat.clone(), question).await;
                } else if is_kind(&event, "permission_request") {
                    if let Some(question) = self
                        .interactions
                        .permission_capture(&chat, &mapped.session_key, &event)
                        .filter(|value| !value.is_empty())
                    {
                        let delivered = self.send_reply(chat.clone(), question).await;
                        self.interactions.permission_confirm_initial(
                            &chat,
                            delivered,
                            event.get("requestId").and_then(JsValue::as_string),
                        );
                    }
                } else if let Some(fragment) = render::render_homeassistant_event(&event) {
                    reply.extend(fragment.units());
                }
            }
            Ok::<_, ChannelError>(())
        }
        .await;
        if let Err(error) = outcome {
            self.log("error", format!("homeassistant: submitTurn error: {error}"));
            reply = "处理消息时发生错误，请重试。".encode_utf16().collect();
        }
        if !self.stopping() {
            self.interactions.elicitation_clear(&chat);
            self.interactions.permission_clear_after_turn(&chat);
        }
        let final_text = trim_units(&reply);
        if !self.stopping() && !final_text.is_empty() {
            self.send_reply(chat, JsString::from_utf16(final_text.to_vec()))
                .await;
        }
    }

    async fn answer_permission(self: &Arc<Self>, chat: &JsString, text: &JsString) {
        let mut guard = PermissionGuard {
            interactions: self.interactions.clone(),
            chat: chat.clone(),
            token: None,
            armed: true,
        };
        let outcome = async {
            let Some(answer) = self.interactions.permission_answer(chat, text).await? else {
                return Ok(());
            };
            guard.token = answer.answer_token;
            let Some(message) = answer.text.filter(|value| !value.is_empty()) else {
                guard.armed = false;
                return Ok(());
            };
            if !self.send_reply(chat.clone(), message).await {
                self.interactions
                    .permission_release_answer(chat, guard.token);
                guard.armed = false;
                return Ok(());
            }
            if !answer.can_advance && !answer.retry_prompt {
                guard.armed = false;
                return Ok(());
            }
            if let Some(next) = self
                .interactions
                .permission_take_next_prompt(chat, guard.token)
                .filter(|value| !value.is_empty())
            {
                let request_id = self
                    .interactions
                    .permission_prompt_request_id(chat, guard.token);
                let delivered = self.send_reply(chat.clone(), next).await;
                self.interactions.permission_confirm_next(
                    chat,
                    delivered,
                    request_id.as_ref(),
                    guard.token,
                );
            }
            guard.armed = false;
            Ok::<_, ChannelError>(())
        }
        .await;
        if let Err(error) = outcome {
            self.log(
                "error",
                format!("homeassistant: permission answer error: {error}"),
            );
        }
    }
}

struct PendingGuard {
    shared: Arc<Shared>,
    id: u64,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.shared
            .runtime
            .lock()
            .expect("HA runtime")
            .pending
            .remove(&self.id);
    }
}
struct ActiveGuard {
    shared: Arc<Shared>,
    chat: JsString,
}
impl Drop for ActiveGuard {
    fn drop(&mut self) {
        let mut runtime = self.shared.runtime.lock().expect("HA runtime");
        if runtime.read_batch_depth > 0 {
            runtime.completed_in_batch.insert(self.chat.clone());
        } else {
            runtime.active.remove(&self.chat);
        }
    }
}
struct ReadBatch(Arc<Shared>);
impl Drop for ReadBatch {
    fn drop(&mut self) {
        let mut runtime = self.0.runtime.lock().expect("HA runtime");
        runtime.read_batch_depth -= 1;
        if runtime.read_batch_depth == 0 {
            for chat in std::mem::take(&mut runtime.completed_in_batch) {
                runtime.active.remove(&chat);
            }
        }
    }
}
struct PermissionGuard {
    interactions: Arc<dyn JsInteractionPort>,
    chat: JsString,
    token: Option<u64>,
    armed: bool,
}
impl Drop for PermissionGuard {
    fn drop(&mut self) {
        if self.armed && self.token.is_some() {
            self.interactions
                .permission_release_answer(&self.chat, self.token);
        }
    }
}
struct StartupGuard(Option<Arc<Shared>>);
impl Drop for StartupGuard {
    fn drop(&mut self) {
        if let Some(shared) = self.0.take() {
            shared.stop();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    if let Err(error) = shared.stop_and_drain().await {
                        shared.log("error", error.to_js_string());
                    }
                });
            }
        }
    }
}

pub struct HomeAssistantChannel {
    key: ChannelKey,
    shared: Arc<Shared>,
}
impl HomeAssistantChannel {
    pub fn new(
        options: HomeAssistantChannelOptions,
        interactions: Arc<dyn JsInteractionPort>,
    ) -> Self {
        Self::new_with_network(options, interactions, Arc::new(ChannelNetwork::new(None)))
    }

    fn new_with_network(
        options: HomeAssistantChannelOptions,
        interactions: Arc<dyn JsInteractionPort>,
        network: Arc<ChannelNetwork>,
    ) -> Self {
        let env = |key: &str| {
            match &options.env {
                Some(lookup) => lookup(key),
                None => std::env::var(key).ok().map(JsString::from),
            }
            .map(JsValue::String)
            .unwrap_or(JsValue::Undefined)
        };
        let url = options
            .url
            .filter(|value| !nullish(value))
            .unwrap_or_else(|| {
                let value = env("HASS_URL");
                if nullish(&value) {
                    "http://127.0.0.1:8123".into()
                } else {
                    value
                }
            });
        let token = options
            .token
            .filter(|value| !nullish(value))
            .unwrap_or_else(|| env("HASS_TOKEN"));
        Self {
            key: ChannelKey::new("homeassistant"),
            shared: Arc::new(Shared {
                runtime: Mutex::new(Runtime {
                    mapper: options.mapper.unwrap_or_default(),
                    active: HashSet::new(),
                    read_batch_depth: 0,
                    completed_in_batch: HashSet::new(),
                    first_repolls: Vec::new(),
                    seen_chats: HashSet::new(),
                    socket: None,
                    pending: HashMap::new(),
                    next_id: 1.0,
                    ready: false,
                    closed: false,
                    auth: None,
                    stop: None,
                    logger: None,
                    gateway: None,
                    tasks: Vec::new(),
                    outbound: Vec::new(),
                    network: None,
                    failed_streams: Vec::new(),
                    stop_errors: Vec::new(),
                }),
                url,
                token,
                prefixes: options
                    .watch_prefixes
                    .filter(|values| !values.is_empty())
                    .unwrap_or_else(|| vec!["conversation.".into()]),
                title: options.notification_title,
                interactions,
                drain: tokio::sync::Mutex::new(()),
                admission: Mutex::new(()),
                shutdown: Mutex::new(None),
                network,
            }),
        }
    }
}

struct NoopHandle;
impl ChannelHandle for NoopHandle {
    fn stop(&self, _: &str) -> BoxFuture<Result<(), ChannelError>> {
        Box::pin(async { Ok(()) })
    }
}
struct Handle(Arc<Shared>);
impl ChannelHandle for Handle {
    fn stop(&self, reason: &str) -> BoxFuture<Result<(), ChannelError>> {
        let shared = self.0.clone();
        shared.log("info", format!("homeassistant: stopping ({reason})"));
        shared.stop();
        Box::pin(async move { shared.stop_and_drain().await })
    }
}

impl ChannelAdapter for HomeAssistantChannel {
    fn channel_key(&self) -> &ChannelKey {
        &self.key
    }
    fn start(
        &self,
        deps: ChannelStartDeps,
    ) -> BoxFuture<Result<Arc<dyn ChannelHandle>, ChannelError>> {
        let shared = self.shared.clone();
        Box::pin(async move {
            {
                let mut shutdown = shared.shutdown.lock().expect("HA shutdown");
                if let Some(job) = shutdown.as_ref() {
                    match job.result.borrow().clone() {
                        None => {
                            return Err(ChannelError::error(
                                "homeassistant: previous shutdown is still running",
                            ));
                        }
                        Some(Err(error)) => return Err(error),
                        Some(Ok(())) => {}
                    }
                }
                *shutdown = None;
            }
            {
                let mut runtime = shared.runtime.lock().expect("HA runtime");
                runtime.logger = deps.log;
                runtime.gateway = deps.turn_gateway;
            }
            if !truthy(&shared.token) {
                shared.log("error", "homeassistant: HASS_TOKEN not set");
                return Ok(Arc::new(NoopHandle) as Arc<dyn ChannelHandle>);
            }
            let url = socket::http_to_ws(&shared.url)?;
            let (auth, auth_result) = oneshot::channel();
            let (stop, stopped) = watch::channel(false);
            {
                let mut runtime = shared.runtime.lock().expect("HA runtime");
                runtime.closed = false;
                runtime.ready = false;
                runtime.auth = Some(auth);
                runtime.stop = Some(stop);
            }
            let mut guard = StartupGuard(Some(shared.clone()));
            let network = tokio::spawn(socket::run(shared.clone(), url.clone(), stopped));
            shared.runtime.lock().expect("HA runtime").network = Some(network);
            let ready = tokio::time::timeout(AUTH_TIMEOUT, auth_result)
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(false);
            shared.runtime.lock().expect("HA runtime").auth = None;
            if !ready {
                shared.log("error", "homeassistant: WebSocket auth failed or timed out");
                shared.stop_and_drain().await?;
                guard.0 = None;
                return Ok(Arc::new(NoopHandle) as Arc<dyn ChannelHandle>);
            }
            shared.log("info", format!("homeassistant: WebSocket {url}"));
            guard.0 = None;
            Ok(Arc::new(Handle(shared)) as Arc<dyn ChannelHandle>)
        })
    }
    fn deliver_cron_result(&self, delivery: &CronResultDelivery) -> BoxFuture<bool> {
        let shared = self.shared.clone();
        deliver_chat_cron_result(delivery, &self.key, move |chat, text| {
            Box::pin(async move { shared.send_reply(chat, text).await })
        })
    }
}

pub fn register(mounts: &mut super::loader::ChannelLoaderMounts, responder: ResponderRef) {
    register_with_network(mounts, responder, Arc::new(ChannelNetwork::new(None)));
}

pub(crate) fn register_with_network(
    mounts: &mut super::loader::ChannelLoaderMounts,
    responder: ResponderRef,
    network: Arc<ChannelNetwork>,
) {
    mounts.register(
        "homeassistant",
        Arc::new(move |options: &JsObject| {
            Ok(Arc::new(HomeAssistantChannel::new_with_network(
                HomeAssistantChannelOptions {
                    url: options.get("url").cloned(),
                    token: options.get("token").cloned(),
                    env: Some(Arc::new(|name| {
                        std::env::var(name).ok().map(JsString::from)
                    })),
                    ..Default::default()
                },
                js_helper_interaction(responder.clone()),
                network.clone(),
            )) as Arc<dyn ChannelAdapter>)
        }),
    );
}

fn object<const N: usize>(values: [(&str, JsValue); N]) -> JsValue {
    let mut object = JsObject::new();
    for (key, value) in values {
        object.insert(key.into(), value);
    }
    JsValue::Object(object)
}
fn reap_finished(tasks: &mut Vec<tokio::task::JoinHandle<()>>, kind: &str) -> Vec<ChannelError> {
    let mut errors = Vec::new();
    tasks.retain_mut(|task| {
        if !task.is_finished() {
            return true;
        }
        match Pin::new(task).poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(Ok(())) => false,
            Poll::Ready(Err(error)) => {
                errors.push(ChannelError::error(format!(
                    "homeassistant {kind} task failed: {error}"
                )));
                false
            }
            Poll::Pending => true,
        }
    });
    errors
}
fn display(value: &JsString) -> String {
    String::from_utf16_lossy(value.units())
}
fn is_kind(event: &JsValue, kind: &str) -> bool {
    event
        .get("type")
        .and_then(JsValue::as_string)
        .is_some_and(|value| value.equals_str(kind))
}
