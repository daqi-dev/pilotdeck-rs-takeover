//! Native Telegram Bot API adapter; baseline TelegramChannel.ts at cc7c4e6.
//! Native HTTP only. The production loader supplies a fresh JS-preserving
//! interaction helper per adapter; inbound turns share the host TurnGateway.
mod mapper;
#[cfg(test)]
mod tests;
mod wire;

pub use mapper::{MappedSession, TelegramSessionMapper, TelegramSessionMapperState};

use crate::channel::{
    ActiveChat,
    im_cron::{CronResultDelivery, deliver_chat_cron_result},
    im_interaction::{JsInteractionPort, ResponderRef, js_helper_interaction},
    protocol::*,
    sms,
};
use crate::runtime::{
    model::http::{
        HyperModelTransport, ModelBody, ModelHttpTransport, TransportFailure, TransportFuture,
        fetch::fetch,
    },
    network::UNDICI_BODY_TIMEOUT,
    tools::builtin::bash::runner::Utf8Stream,
};
use bytes::Bytes;
use futures_util::FutureExt;
use http_body_util::{BodyExt, Full};
use hyper::{HeaderMap, Method, Request, Response};
use pilotdeck_protocol::{JsObject, JsString, JsValue, parse_json, stringify_json};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{oneshot, watch},
    task::JoinHandle,
};

pub const CHANNEL_KEY: &str = "telegram";
pub type Env = Arc<dyn Fn(&str) -> Option<JsString> + Send + Sync>;

#[derive(Default)]
pub struct TelegramChannelOptions {
    pub rest_url: Option<JsString>,
    pub token: Option<JsString>,
    pub webhook_url: Option<JsString>,
    pub mapper: Option<TelegramSessionMapper>,
    pub env: Option<Env>,
    pub transport: Option<Arc<dyn ModelHttpTransport>>,
}

struct ReceiveTask {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}
/// Redirects receive the dispatcher's per-exchange header budget each time.
struct HeaderTransport(Arc<dyn ModelHttpTransport>);
impl ModelHttpTransport for HeaderTransport {
    fn send(&self, request: Request<Full<Bytes>>) -> TransportFuture {
        let exchange = self.0.send(request);
        TransportFuture::io(async move {
            tokio::time::timeout(UNDICI_BODY_TIMEOUT, exchange)
                .await
                .unwrap_or_else(|_| {
                    Err(TransportFailure::fetch_failed(Some(
                        "UND_ERR_HEADERS_TIMEOUT",
                    )))
                })
        })
    }
}
#[derive(Clone)]
struct CleanupOutcome {
    result: Result<(), ChannelError>,
    retryable: bool,
}
struct CleanupCompletion {
    sender: watch::Sender<Option<CleanupOutcome>>,
    finished: bool,
}
impl CleanupCompletion {
    fn finish(mut self, result: Result<(), ChannelError>, retryable: bool) {
        self.sender
            .send_replace(Some(CleanupOutcome { result, retryable }));
        self.finished = true;
    }
}
impl Drop for CleanupCompletion {
    fn drop(&mut self) {
        if !self.finished {
            self.sender.send_replace(Some(CleanupOutcome {
                result: Err(ChannelError::error("telegram: cleanup worker terminated before draining; cleanup ownership is unresolved")),
                retryable: false,
            }));
        }
    }
}
struct CleanupAttempt {
    result: watch::Receiver<Option<CleanupOutcome>>,
    // The worker owns cleanup independently of every stop waiter. Keeping its
    // handle here also makes a worker failure observable through the receipt.
    _worker: JoinHandle<()>,
}
#[derive(Default)]
struct OutboundState {
    accepting: bool,
    tasks: Vec<JoinHandle<()>>,
}
struct State {
    mapper: TelegramSessionMapper,
    recipients: HashMap<JsString, JsString>,
    gateway: Option<Arc<dyn TurnGateway>>,
    logger: Option<ChannelLog>,
}
struct Shared {
    key: ChannelKey,
    rest_url: String,
    token: JsString,
    webhook_url: Option<JsString>,
    transport: Arc<dyn ModelHttpTransport>,
    interaction: Arc<dyn JsInteractionPort>,
    state: Mutex<State>,
    active: Arc<Mutex<HashSet<JsString>>>,
    running: AtomicBool,
    receiver: Mutex<Option<ReceiveTask>>,
    stopping: watch::Sender<bool>,
    handlers: Mutex<Vec<JoinHandle<()>>>,
    outbound: Mutex<OutboundState>,
    cleanup_errors: Mutex<Vec<ChannelError>>,
    failed_streams: Mutex<Vec<Box<dyn TurnEventStream>>>,
    cleanup: Mutex<Option<CleanupAttempt>>,
    fatal_cleanup: AtomicBool,
}

