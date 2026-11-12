//! Native Discord REST and Gateway adapter for fixed TS `cc7c4e6`.
//! Native HTTP only. The production loader supplies a fresh JS-preserving
//! interaction helper per adapter; inbound turns share the host TurnGateway.
mod mapper;
#[cfg(test)]
mod tests;
mod wire;

pub use mapper::{DiscordSessionMapper, DiscordSessionMapperState, MappedSession};

use crate::channel::{
    ActiveChat,
    im_cron::{CronResultDelivery, deliver_chat_cron_result},
    im_interaction::{JsInteractionPort, ResponderRef, js_helper_interaction},
    protocol::*,
    sms,
};
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

pub const CHANNEL_KEY: &str = "discord";
pub type Env = Arc<dyn Fn(&str) -> Option<JsString> + Send + Sync>;

#[derive(Default)]
pub struct DiscordChannelOptions {
    pub token: Option<JsString>,
    pub api_base: Option<String>,
    pub gateway_url: Option<String>,
    pub(crate) network: Option<Arc<crate::channel::network::ChannelNetwork>>,
    pub mapper: Option<DiscordSessionMapper>,
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
                result: Err(ChannelError::error("discord: cleanup worker terminated before draining; cleanup ownership is unresolved")),
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
    mapper: DiscordSessionMapper,
    user_id: Option<JsString>,
    chats: HashSet<JsString>,
    gateway: Option<Arc<dyn TurnGateway>>,
    logger: Option<ChannelLog>,
}
struct Shared {
    key: ChannelKey,
    token: JsString,
    api_base: String,
    gateway_url: Option<String>,
    network: Arc<crate::channel::network::ChannelNetwork>,
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
pub struct DiscordChannel(Arc<Shared>);

impl DiscordChannel {
    pub fn new(options: DiscordChannelOptions, interaction: Arc<dyn JsInteractionPort>) -> Self {
        let env = options
            .env
            .unwrap_or_else(|| Arc::new(|key| std::env::var(key).ok().map(JsString::from)));
        let token = options
            .token
            .or_else(|| env("DISCORD_BOT_TOKEN"))
            .unwrap_or_default();
        let network = options.network.unwrap_or_default();
        Self(Arc::new(Shared {
            key: ChannelKey::new(CHANNEL_KEY),
            token,
            api_base: options
                .api_base
                .unwrap_or_else(|| "https://discord.com/api/v10".into())
                .trim_end_matches('/')
                .to_owned(),
            gateway_url: options.gateway_url,
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
        let logger = self.state.lock().expect("discord state").logger.clone();
        if let Some(log) = logger {
            log(level, text);
        }
    }
    fn gateway(&self) -> Option<Arc<dyn TurnGateway>> {
        self.state.lock().expect("discord state").gateway.clone()
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
            format!("Bot {}", String::from_utf16_lossy(self.token.units()))
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

    async fn ws_send(
        &self,
        socket: &mut crate::channel::ws_transport::ChannelWebSocket,
        message: Message,
    ) -> bool {
        tokio::select! {
            biased;
            _ = self.stopped() => false,
            result = tokio::time::timeout(Duration::from_secs(30), socket.send(message)) => matches!(result, Ok(Ok(()))),
        }
    }

    async fn receive(
        self: Arc<Self>,
        initial_url: String,
        ready: oneshot::Sender<Result<(), String>>,
        mut stop: watch::Receiver<bool>,
    ) {
        let mut ready = Some(ready);
        let mut session: Option<JsString> = None;
        let mut sequence = JsValue::Null;
        let mut resume_url: Option<String> = None;
        loop {
            if *stop.borrow() {
                return;
            }
            let base = resume_url.as_deref().unwrap_or(&initial_url);
            let url = match wire::gateway_url(base) {
                Ok(url) => url,
                Err(error) => {
                    self.log("error", &format!("discord: client error: {error}"));
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(Err(error));
                    }
                    return;
                }
            };
            let connected = tokio::select! { biased; _ = stop.changed() => return, connected = self.network.connect_ws(url.as_str()) => connected };
            match connected {
                Ok((mut socket, _)) => {
                    let mut heartbeat: Option<tokio::time::Interval> = None;
                    let mut acknowledged = true;
                    loop {
                        let incoming = tokio::select! {
                            biased;
                            _ = stop.changed() => return,
                            _ = async { if let Some(timer) = &mut heartbeat { timer.tick().await; } else { std::future::pending::<()>().await; } } => {
                                if !acknowledged { break; }
                                acknowledged = false;
                                if !self.ws_send(&mut socket, Message::Text(serde_json::json!({"op":1,"d":serde_json::from_str::<serde_json::Value>(&stringify_json(&sequence).unwrap().unwrap()).unwrap()}).to_string().into())).await { break; }
                                continue;
                            },
                            incoming = socket.next() => incoming,
                        };
                        let Some(incoming) = incoming else {
                            break;
                        };
                        let raw = match incoming {
                            Ok(Message::Text(raw)) => raw,
                            Ok(Message::Ping(bytes)) => {
                                if !self.ws_send(&mut socket, Message::Pong(bytes)).await {
                                    break;
                                }
                                continue;
                            }
                            Ok(Message::Close(frame)) => {
                                let code = frame.map(|f| u16::from(f.code)).unwrap_or(1000);
                                if matches!(code, 4004 | 4010 | 4011 | 4012 | 4013 | 4014) {
                                    let error = format!("gateway closed with fatal code {code}");
                                    self.log("error", &format!("discord: client error: {error}"));
                                    if let Some(ready) = ready.take() {
                                        let _ = ready.send(Err(error));
                                    }
                                    return;
                                }
                                if matches!(code, 4007 | 4009) {
                                    session = None;
                                    sequence = JsValue::Null;
                                    resume_url = None;
                                }
                                break;
                            }
                            Err(_) => break,
                            _ => continue,
                        };
                        let Ok(packet) = parse_json(raw.as_str()) else {
                            continue;
                        };
                        if let Some(seq) = packet.get("s").filter(|v| !sms::nullish(v)) {
                            sequence = seq.clone();
                        }
                        let op = packet
                            .get("op")
                            .and_then(JsValue::as_number)
                            .unwrap_or(-1.0) as i64;
                        match op {
                            10 => {
                                let interval = packet
                                    .get("d")
                                    .and_then(|v| v.get("heartbeat_interval"))
                                    .and_then(JsValue::as_number)
                                    .unwrap_or(45000.0)
                                    .max(1.0) as u64;
                                let mut timer = tokio::time::interval_at(
                                    tokio::time::Instant::now() + Duration::from_millis(interval),
                                    Duration::from_millis(interval),
                                );
                                timer.set_missed_tick_behavior(
                                    tokio::time::MissedTickBehavior::Delay,
                                );
                                heartbeat = Some(timer);
                                let token = String::from_utf16_lossy(self.token.units());
                                let payload = if let Some(id) = &session {
                                    let seq = stringify_json(&sequence).unwrap().unwrap();
                                    serde_json::json!({"op":6,"d":{"token":token,"session_id":String::from_utf16_lossy(id.units()),"seq":serde_json::from_str::<serde_json::Value>(&seq).unwrap()}})
                                } else {
                                    serde_json::json!({"op":2,"d":{"token":token,"intents":37377,"properties":{"os":std::env::consts::OS,"browser":"pilotdeck","device":"pilotdeck"}}})
                                };
                                if !self
                                    .ws_send(&mut socket, Message::Text(payload.to_string().into()))
                                    .await
                                {
                                    break;
                                }
                            }
                            11 => acknowledged = true,
                            1 => {
                                let seq = stringify_json(&sequence).unwrap().unwrap();
                                let payload = serde_json::json!({"op":1,"d":serde_json::from_str::<serde_json::Value>(&seq).unwrap()});
                                if !self
                                    .ws_send(&mut socket, Message::Text(payload.to_string().into()))
                                    .await
                                {
                                    break;
                                }
                            }
                            7 => break,
                            9 => {
                                if !packet.get("d").is_some_and(sms::truthy) {
                                    session = None;
                                    sequence = JsValue::Null;
                                    resume_url = None;
                                }
                                break;
                            }
                            0 => {
                                let kind = packet.get("t").and_then(JsValue::as_string);
                                if kind.is_some_and(|s| s.equals_str("READY")) {
                                    if let Some(data) = packet.get("d") {
                                        session = data
                                            .get("session_id")
                                            .and_then(JsValue::as_string)
                                            .cloned();
                                        resume_url = data
                                            .get("resume_gateway_url")
                                            .and_then(JsValue::as_string)
                                            .map(|s| String::from_utf16_lossy(s.units()));
                                        let user = data.get("user");
                                        let id = user
                                            .and_then(|v| v.get("id"))
                                            .and_then(JsValue::as_string)
                                            .cloned();
                                        self.state.lock().expect("discord state").user_id =
                                            id.clone();
                                        if let Some(ready) = ready.take() {
                                            let _ = ready.send(Ok(()));
                                        }
                                        self.log(
                                            "info",
                                            &format!(
                                                "discord: logged in as {}",
                                                id.map(|s| String::from_utf16_lossy(s.units()))
                                                    .unwrap_or_default()
                                            ),
                                        );
                                    }
                                } else if kind.is_some_and(|s| s.equals_str("MESSAGE_CREATE"))
                                    && let Some(data) = packet.get("d")
                                {
                                    let Some(chat) = data
                                        .get("channel_id")
                                        .and_then(JsValue::as_string)
                                        .cloned()
                                    else {
                                        continue;
                                    };
                                    let shared = self.clone();
                                    let data = data.clone();
                                    let mut future = Box::pin(async move {
                                        shared.parse_message(chat, data).await;
                                    });
                                    if future.as_mut().now_or_never().is_none() {
                                        let mut handlers =
                                            self.handlers.lock().expect("discord handlers");
                                        self.harvest_finished(&mut handlers, "handler");
                                        handlers.push(tokio::spawn(future));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Err(error) => self.log("error", &format!("discord: client error: {error}")),
            }
            tokio::select! { biased; _ = stop.changed() => return, _ = tokio::time::sleep(Duration::from_secs(1)) => {} }
        }
    }

    async fn parse_message(self: Arc<Self>, chat: JsString, raw: JsValue) {
        if *self.stopping.borrow() {
            return;
        }
        let own_id = self.state.lock().expect("discord state").user_id.clone();
        let Some(text) = wire::message(&raw, own_id.as_ref()) else {
            return;
        };
        self.state
            .lock()
            .expect("discord state")
            .chats
            .insert(chat.clone());
        if self.interaction.elicitation_pending(&chat) && self.gateway().is_some() {
            let answer = tokio::select! { biased; _ = self.stopped() => return, answer = self.interaction.elicitation_answer(&chat, &text) => answer };
            match answer {
                Ok(Some(text)) if !text.is_empty() => {
                    self.send_reply(&chat, &text).await;
                }
                Ok(_) => {}
                Err(e) => self.log(
                    "error",
                    &format!("discord: elicitation answer error: {}", e.to_js_string()),
                ),
            }
            return;
        }
        if self.interaction.permission_pending(&chat) && self.gateway().is_some() {
            let answer = tokio::select! { biased; _ = self.stopped() => return, answer = self.interaction.permission_answer(&chat, &text) => answer };
            match answer {
                Err(e) => self.log(
                    "error",
                    &format!("discord: permission answer error: {}", e.to_js_string()),
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
            let mut state = self.state.lock().expect("discord state");
            let mut active = self.active.lock().expect("discord active chats");
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
                    "discord: chat {} already active, skipping",
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
        // Typing is best effort but still owned by this adapter's task.
        let path = format!("/channels/{}/typing", wire::encode(chat));
        let typing = self.clone();
        {
            let mut outbound = self.outbound.lock().expect("discord outbound");
            if outbound.accepting && !*self.stopping.borrow() {
                self.harvest_finished(&mut outbound.tasks, "typing");
                outbound.tasks.push(tokio::spawn(async move {
                    let url = format!("{}{}", typing.api_base, path);
                    tokio::select! { biased; _ = typing.stopped() => {}, _ = typing.request(Method::POST, &url, None) => {} }
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
                        if let Err(error) = stream.cancel_and_drain(JsString::from("discord channel stopped")).await {
                            self.cleanup_errors.lock().expect("discord cleanup errors").push(error.clone());
                            self.failed_streams.lock().expect("discord failed streams").push(stream);
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
                &format!("discord: submitTurn error: {}", error.to_js_string()),
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
                        .expect("discord cleanup errors")
                        .push(ChannelError::error(format!(
                            "discord: {role} cleanup failed: {error}"
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
            let mut outbound = self.outbound.lock().expect("discord outbound");
            if !outbound.accepting || *self.stopping.borrow() {
                return false;
            }
            self.harvest_finished(&mut outbound.tasks, "outbound");
            let recipient = chat.clone();
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
        let channel = format!("/channels/{}", wire::encode(recipient));
        let fetched = tokio::select! { biased; _ = self.stopped() => return false, fetched = self.json_request(Method::GET, &channel, None) => fetched };
        match fetched {
            Err(error) => {
                self.log("error", &format!("discord: fetch channel failed: {error}"));
                return false;
            }
            Ok(value) => {
                let kind = value
                    .get("type")
                    .and_then(JsValue::as_number)
                    .unwrap_or(-1.0) as i64;
                if !matches!(kind, 0 | 1 | 2 | 3 | 5 | 10 | 11 | 12 | 13) {
                    self.log(
                        "warn",
                        &format!(
                            "discord: channel {} not sendable",
                            String::from_utf16_lossy(recipient.units())
                        ),
                    );
                    return false;
                }
            }
        }
        let mut ok = true;
        for chunk in wire::chunk_text(text) {
            let mut body = JsObject::new();
            body.insert("content".into(), JsValue::String(chunk));
            let body = stringify_json(&JsValue::Object(body))
                .expect("finite Discord body")
                .expect("object JSON");
            let path = format!("{channel}/messages");
            let url = format!("{}{}", self.api_base, path);
            let response = tokio::select! { biased; _ = self.stopped() => return false, response = self.request(Method::POST, &url, Some(body)) => response };
            match response {
                Ok(response) if response.status().is_success() => {}
                Ok(response) => {
                    let status = response.status().as_u16();
                    let text = tokio::select! { biased; _ = self.stopped() => return false, text = Self::response_text(response.into_body()) => text.unwrap_or_default() };
                    let units: Vec<_> = text.encode_utf16().take(500).collect();
                    self.log(
                        "error",
                        &format!(
                            "discord: send HTTP {status}: {}",
                            String::from_utf16_lossy(&units)
                        ),
                    );
                    ok = false;
                }
                Err(error) => {
                    self.log("error", &format!("discord: send failed: {error}"));
                    ok = false;
                }
            }
        }
        ok
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
            std::mem::take(&mut *self.failed_streams.lock().expect("discord failed streams"))
        } else {
            Vec::new()
        };
        let receiver = self.receiver.lock().expect("discord receiver").take();
        if let Some(receiver) = receiver
            && let Err(error) = receiver.task.await
        {
            self.fatal_cleanup.store(true, Ordering::Release);
            self.cleanup_errors
                .lock()
                .expect("discord cleanup errors")
                .push(ChannelError::error(format!(
                    "discord: receiver cleanup failed: {error}"
                )));
        }

        let handlers = std::mem::take(&mut *self.handlers.lock().expect("discord handlers"));
        for handler in handlers {
            if let Err(error) = handler.await {
                self.fatal_cleanup.store(true, Ordering::Release);
                self.log(
                    "error",
                    &format!("discord: handler cleanup failed: {error}"),
                );
                self.cleanup_errors
                    .lock()
                    .expect("discord cleanup errors")
                    .push(ChannelError::error(format!(
                        "discord: handler cleanup failed: {error}"
                    )));
            }
        }
        let outbound = std::mem::take(&mut self.outbound.lock().expect("discord outbound").tasks);
        for task in outbound {
            if let Err(error) = task.await {
                self.fatal_cleanup.store(true, Ordering::Release);
                self.cleanup_errors
                    .lock()
                    .expect("discord cleanup errors")
                    .push(ChannelError::error(format!(
                        "discord: outbound cleanup failed: {error}"
                    )));
            }
        }
        for mut stream in retry_streams {
            if let Err(error) = stream
                .cancel_and_drain(JsString::from("discord channel stopped"))
                .await
            {
                self.cleanup_errors
                    .lock()
                    .expect("discord cleanup errors")
                    .push(error);
                self.failed_streams
                    .lock()
                    .expect("discord failed streams")
                    .push(stream);
            }
        }
        let errors =
            std::mem::take(&mut *self.cleanup_errors.lock().expect("discord cleanup errors"));
        if !errors.is_empty() {
            return Err(ChannelError::Aggregate {
                message: "discord: owned turn cleanup failed".into(),
                errors,
            });
        }
        let chats: Vec<_> = self
            .state
            .lock()
            .expect("discord state")
            .chats
            .iter()
            .cloned()
            .collect();
        for chat in chats {
            self.interaction.clear_chat_for_shutdown(&chat);
        }
        self.state.lock().expect("discord state").user_id = None;
        Ok(())
    }

    fn cleanup_receipt(self: &Arc<Self>) -> watch::Receiver<Option<CleanupOutcome>> {
        let mut attempt = self.cleanup.lock().expect("discord cleanup attempt");
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
                    .expect("discord failed streams")
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
        self.0.outbound.lock().expect("discord outbound").accepting = false;
        self.0.running.store(false, Ordering::Release);
        self.0.stopping.send_replace(true);
        if let Some(receiver) = self.0.receiver.lock().expect("discord receiver").as_ref() {
            let _ = receiver.stop.send(true);
        }
        self.0.log("info", &format!("discord: stopping ({reason})"));
        let mut receipt = self.0.cleanup_receipt();
        Box::pin(async move {
            loop {
                if let Some(outcome) = receipt.borrow().clone() {
                    return outcome.result;
                }
                receipt.changed().await.map_err(|_| {
                    ChannelError::error("discord: cleanup worker exited without a result")
                })?;
            }
        })
    }
}

impl ChannelAdapter for DiscordChannel {
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
                let mut cleanup = shared.cleanup.lock().expect("discord cleanup attempt");
                if let Some(attempt) = cleanup.as_ref() {
                    if !attempt
                        .result
                        .borrow()
                        .as_ref()
                        .is_some_and(|outcome| outcome.result.is_ok())
                    {
                        return Err(ChannelError::error("discord: prior stop has not drained"));
                    }
                    *cleanup = None;
                }
            }
            {
                let mut state = shared.state.lock().expect("discord state");
                state.gateway = deps.turn_gateway;
                state.logger = deps.log;
            }
            if shared.token.is_empty() {
                shared.log("error", "discord: DISCORD_BOT_TOKEN not set");
                return Ok(Arc::new(NoopHandle) as Arc<dyn ChannelHandle>);
            }
            let url = match &shared.gateway_url {
                Some(url) => url.clone(),
                None => match shared.json_request(Method::GET, "/gateway/bot", None).await {
                    Ok(value) => match value.get("url").and_then(JsValue::as_string) {
                        Some(url) => String::from_utf16_lossy(url.units()),
                        None => {
                            shared.log("error", "discord: start failed: gateway URL missing");
                            return Ok(Arc::new(NoopHandle) as Arc<dyn ChannelHandle>);
                        }
                    },
                    Err(error) => {
                        shared.log("error", &format!("discord: start failed: {error}"));
                        return Ok(Arc::new(NoopHandle) as Arc<dyn ChannelHandle>);
                    }
                },
            };
            let (stop, rx) = watch::channel(false);
            shared.stopping.send_replace(false);
            shared.running.store(true, Ordering::Release);
            shared.outbound.lock().expect("discord outbound").accepting = true;
            let (ready, receipt) = oneshot::channel();
            let task = tokio::spawn(shared.clone().receive(url, ready, rx));
            *shared.receiver.lock().expect("discord receiver") = Some(ReceiveTask { stop, task });
            let mut startup = StartupOwner {
                shared: shared.clone(),
                committed: false,
            };
            let started = tokio::time::timeout(Duration::from_secs(30), receipt).await;
            match started {
                Ok(Ok(Ok(()))) => {
                    startup.committed = true;
                    Ok(Arc::new(Handle(shared)) as Arc<dyn ChannelHandle>)
                }
                outcome => {
                    let error = match outcome {
                        Ok(Ok(Err(error))) => error,
                        Ok(Err(_)) => "gateway stopped before READY".into(),
                        Err(_) => "gateway READY timeout".into(),
                        _ => unreachable!(),
                    };
                    shared.log("error", &format!("discord: start failed: {error}"));
                    startup.committed = true;
                    Handle(shared).stop("start failed").await?;
                    Ok(Arc::new(NoopHandle) as Arc<dyn ChannelHandle>)
                }
            }
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
    register_transport(mounts, responder, Arc::default());
}

pub(crate) fn register_with_network(
    mounts: &mut crate::channel::loader::ChannelLoaderMounts,
    responder: ResponderRef,
    network: Arc<crate::channel::network::ChannelNetwork>,
) {
    register_transport(mounts, responder, network);
}

fn register_transport(
    mounts: &mut crate::channel::loader::ChannelLoaderMounts,
    responder: ResponderRef,
    network: Arc<crate::channel::network::ChannelNetwork>,
) {
    mounts.register(
        CHANNEL_KEY,
        Arc::new(move |options: &JsObject| {
            Ok(Arc::new(DiscordChannel::new(
                DiscordChannelOptions {
                    token: options.get("token").and_then(JsValue::as_string).cloned(),
                    network: Some(network.clone()),
                    ..Default::default()
                },
                js_helper_interaction(responder.clone()),
            )) as Arc<dyn ChannelAdapter>)
        }),
    );
}
