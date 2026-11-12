//! Production native Slack Socket Mode and Web API adapter, fixed TS cc7c4e6.
//! Shared HTTP/WS network dispatcher. The production loader supplies a fresh JS-preserving
//! interaction helper per adapter; inbound turns share the host TurnGateway.
use super::{
    mapper::{MappedSession, SlackSessionMapper},
    render::render_slack_event,
};
#[cfg(test)]
mod tests;

use crate::channel::{
    ActiveChat,
    im_interaction::{JsInteractionPort, ResponderRef, js_helper_interaction},
    protocol::*,
    sms,
};
use crate::channel::{network::ChannelNetwork, ws_transport::ChannelWebSocket};
use crate::runtime::{
    model::http::{ModelBody, ModelHttpTransport, TransportFailure, TransportFuture, fetch::fetch},
    network::UNDICI_BODY_TIMEOUT,
    tools::builtin::bash::runner::Utf8Stream,
};
use bytes::Bytes;
use futures_util::{FutureExt, SinkExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::{HeaderMap, Method, Request, Response};
use pilotdeck_protocol::{JsObject, JsString, JsValue, parse_json, stringify_json};
use std::{
    collections::HashSet,
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
use tokio_tungstenite::tungstenite::Message;

const CHANNEL_KEY: &str = "slack";
type Env = Arc<dyn Fn(&str) -> Option<JsString> + Send + Sync>;

#[derive(Default)]
struct NativeSlackChannelOptions {
    bot_token: Option<JsValue>,
    app_token: Option<JsValue>,
    mapper: Option<SlackSessionMapper>,
    env: Option<Env>,
    transport: Option<Arc<dyn ModelHttpTransport>>,
    api_base: Option<String>,
    network: Option<Arc<ChannelNetwork>>,
}

#[derive(Clone, Debug)]
struct SendContext {
    channel: JsString,
    thread: JsValue,
}
impl SendContext {
    fn helper_chat(&self) -> JsString {
        super::thread_chat_id(&self.channel, &self.thread).unwrap_or_else(|| self.channel.clone())
    }
}
fn message(raw: &JsValue, own: Option<&JsString>) -> Option<(SendContext, JsString, JsString)> {
    if raw.get("bot_id").is_some_and(sms::truthy) {
        return None;
    }
    if raw
        .get("subtype")
        .and_then(JsValue::as_string)
        .is_some_and(|s| {
            ["bot_message", "message_changed", "message_deleted"]
                .iter()
                .any(|kind| s.equals_str(kind))
        })
    {
        return None;
    }
    if own.is_some_and(|id| raw.get("user").and_then(JsValue::as_string) == Some(id)) {
        return None;
    }
    let channel = raw
        .get("channel")
        .filter(|v| sms::truthy(v))
        .map(sms::js_string_of)?;
    let text = raw
        .get("text")
        .filter(|v| !sms::nullish(v))
        .map(sms::js_string_of)
        .unwrap_or_default();
    let text = super::strip_mentions(text.units());
    let text = JsString::from_utf16(sms::trim_units(&text).to_vec());
    if text.is_empty() {
        return None;
    }
    let thread = raw
        .get("thread_ts")
        .filter(|v| !sms::nullish(v))
        .cloned()
        .unwrap_or(JsValue::Undefined);
    let ts = raw.get("ts").cloned().unwrap_or(JsValue::Undefined);
    let context = SendContext {
        channel: channel.clone(),
        thread,
    };
    let chat = if context.thread != ts {
        context.helper_chat()
    } else {
        channel
    };
    Some((context, chat, text))
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
                result: Err(ChannelError::error("slack: cleanup worker terminated before draining; cleanup ownership is unresolved")),
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
    mapper: SlackSessionMapper,
    user_id: Option<JsString>,
    chats: HashSet<JsString>,
    gateway: Option<Arc<dyn TurnGateway>>,
    logger: Option<ChannelLog>,
}
struct Shared {
    key: ChannelKey,
    bot_token: JsValue,
    app_token: JsValue,
    api_base: String,
    network: Arc<ChannelNetwork>,
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
struct NativeSlackChannel(Arc<Shared>);

impl NativeSlackChannel {
    fn new(options: NativeSlackChannelOptions, interaction: Arc<dyn JsInteractionPort>) -> Self {
        let env = options
            .env
            .unwrap_or_else(|| Arc::new(|key| std::env::var(key).ok().map(JsString::from)));
        let bot_token = options
            .bot_token
            .filter(|v| !sms::nullish(v))
            .or_else(|| env("SLACK_BOT_TOKEN").map(JsValue::String))
            .unwrap_or(JsValue::Undefined);
        let app_token = options
            .app_token
            .filter(|v| !sms::nullish(v))
            .or_else(|| env("SLACK_APP_TOKEN").map(JsValue::String))
            .unwrap_or(JsValue::Undefined);
        let network = options.network.unwrap_or_default();
        Self(Arc::new(Shared {
            key: ChannelKey::new(CHANNEL_KEY),
            bot_token,
            app_token,
            api_base: options
                .api_base
                .unwrap_or_else(|| "https://slack.com/api".into())
                .trim_end_matches('/')
                .to_owned(),
            transport: Arc::new(HeaderTransport(
                options.transport.unwrap_or_else(|| network.clone()),
            )),
            network,
            interaction,
            state: Mutex::new(State {
                mapper: options.mapper.unwrap_or_default(),
                user_id: None,
                chats: HashSet::new(),
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
        let logger = self.state.lock().expect("slack state").logger.clone();
        if let Some(log) = logger {
            log(level, text);
        }
    }
    fn gateway(&self) -> Option<Arc<dyn TurnGateway>> {
        self.state.lock().expect("slack state").gateway.clone()
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
        headers.insert(
            "authorization",
            format!(
                "Bearer {}",
                String::from_utf16_lossy(
                    sms::js_string_of(if url.path().ends_with("/apps.connections.open") {
                        &self.app_token
                    } else {
                        &self.bot_token
                    })
                    .units()
                )
            )
            .parse()
            .map_err(|_| "invalid access token header".to_owned())?,
        );
        if body.is_some() {
            headers.insert(
                "content-type",
                "application/json".parse().expect("static header"),
            );
        } else {
            headers.insert("accept", "application/json".parse().expect("static header"));
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

    async fn json_request(
        &self,
        method: Method,
        path: &str,
        body: Option<String>,
    ) -> Result<JsValue, String> {
        let response = self
            .request(method, &format!("{}{}", self.api_base, path), body)
            .await?;
        let status = response.status();
        let text = Self::response_text(response.into_body()).await?;
        if !status.is_success() {
            return Err(format!("HTTP {}: {}", status.as_u16(), text));
        }
        parse_json(&text).map_err(|e| e.to_string())
    }

    async fn api(&self, path: &str, body: JsObject) -> Result<JsValue, String> {
        let body = stringify_json(&JsValue::Object(body))
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "JSON.stringify returned undefined".to_owned())?;
        let payload = self.json_request(Method::POST, path, Some(body)).await?;
        if !payload.get("ok").is_some_and(sms::truthy) {
            let error = payload.get("error").cloned().unwrap_or(JsValue::Undefined);
            return Err(format!(
                "Error: {}",
                String::from_utf16_lossy(sms::js_string_of(&error).units())
            ));
        }
        Ok(payload)
    }

    async fn open_socket(&self) -> Result<ChannelWebSocket, String> {
        let mut body = JsObject::new();
        body.insert("token".into(), self.app_token.clone());
        let opened = self.api("/apps.connections.open", body).await?;
        let url = opened
            .get("url")
            .and_then(JsValue::as_string)
            .ok_or_else(|| "Socket Mode URL missing".to_owned())?;
        let url = String::from_utf16_lossy(url.units());
        tokio::time::timeout(
            Duration::from_secs(30),
            self.network.connect_ws(url.as_str()),
        )
        .await
        .map_err(|_| "Socket Mode handshake timeout".to_owned())?
        .map(|(socket, _)| socket)
        .map_err(|e| e.to_string())
    }

    async fn ws_send(&self, socket: &mut ChannelWebSocket, message: Message) -> bool {
        tokio::select! { biased; _ = self.stopped() => false,
        result = tokio::time::timeout(Duration::from_secs(30), socket.send(message)) => matches!(result, Ok(Ok(()))) }
    }

    async fn receive(
        self: Arc<Self>,
        mut socket: ChannelWebSocket,
        mut stop: watch::Receiver<bool>,
    ) {
        loop {
            loop {
                let frame = tokio::select! { biased;
                    _ = stop.changed() => { let _ = tokio::time::timeout(Duration::from_secs(2), socket.close(None)).await; return; },
                    frame = socket.next() => frame,
                };
                let Some(frame) = frame else {
                    break;
                };
                let packet = match frame {
                    Ok(Message::Text(text)) => match parse_json(text.as_str()) {
                        Ok(packet) => packet,
                        Err(_) => continue,
                    },
                    Ok(Message::Ping(bytes)) => {
                        if !self.ws_send(&mut socket, Message::Pong(bytes)).await {
                            break;
                        }
                        continue;
                    }
                    Ok(Message::Close(_)) => break,
                    Err(error) => {
                        self.log("error", &format!("slack: bolt error: {error}"));
                        break;
                    }
                    _ => continue,
                };
                let kind = packet.get("type").and_then(JsValue::as_string);
                if kind.is_some_and(|s| s.equals_str("disconnect")) {
                    break;
                }
                if let Some(id) = packet.get("envelope_id").and_then(JsValue::as_string) {
                    let mut ack = JsObject::new();
                    ack.insert("envelope_id".into(), JsValue::String(id.clone()));
                    let ack = stringify_json(&JsValue::Object(ack)).unwrap().unwrap();
                    if !self.ws_send(&mut socket, Message::Text(ack.into())).await {
                        break;
                    }
                }
                let native = kind.is_some_and(|s| s.equals_str("events_api"));
                let legacy = kind.is_some_and(|s| s.equals_str("message"));
                if !native && !legacy {
                    continue;
                }
                let Some(event) = packet.get("payload").and_then(|v| v.get("event")) else {
                    continue;
                };
                if native
                    && !event
                        .get("type")
                        .and_then(JsValue::as_string)
                        .is_some_and(|s| s.equals_str("message"))
                {
                    continue;
                }
                let shared = self.clone();
                let event = event.clone();
                let mut future = Box::pin(async move {
                    shared.parse_message(event).await;
                });
                if future.as_mut().now_or_never().is_none() {
                    let mut handlers = self.handlers.lock().expect("slack handlers");
                    self.harvest_finished(&mut handlers, "handler");
                    handlers.push(tokio::spawn(future));
                }
            }
            loop {
                tokio::select! { biased; _ = stop.changed() => return, _ = tokio::time::sleep(Duration::from_secs(1)) => {} }
                let opened = tokio::select! { biased; _ = stop.changed() => return, opened = self.open_socket() => opened };
                match opened {
                    Ok(next) => {
                        socket = next;
                        break;
                    }
                    Err(error) => self.log("error", &format!("slack: reconnect failed: {error}")),
                }
            }
        }
    }

    async fn parse_message(self: Arc<Self>, raw: JsValue) {
        if *self.stopping.borrow() {
            return;
        }
        let own_id = self.state.lock().expect("slack state").user_id.clone();
        let Some((context, chat, text)) = message(&raw, own_id.as_ref()) else {
            return;
        };
        self.state
            .lock()
            .expect("slack state")
            .chats
            .insert(chat.clone());
        if self.interaction.elicitation_pending(&chat) && self.gateway().is_some() {
            let answer = tokio::select! { biased; _ = self.stopped() => return, answer = self.interaction.elicitation_answer(&chat, &text) => answer };
            match answer {
                Ok(Some(text)) if !text.is_empty() => {
                    self.send_reply(&context, &text).await;
                }
                Ok(_) => {}
                Err(e) => self.log(
                    "error",
                    &format!("slack: elicitation answer error: {}", e.to_js_string()),
                ),
            }
            return;
        }
        if self.interaction.permission_pending(&chat) && self.gateway().is_some() {
            let answer = tokio::select! { biased; _ = self.stopped() => return, answer = self.interaction.permission_answer(&chat, &text) => answer };
            match answer {
                Err(e) => self.log(
                    "error",
                    &format!("slack: permission answer error: {}", e.to_js_string()),
                ),
                Ok(None) => {}
                Ok(Some(answer)) => {
                    if let Some(text) = answer.text.filter(|t| !t.is_empty()) {
                        if !self.send_reply(&context, &text).await {
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
                            let delivered = self.send_reply(&context, &prompt).await;
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
            let mut state = self.state.lock().expect("slack state");
            let mut active = self.active.lock().expect("slack active chats");
            if active.contains(&chat) {
                Admission::Busy
            } else {
                let mapped = state.mapper.resolve(&chat, &text);
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
                    "slack: chat {} already active, skipping",
                    String::from_utf16_lossy(chat.units())
                ),
            ),
            Admission::New => {
                self.send_reply(&context, &JsString::from("已创建新会话。"))
                    .await;
            }
            Admission::Empty => {}
            Admission::Turn(mapped, _guard) => self.process_message(&context, mapped).await,
        }
    }

    async fn process_message(self: &Arc<Self>, context: &SendContext, mapped: MappedSession) {
        let chat = context.helper_chat();
        self.state
            .lock()
            .expect("slack state")
            .chats
            .insert(chat.clone());
        let chat = &chat;
        let Some(gateway) = self.gateway() else {
            return;
        };
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
                        if let Err(error) = stream.cancel_and_drain(JsString::from("slack channel stopped")).await {
                            self.cleanup_errors.lock().expect("slack cleanup errors").push(error.clone());
                            self.failed_streams.lock().expect("slack failed streams").push(stream);
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
                    self.send_reply(context, &prompt).await;
                } else if kind.is_some_and(|s| s.equals_str("permission_request")) {
                    if let Some(prompt) = self.interaction.permission_capture(chat, &mapped.session_key, &event).filter(|p| !p.is_empty()) {
                        let delivered = self.send_reply(context, &prompt).await;
                        self.interaction.permission_confirm_initial(chat, delivered, event.get("requestId").and_then(JsValue::as_string));
                    }
                } else if let Some(fragment) = render_slack_event(&event) { reply.extend_from_slice(fragment.units()); }
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
                &format!("slack: submitTurn error: {}", error.to_js_string()),
            );
            reply = "处理消息时发生错误，请重试。".encode_utf16().collect();
        }
        self.interaction.elicitation_clear(chat);
        self.interaction.permission_clear_after_turn(chat);
        let reply = sms::trim_units(&reply);
        if !reply.is_empty() {
            self.send_reply(context, &JsString::from_utf16(reply.to_vec()))
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
                        .expect("slack cleanup errors")
                        .push(ChannelError::error(format!(
                            "slack: {role} cleanup failed: {error}"
                        )));
                }
                None => {
                    tasks.push(task);
                    break;
                }
            }
        }
    }

    async fn send_reply(self: &Arc<Self>, context: &SendContext, text: &JsString) -> bool {
        let receipt = {
            let mut outbound = self.outbound.lock().expect("slack outbound");
            if !outbound.accepting || *self.stopping.borrow() {
                return false;
            }
            self.harvest_finished(&mut outbound.tasks, "outbound");
            let (finished, receipt) = oneshot::channel();
            let shared = self.clone();
            let context = context.clone();
            let text = text.clone();
            outbound.tasks.push(tokio::spawn(async move {
                let delivered = shared.send_reply_owned(&context, &text).await;
                let _ = finished.send(delivered);
            }));
            receipt
        };
        receipt.await.unwrap_or(false)
    }

    async fn send_reply_owned(&self, context: &SendContext, text: &JsString) -> bool {
        if !self.running.load(Ordering::Acquire) || *self.stopping.borrow() {
            return false;
        }
        let formatted = super::format_slack_mrkdwn(text);
        let mut delivered = true;
        for chunk in super::chunk_text(formatted.units(), 39000) {
            let mut body = JsObject::new();
            body.insert("channel".into(), JsValue::String(context.channel.clone()));
            body.insert("text".into(), JsValue::String(JsString::from_utf16(chunk)));
            body.insert("mrkdwn".into(), JsValue::Bool(true));
            if sms::truthy(&context.thread) {
                body.insert("thread_ts".into(), context.thread.clone());
            }
            let result = tokio::select! { biased; _ = self.stopped() => return false, result = self.api("/chat.postMessage", body) => result };
            if let Err(error) = result {
                self.log("error", &format!("slack: postMessage failed: {error}"));
                delivered = false;
            }
        }
        delivered
    }
}

struct Handle(Arc<Shared>);
// A cancelled startup must release its already-open receiver independently
// of the caller that dropped `start()` while waiting for authentication.
struct StartupOwner {
    shared: Arc<Shared>,
    committed: bool,
}
impl Drop for StartupOwner {
    fn drop(&mut self) {
        if !self.committed {
            drop(Handle(self.shared.clone()).stop("startup cancelled"));
        }
    }
}

struct NoopHandle;
impl ChannelHandle for NoopHandle {
    fn stop(&self, _reason: &str) -> BoxFuture<Result<(), ChannelError>> {
        Box::pin(async { Ok(()) })
    }
}
impl Shared {
    async fn cleanup_owned(self: Arc<Self>, retry: bool) -> Result<(), ChannelError> {
        let retry_streams = if retry {
            std::mem::take(&mut *self.failed_streams.lock().expect("slack failed streams"))
        } else {
            Vec::new()
        };
        let receiver = self.receiver.lock().expect("slack receiver").take();
        if let Some(receiver) = receiver {
            if let Err(error) = receiver.task.await {
                self.fatal_cleanup.store(true, Ordering::Release);
                self.cleanup_errors
                    .lock()
                    .expect("slack cleanup errors")
                    .push(ChannelError::error(format!(
                        "slack: receiver cleanup failed: {error}"
                    )));
            }
        }
        let handlers = std::mem::take(&mut *self.handlers.lock().expect("slack handlers"));
        for handler in handlers {
            if let Err(error) = handler.await {
                self.fatal_cleanup.store(true, Ordering::Release);
                self.log("error", &format!("slack: handler cleanup failed: {error}"));
                self.cleanup_errors
                    .lock()
                    .expect("slack cleanup errors")
                    .push(ChannelError::error(format!(
                        "slack: handler cleanup failed: {error}"
                    )));
            }
        }
        let outbound = std::mem::take(&mut self.outbound.lock().expect("slack outbound").tasks);
        for task in outbound {
            if let Err(error) = task.await {
                self.fatal_cleanup.store(true, Ordering::Release);
                self.cleanup_errors
                    .lock()
                    .expect("slack cleanup errors")
                    .push(ChannelError::error(format!(
                        "slack: outbound cleanup failed: {error}"
                    )));
            }
        }
        for mut stream in retry_streams {
            if let Err(error) = stream
                .cancel_and_drain(JsString::from("slack channel stopped"))
                .await
            {
                self.cleanup_errors
                    .lock()
                    .expect("slack cleanup errors")
                    .push(error);
                self.failed_streams
                    .lock()
                    .expect("slack failed streams")
                    .push(stream);
            }
        }
        let errors =
            std::mem::take(&mut *self.cleanup_errors.lock().expect("slack cleanup errors"));
        if !errors.is_empty() {
            return Err(ChannelError::Aggregate {
                message: "slack: owned turn cleanup failed".into(),
                errors,
            });
        }
        let chats: Vec<_> = self
            .state
            .lock()
            .expect("slack state")
            .chats
            .iter()
            .cloned()
            .collect();
        for chat in chats {
            self.interaction.clear_chat_for_shutdown(&chat);
        }
        self.state.lock().expect("slack state").user_id = None;
        Ok(())
    }

    fn cleanup_receipt(self: &Arc<Self>) -> watch::Receiver<Option<CleanupOutcome>> {
        let mut attempt = self.cleanup.lock().expect("slack cleanup attempt");
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
                    .expect("slack failed streams")
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
        self.0.outbound.lock().expect("slack outbound").accepting = false;
        self.0.running.store(false, Ordering::Release);
        self.0.stopping.send_replace(true);
        if let Some(receiver) = self.0.receiver.lock().expect("slack receiver").as_ref() {
            let _ = receiver.stop.send(true);
        }
        self.0.log("info", &format!("slack: stopping ({reason})"));
        let mut receipt = self.0.cleanup_receipt();
        Box::pin(async move {
            loop {
                if let Some(outcome) = receipt.borrow().clone() {
                    return outcome.result;
                }
                receipt.changed().await.map_err(|_| {
                    ChannelError::error("slack: cleanup worker exited without a result")
                })?;
            }
        })
    }
}

impl ChannelAdapter for NativeSlackChannel {
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
                let mut cleanup = shared.cleanup.lock().expect("slack cleanup attempt");
                if let Some(attempt) = cleanup.as_ref() {
                    if !attempt
                        .result
                        .borrow()
                        .as_ref()
                        .is_some_and(|outcome| outcome.result.is_ok())
                    {
                        return Err(ChannelError::error("slack: prior stop has not drained"));
                    }
                    *cleanup = None;
                }
            }
            {
                let mut state = shared.state.lock().expect("slack state");
                state.gateway = deps.turn_gateway;
                state.logger = deps.log;
            }
            if !sms::truthy(&shared.bot_token) {
                shared.log("error", "slack: SLACK_BOT_TOKEN not set");
                return Ok(Arc::new(NoopHandle) as Arc<dyn ChannelHandle>);
            }
            if !sms::truthy(&shared.app_token) {
                shared.log(
                    "error",
                    "slack: SLACK_APP_TOKEN not set (required for Socket Mode)",
                );
                return Ok(Arc::new(NoopHandle) as Arc<dyn ChannelHandle>);
            }
            shared.stopping.send_replace(false);
            let socket = match shared.open_socket().await {
                Ok(socket) => socket,
                Err(error) => {
                    shared.log("error", &format!("slack: start failed: {error}"));
                    return Ok(Arc::new(NoopHandle) as Arc<dyn ChannelHandle>);
                }
            };
            let (stop, rx) = watch::channel(false);
            shared.running.store(true, Ordering::Release);
            shared.outbound.lock().expect("slack outbound").accepting = true;
            let task = tokio::spawn(shared.clone().receive(socket, rx));
            *shared.receiver.lock().expect("slack receiver") = Some(ReceiveTask { stop, task });
            let mut startup = StartupOwner {
                shared: shared.clone(),
                committed: false,
            };
            let mut body = JsObject::new();
            body.insert("token".into(), shared.bot_token.clone());
            match shared.api("/auth.test", body).await {
                Ok(auth) => {
                    shared.state.lock().expect("slack state").user_id =
                        auth.get("user_id").and_then(JsValue::as_string).cloned();
                    let user = auth
                        .get("user")
                        .filter(|v| !sms::nullish(v))
                        .or_else(|| auth.get("user_id"))
                        .cloned()
                        .unwrap_or(JsValue::Undefined);
                    shared.log(
                        "info",
                        &format!(
                            "slack: Socket Mode connected as {}",
                            String::from_utf16_lossy(sms::js_string_of(&user).units())
                        ),
                    );
                }
                Err(error) => {
                    shared.log("error", &format!("slack: start failed: {error}"));
                    startup.committed = true;
                    Handle(shared).stop("start failed").await?;
                    return Ok(Arc::new(NoopHandle) as Arc<dyn ChannelHandle>);
                }
            }
            startup.committed = true;
            Ok(Arc::new(Handle(shared)) as Arc<dyn ChannelHandle>)
        })
    }
}

pub(crate) fn register_with_network(
    mounts: &mut crate::channel::loader::ChannelLoaderMounts,
    responder: ResponderRef,
    network: Arc<ChannelNetwork>,
) {
    mounts.register(
        CHANNEL_KEY,
        Arc::new(move |options: &JsObject| {
            Ok(Arc::new(NativeSlackChannel::new(
                NativeSlackChannelOptions {
                    bot_token: options.get("botToken").cloned(),
                    app_token: options.get("appToken").cloned(),
                    network: Some(network.clone()),
                    ..Default::default()
                },
                js_helper_interaction(responder.clone()),
            )) as Arc<dyn ChannelAdapter>)
        }),
    );
}