#[derive(Clone)]
pub struct TelegramChannel(Arc<Shared>);

impl TelegramChannel {
    pub fn new(options: TelegramChannelOptions, interaction: Arc<dyn JsInteractionPort>) -> Self {
        let env = options
            .env
            .unwrap_or_else(|| Arc::new(|key| std::env::var(key).ok().map(JsString::from)));
        let rest_url = options
            .rest_url
            .unwrap_or_else(|| JsString::from("https://api.telegram.org"));
        let token = options
            .token
            .or_else(|| env("TELEGRAM_BOT_TOKEN"))
            .unwrap_or_default();
        Self(Arc::new(Shared {
            key: ChannelKey::new(CHANNEL_KEY),
            rest_url: String::from_utf16_lossy(rest_url.units())
                .trim_end_matches('/')
                .to_owned(),
            token,
            webhook_url: options.webhook_url,
            transport: Arc::new(HeaderTransport(
                options
                    .transport
                    .unwrap_or_else(|| Arc::new(HyperModelTransport::new())),
            )),
            interaction,
            state: Mutex::new(State {
                mapper: options.mapper.unwrap_or_default(),
                recipients: HashMap::new(),
                gateway: None,
                logger: None,
            }),
            active: Arc::default(),
            running: AtomicBool::new(false),
            receiver: Mutex::new(None),
            stopping: watch::channel(false).0,
            handlers: Mutex::new(Vec::new()),
            outbound: Mutex::new(OutboundState::default()),
            cleanup_errors: Mutex::new(Vec::new()),
            failed_streams: Mutex::new(Vec::new()),
            cleanup: Mutex::new(None),
            fatal_cleanup: AtomicBool::new(false),
        }))
    }
}

impl Shared {
    fn log(&self, level: &str, text: &str) {
        let logger = self.state.lock().expect("telegram state").logger.clone();
        if let Some(log) = logger {
            log(level, text);
        }
    }
    fn gateway(&self) -> Option<Arc<dyn TurnGateway>> {
        self.state.lock().expect("telegram state").gateway.clone()
    }
    async fn stopped(&self) {
        let mut stop = self.stopping.subscribe();
        if *stop.borrow() {
            return;
        }
        let _ = stop.changed().await;
    }

    async fn request(
        &self,
        method: Method,
        url: &str,
        body: Option<String>,
    ) -> Result<Response<ModelBody>, String> {
        let url = url::Url::parse(url)
            .map_err(|_| format!("TypeError: Failed to parse URL from {url}"))?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(
                "TypeError: Request cannot be constructed from a URL that includes credentials"
                    .into(),
            );
        }
        let mut headers = HeaderMap::new();
        if body.is_some() {
            headers.insert(
                "content-type",
                "application/json".parse().expect("static header"),
            );
        } else {
            headers.insert(
                "accept",
                "text/event-stream, application/json, */*"
                    .parse()
                    .expect("static header"),
            );
        }
        fetch(
            self.transport.as_ref(),
            url,
            method,
            headers,
            Bytes::from(body.unwrap_or_default()),
        )
        .await
        .map_err(|e| format!("TypeError: {}", e.message))
    }

    async fn response_text(mut body: ModelBody) -> Result<String, String> {
        let mut decoder = Utf8Stream::default();
        let mut text = String::new();
        loop {
            let frame = tokio::time::timeout(UNDICI_BODY_TIMEOUT, body.frame())
                .await
                .map_err(|_| "TypeError: terminated".to_owned())?;
            let Some(frame) = frame else {
                break;
            };
            let frame = frame.map_err(|_| "TypeError: terminated".to_owned())?;
            if let Some(bytes) = frame.data_ref() {
                text.push_str(&decoder.decode(bytes));
            }
        }
        text.push_str(&decoder.flush());
        Ok(text)
    }

    async fn api(&self, method: &str, body: JsValue) -> Result<JsValue, String> {
        let token = String::from_utf16_lossy(self.token.units());
        let url = format!("{}/bot{token}/{method}", self.rest_url);
        let body = stringify_json(&body)
            .map_err(|e| format!("{e:?}"))?
            .unwrap_or_default();
        let response = self.request(Method::POST, &url, Some(body)).await?;
        let status = response.status();
        let text = Self::response_text(response.into_body()).await?;
        let value =
            parse_json(&text).map_err(|_| format!("Invalid JSON response from {method}"))?;
        if !status.is_success() || !value.get("ok").is_some_and(sms::truthy) {
            let description = value
                .get("description")
                .map(sms::js_string_of)
                .unwrap_or_else(|| JsString::from(text));
            return Err(String::from_utf16_lossy(description.units()));
        }
        Ok(value.get("result").cloned().unwrap_or(JsValue::Undefined))
    }

    async fn receive(self: Arc<Self>, mut stop: watch::Receiver<bool>) {
        let mut offset = 0.0;
        loop {
            if *stop.borrow() {
                return;
            }
            let body = object(&[
                ("offset", JsValue::Number(offset)),
                ("timeout", JsValue::Number(30.0)),
            ]);
            let updates = tokio::select! { biased; _ = stop.changed() => return, result = self.api("getUpdates", body) => result };
            match updates {
                Ok(JsValue::Array(ref updates)) => {
                    for update in updates {
                        if let Some(JsValue::Number(id)) = update.get("update_id") {
                            offset = id + 1.0;
                        }
                        let update = update.clone();
                        let shared = self.clone();
                        let mut future = Box::pin(async move {
                            shared.parse_update(&update).await;
                        });
                        if future.as_mut().now_or_never().is_none() {
                            let mut handlers = self.handlers.lock().expect("telegram handlers");
                            self.harvest_finished(&mut handlers, "handler");
                            handlers.push(tokio::spawn(future));
                        }
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    self.log("error", &format!("telegram: bot error: {error}"));
                    tokio::select! { biased; _ = stop.changed() => return, _ = tokio::time::sleep(Duration::from_secs(3)) => {} }
                }
            }
        }
    }

    async fn parse_update(self: Arc<Self>, raw: &JsValue) {
        if *self.stopping.borrow() {
            return;
        }
        let Some(message) = raw.get("message") else {
            return;
        };
        let Some(text) = message
            .get("text")
            .and_then(JsValue::as_string)
            .filter(|s| !s.is_empty())
            .cloned()
        else {
            return;
        };
        let Some(id) = message.get("chat").and_then(|c| c.get("id")) else {
            return;
        };
        let chat = sms::js_string_of(id);
        self.state
            .lock()
            .expect("telegram state")
            .recipients
            .insert(chat.clone(), chat.clone());
        let envelope = wire::Envelope { text };
        if self.interaction.elicitation_pending(&chat) && self.gateway().is_some() {
            let answer = tokio::select! { biased; _ = self.stopped() => return, answer = self.interaction.elicitation_answer(&chat, &envelope.text) => answer };
            match answer {
                Ok(Some(text)) if !text.is_empty() => {
                    self.send_reply(&chat, &text).await;
                }
                Ok(_) => {}
                Err(e) => self.log(
                    "error",
                    &format!("telegram: elicitation answer error: {}", e.to_js_string()),
                ),
            }
            return;
        }
        if self.interaction.permission_pending(&chat) && self.gateway().is_some() {
            let answer = tokio::select! { biased; _ = self.stopped() => return, answer = self.interaction.permission_answer(&chat, &envelope.text) => answer };
            match answer {
                Err(e) => self.log(
                    "error",
                    &format!("telegram: permission answer error: {}", e.to_js_string()),
                ),
                Ok(None) => {}
                Ok(Some(answer)) => {
                    if let Some(text) = answer.text.filter(|t| !t.is_empty()) {
                        if !self.send_reply(&chat, &text).await {
                            self.interaction
                                .permission_release_answer(&chat, answer.answer_token);
                            return;
                        }
                        if !answer.can_advance && !answer.retry_prompt {
                            return;
                        }
                        if let Some(prompt) = self
                            .interaction
                            .permission_take_next_prompt(&chat, answer.answer_token)
                            .filter(|p| !p.is_empty())
                        {
                            let id = self
                                .interaction
                                .permission_prompt_request_id(&chat, answer.answer_token);
                            let delivered = self.send_reply(&chat, &prompt).await;
                            self.interaction.permission_confirm_next(
                                &chat,
                                delivered,
                                id.as_ref(),
                                answer.answer_token,
                            );
                        }
                    }
                }
            }
            return;
        }

        enum Admission {
            Busy,
            New,
            Empty,
            Turn(MappedSession, ActiveChat<JsString>),
        }
        let admission = {
            let mut state = self.state.lock().expect("telegram state");
            let mut active = self.active.lock().expect("telegram active chats");
            if active.contains(&chat) {
                Admission::Busy
            } else {
                let mapped = state.mapper.resolve(&chat, &envelope.text);
                if mapped.command_new && mapped.message.is_empty() {
                    Admission::New
                } else if mapped.message.is_empty() {
                    Admission::Empty
                } else {
                    active.insert(chat.clone());
                    Admission::Turn(mapped, ActiveChat::new(self.active.clone(), chat.clone()))
                }
            }
        };
        match admission {
            Admission::Busy => self.log(
                "info",
                &format!(
                    "telegram: chat {} already active, skipping",
                    String::from_utf16_lossy(chat.units())
                ),
            ),
            Admission::New => {
                self.send_reply(&chat, &JsString::from("已创建新会话。"))
                    .await;
            }
            Admission::Empty => {}
            Admission::Turn(mapped, _guard) => self.process_message(&chat, mapped).await,
        }
    }

    async fn process_message(self: &Arc<Self>, chat: &JsString, mapped: MappedSession) {
        let Some(gateway) = self.gateway() else {
            return;
        };
        {
            let mut outbound = self.outbound.lock().expect("telegram outbound");
            if outbound.accepting && !*self.stopping.borrow() {
                let shared = self.clone();
                let chat = chat.clone();
                outbound.tasks.push(tokio::spawn(async move {
                    let body = object(&[("chat_id", JsValue::String(chat)), ("action", JsValue::from("typing"))]);
                    tokio::select! { biased; _ = shared.stopped() => {}, _ = shared.api("sendChatAction", body) => {} }
                }));
            }
        }
        let mut reply = Vec::new();
        let result = async {
            let request = TurnRequest { session_key: mapped.session_key.clone(), channel_key: self.key.clone(), message: mapped.message, ..Default::default() };
            let mut stream = tokio::select! { biased; _ = self.stopped() => return Ok(()), stream = gateway.submit(request) => stream? };
            // A resolved JS async iterator still resumes in a microtask.
            // Keep admission armed while dispatching the rest of this chunk.
            tokio::task::yield_now().await;
            loop {
                let event = tokio::select! {
                    biased;
                    _ = self.stopped() => {
                        // This stream owns exactly one accepted turn. Never
                        // cancel by session key: a later turn may already own it.
                        if let Err(error) = stream.cancel_and_drain(JsString::from("telegram channel stopped")).await {
                            self.cleanup_errors.lock().expect("telegram cleanup errors").push(error.clone());
                            self.failed_streams.lock().expect("telegram failed streams").push(stream);
                            return Err(error);
                        }
                        break;
                    },
                    event = stream.next() => event?,
                };
                let Some(event) = event else { break; };
                let kind = event.get("type").and_then(JsValue::as_string);
                if kind.is_some_and(|s| s.equals_str("elicitation_request")) {
                    let prompt = self.interaction.elicitation_capture(chat, &mapped.session_key, &event);
                    self.send_reply(chat, &prompt).await;
                } else if kind.is_some_and(|s| s.equals_str("permission_request")) {
                    if let Some(prompt) = self.interaction.permission_capture(chat, &mapped.session_key, &event).filter(|p| !p.is_empty()) {
                        let delivered = self.send_reply(chat, &prompt).await;
                        self.interaction.permission_confirm_initial(chat, delivered, event.get("requestId").and_then(JsValue::as_string));
                    }
                } else if let Some(fragment) = sms::render::render_sms_event(&event) { reply.extend_from_slice(fragment.units()); }
            }
            Ok::<_, ChannelError>(())
        }.await;
        if result.is_err() && *self.stopping.borrow() {
            // A failed cancel retains interaction state for the host's error
            // path; it must not masquerade as a successfully drained channel.
            return;
        }
        if let Err(error) = result {
            self.log(
                "error",
                &format!("telegram: submitTurn error: {}", error.to_js_string()),
            );
            reply = "处理消息时发生错误，请重试。".encode_utf16().collect();
        }
        self.interaction.elicitation_clear(chat);
        self.interaction.permission_clear_after_turn(chat);
        let reply = sms::trim_units(&reply);
        if !reply.is_empty() {
            self.send_reply(chat, &JsString::from_utf16(reply.to_vec()))
                .await;
        }
    }

    fn harvest_finished(&self, tasks: &mut Vec<JoinHandle<()>>, role: &str) {
        let mut index = 0;
        while index < tasks.len() {
            if !tasks[index].is_finished() {
                index += 1;
                continue;
            }
            let mut task = tasks.swap_remove(index);
            match (&mut task).now_or_never() {
                Some(Ok(())) => {}
                Some(Err(error)) => {
                    // This runs under the owner registry lock, before stop
                    // can observe an empty registry and publish completion.
                    self.fatal_cleanup.store(true, Ordering::Release);
                    self.cleanup_errors
                        .lock()
                        .expect("telegram cleanup errors")
                        .push(ChannelError::error(format!(
                            "telegram: {role} cleanup failed: {error}"
                        )));
                }
                None => {
                    tasks.push(task);
                    break;
                }
            }
        }
    }

    async fn send_reply(self: &Arc<Self>, chat: &JsString, text: &JsString) -> bool {
        let receipt = {
            // Admission and registration share stop's synchronous boundary.
            // An external cron caller owns only the receipt, never the HTTP
            // exchange or the sole capability for waiting for its release.
            let mut outbound = self.outbound.lock().expect("telegram outbound");
            if !outbound.accepting || *self.stopping.borrow() {
                return false;
            }
            self.harvest_finished(&mut outbound.tasks, "outbound");
            let recipient = self
                .state
                .lock()
                .expect("telegram state")
                .recipients
                .get(chat)
                .cloned()
                .unwrap_or_else(|| chat.clone());
            if recipient.is_empty() {
                drop(outbound);
                self.log(
                    "warn",
                    &format!(
                        "telegram: no recipient for {}, cannot send",
                        String::from_utf16_lossy(chat.units())
                    ),
                );
                return false;
            }
            let (finished, receipt) = oneshot::channel();
            let shared = self.clone();
            let text = text.clone();
            outbound.tasks.push(tokio::spawn(async move {
                let delivered = shared.send_reply_owned(&recipient, &text).await;
                // Dropping the external receipt does not cancel this owner.
                let _ = finished.send(delivered);
            }));
            receipt
        };
        // A panicked worker closes the receipt and its JoinError remains in
        // the registry, making subsequent stop explicitly fail.
        receipt.await.unwrap_or(false)
    }

    async fn send_reply_owned(&self, recipient: &JsString, text: &JsString) -> bool {
        if !self.running.load(Ordering::Acquire) || *self.stopping.borrow() {
            return false;
        }
        let mut ok = true;
        for chunk in wire::chunk_text(text) {
            let body = object(&[
                ("chat_id", JsValue::String(recipient.clone())),
                ("text", JsValue::String(chunk)),
            ]);
            let result = tokio::select! { biased; _ = self.stopped() => return false, result = self.api("sendMessage", body) => result };
            if let Err(error) = result {
                self.log("error", &format!("telegram: sendMessage failed: {error}"));
                ok = false;
            }
        }
        ok
    }
}

struct Handle(Arc<Shared>);
struct NoopHandle;
impl ChannelHandle for NoopHandle {
    fn stop(&self, _reason: &str) -> BoxFuture<Result<(), ChannelError>> {
        Box::pin(async { Ok(()) })
    }
}
impl Shared {
    async fn cleanup_owned(self: Arc<Self>, retry: bool) -> Result<(), ChannelError> {
        let retry_streams = if retry {
            std::mem::take(&mut *self.failed_streams.lock().expect("telegram failed streams"))
        } else {
            Vec::new()
        };
        let receiver = self.receiver.lock().expect("telegram receiver").take();
        if let Some(receiver) = receiver
            && let Err(error) = receiver.task.await
        {
            self.fatal_cleanup.store(true, Ordering::Release);
            self.cleanup_errors
                .lock()
                .expect("telegram cleanup errors")
                .push(ChannelError::error(format!(
                    "telegram: receiver cleanup failed: {error}"
                )));
        }
        let handlers = std::mem::take(&mut *self.handlers.lock().expect("telegram handlers"));
        for handler in handlers {
            if let Err(error) = handler.await {
                self.fatal_cleanup.store(true, Ordering::Release);
                self.log(
                    "error",
                    &format!("telegram: handler cleanup failed: {error}"),
                );
                self.cleanup_errors
                    .lock()
                    .expect("telegram cleanup errors")
                    .push(ChannelError::error(format!(
                        "telegram: handler cleanup failed: {error}"
                    )));
            }
        }
        let outbound = std::mem::take(&mut self.outbound.lock().expect("telegram outbound").tasks);
        for task in outbound {
            if let Err(error) = task.await {
                self.fatal_cleanup.store(true, Ordering::Release);
                self.cleanup_errors
                    .lock()
                    .expect("telegram cleanup errors")
                    .push(ChannelError::error(format!(
                        "telegram: outbound cleanup failed: {error}"
                    )));
            }
        }
        for mut stream in retry_streams {
            if let Err(error) = stream
                .cancel_and_drain(JsString::from("telegram channel stopped"))
                .await
            {
                self.cleanup_errors
                    .lock()
                    .expect("telegram cleanup errors")
                    .push(error);
                self.failed_streams
                    .lock()
                    .expect("telegram failed streams")
                    .push(stream);
            }
        }
        let errors =
            std::mem::take(&mut *self.cleanup_errors.lock().expect("telegram cleanup errors"));
        if !errors.is_empty() {
            return Err(ChannelError::Aggregate {
                message: "telegram: owned turn cleanup failed".into(),
                errors,
            });
        }
        let chats: Vec<_> = self
            .state
            .lock()
            .expect("telegram state")
            .recipients
            .keys()
            .cloned()
            .collect();
        for chat in chats {
            self.interaction.clear_chat_for_shutdown(&chat);
        }
        Ok(())
    }

    fn cleanup_receipt(self: &Arc<Self>) -> watch::Receiver<Option<CleanupOutcome>> {
        let mut attempt = self.cleanup.lock().expect("telegram cleanup attempt");
        let retry = attempt.is_some();
        if let Some(current) = attempt.as_ref() {
            // Pending work is always shared. A completed failure retains its
            // owned streams in Shared and a subsequent stop retries them.
            if !current
                .result
                .borrow()
                .as_ref()
                .is_some_and(|outcome| outcome.retryable)
            {
                return current.result.clone();
            }
        }
        let (completed, result) = watch::channel(None);
        let shared = self.clone();
        // Construct the guard before spawning: abort before the worker's first
        // poll must also publish a terminal error instead of an empty receipt.
        let completion = CleanupCompletion {
            sender: completed,
            finished: false,
        };
        let worker = tokio::spawn(async move {
            let outcome = shared.clone().cleanup_owned(retry).await;
            let can_retry = outcome.is_err()
                && !shared.fatal_cleanup.load(Ordering::Acquire)
                && !shared
                    .failed_streams
                    .lock()
                    .expect("telegram failed streams")
                    .is_empty();
            completion.finish(outcome, can_retry);
        });
        *attempt = Some(CleanupAttempt {
            result: result.clone(),
            _worker: worker,
        });
        result
    }
}
impl ChannelHandle for Handle {
    fn stop(&self, reason: &str) -> BoxFuture<Result<(), ChannelError>> {
        // This prefix is deliberately synchronous: merely obtaining and
        // dropping the returned future must already stop accepting messages.
        self.0.outbound.lock().expect("telegram outbound").accepting = false;
        self.0.running.store(false, Ordering::Release);
        self.0.stopping.send_replace(true);
        if let Some(receiver) = self.0.receiver.lock().expect("telegram receiver").as_ref() {
            let _ = receiver.stop.send(true);
        }
        self.0
            .log("info", &format!("telegram: stopping ({reason})"));
        let mut receipt = self.0.cleanup_receipt();
        Box::pin(async move {
            loop {
                if let Some(outcome) = receipt.borrow().clone() {
                    return outcome.result;
                }
                receipt.changed().await.map_err(|_| {
                    ChannelError::error("telegram: cleanup worker exited without a result")
                })?;
            }
        })
    }
}

impl ChannelAdapter for TelegramChannel {
    fn channel_key(&self) -> &ChannelKey {
        &self.0.key
    }
    fn start(
        &self,
        deps: ChannelStartDeps,
    ) -> BoxFuture<Result<Arc<dyn ChannelHandle>, ChannelError>> {
        let shared = self.0.clone();
        Box::pin(async move {
            {
                let mut cleanup = shared.cleanup.lock().expect("telegram cleanup attempt");
                if let Some(attempt) = cleanup.as_ref() {
                    if !attempt
                        .result
                        .borrow()
                        .as_ref()
                        .is_some_and(|outcome| outcome.result.is_ok())
                    {
                        return Err(ChannelError::error("telegram: prior stop has not drained"));
                    }
                    *cleanup = None;
                }
            }
            {
                let mut state = shared.state.lock().expect("telegram state");
                state.gateway = deps.turn_gateway;
                state.logger = deps.log;
            }
            if shared.token.is_empty() {
                shared.log("error", "telegram: TELEGRAM_BOT_TOKEN not set");
                return Ok(Arc::new(NoopHandle) as Arc<dyn ChannelHandle>);
            }
            let setup = if let Some(url) = shared.webhook_url.as_ref().filter(|s| !s.is_empty()) {
                shared
                    .api(
                        "setWebhook",
                        object(&[("url", JsValue::String(url.clone()))]),
                    )
                    .await
            } else {
                shared.api("deleteWebhook", object(&[])).await
            };
            if let Err(error) = setup {
                shared.log("error", &format!("telegram: start failed: {error}"));
                return Ok(Arc::new(NoopHandle) as Arc<dyn ChannelHandle>);
            }
            let me = shared.api("getMe", object(&[])).await;
            match me {
                Ok(me) => shared.log(
                    "info",
                    &format!(
                        "telegram: connected as @{}",
                        String::from_utf16_lossy(
                            sms::js_string_of(me.get("username").unwrap_or(&JsValue::Undefined))
                                .units()
                        )
                    ),
                ),
                Err(error) => {
                    shared.log("error", &format!("telegram: start failed: {error}"));
                    return Ok(Arc::new(NoopHandle) as Arc<dyn ChannelHandle>);
                }
            }
            let (stop, rx) = watch::channel(false);
            shared.stopping.send_replace(false);
            shared.running.store(true, Ordering::Release);
            shared.outbound.lock().expect("telegram outbound").accepting = true;
            if let Some(url) = shared.webhook_url.as_ref().filter(|s| !s.is_empty()) {
                shared.log(
                    "info",
                    &format!(
                        "telegram: webhook mode at {}",
                        String::from_utf16_lossy(url.units())
                    ),
                );
            } else {
                let task = tokio::spawn(shared.clone().receive(rx));
                *shared.receiver.lock().expect("telegram receiver") =
                    Some(ReceiveTask { stop, task });
                shared.log("info", "telegram: long-polling started");
            }
            Ok(Arc::new(Handle(shared)) as Arc<dyn ChannelHandle>)
        })
    }
    fn deliver_cron_result(&self, delivery: &CronResultDelivery) -> BoxFuture<bool> {
        let shared = self.0.clone();
        deliver_chat_cron_result(delivery, &self.0.key, move |chat, text| {
            Box::pin(async move { shared.send_reply(&chat, &text).await })
        })
    }
}

/// Each loader construction gets its own helper state; the responder remains
/// the one native gateway owner shared by the host.
pub fn register(mounts: &mut crate::channel::loader::ChannelLoaderMounts, responder: ResponderRef) {
    register_transport(mounts, responder, None);
}

pub(crate) fn register_with_network(
    mounts: &mut crate::channel::loader::ChannelLoaderMounts,
    responder: ResponderRef,
    network: Arc<crate::channel::network::ChannelNetwork>,
) {
    register_transport(mounts, responder, Some(network));
}

fn register_transport(
    mounts: &mut crate::channel::loader::ChannelLoaderMounts,
    responder: ResponderRef,
    transport: Option<Arc<dyn ModelHttpTransport>>,
) {
    mounts.register(
        CHANNEL_KEY,
        Arc::new(move |options: &JsObject| {
            Ok(Arc::new(TelegramChannel::new(
                TelegramChannelOptions {
                    token: options.get("token").and_then(JsValue::as_string).cloned(),
                    webhook_url: options
                        .get("webhookUrl")
                        .and_then(JsValue::as_string)
                        .cloned(),
                    transport: transport.clone(),
                    ..Default::default()
                },
                js_helper_interaction(responder.clone()),
            )) as Arc<dyn ChannelAdapter>)
        }),
    );
}

fn object(fields: &[(&str, JsValue)]) -> JsValue {
    let mut object = JsObject::new();
    for (key, value) in fields {
        object.insert((*key).into(), value.clone());
    }
    JsValue::Object(object)
}
