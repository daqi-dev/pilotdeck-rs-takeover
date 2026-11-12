//! Native QQ Bot API adapter; baseline QQChannel.ts at cc7c4e6.
//! Native HTTP and WebSocket transport. The production loader supplies a fresh JS-preserving
//! interaction helper per adapter; inbound turns share the host TurnGateway.
mod mapper;
#[cfg(test)]
mod tests;
mod wire;

pub use mapper::{MappedSession, QQSessionMapper, QQSessionMapperState};

use crate::channel::{
    ActiveChat,
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
use futures_util::{FutureExt, SinkExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::{HeaderMap, Method, Request};
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
use tokio_tungstenite::tungstenite::Message;

pub const CHANNEL_KEY: &str = "qq";
pub type Env = Arc<dyn Fn(&str) -> Option<JsString> + Send + Sync>;

#[derive(Default)]
pub struct QQChannelOptions {
    pub rest_url: Option<JsString>,
    pub app_id: Option<JsString>,
    pub client_secret: Option<JsString>,
    pub allow_groups: Option<Vec<JsString>>,
    pub trigger_prefixes: Option<Vec<JsString>>,
    pub max_message_length: Option<usize>,
    pub token_url: Option<String>,
    network: Option<Arc<crate::channel::network::ChannelNetwork>>,
    pub mapper: Option<QQSessionMapper>,
    pub on_state_change: Option<Arc<dyn Fn(QQSessionMapperState) + Send + Sync>>,
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
                result: Err(ChannelError::error("qq: cleanup worker terminated before draining; cleanup ownership is unresolved")),
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
    mapper: QQSessionMapper,
    recipients: HashMap<JsString, JsString>,
    message_ids: HashMap<JsString, JsValue>,
    gateway: Option<Arc<dyn TurnGateway>>,
    logger: Option<ChannelLog>,
}
struct Shared {
    key: ChannelKey,
    rest_url: String,
    app_id: JsString,
    client_secret: JsString,
    allow_groups: Vec<JsString>,
    trigger_prefixes: Vec<JsString>,
    max_message_length: usize,
    on_state_change: Option<Arc<dyn Fn(QQSessionMapperState) + Send + Sync>>,
    token_url: String,
    token: Mutex<Option<(JsString, tokio::time::Instant)>>,
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
pub struct QQChannel(Arc<Shared>);

impl QQChannel {
    pub fn new(options: QQChannelOptions, interaction: Arc<dyn JsInteractionPort>) -> Self {
        let env = options
            .env
            .unwrap_or_else(|| Arc::new(|key| std::env::var(key).ok().map(JsString::from)));
        let rest_url = options
            .rest_url
            .unwrap_or_else(|| JsString::from("https://api.sgroup.qq.com"));
        let app_id = options
            .app_id
            .or_else(|| env("QQ_BOT_APPID"))
            .unwrap_or_default();
        let client_secret = options
            .client_secret
            .or_else(|| env("QQ_BOT_SECRET"))
            .unwrap_or_default();
        let allow_groups = options
            .allow_groups
            .or_else(|| {
                env("QQ_ALLOW_GROUPS").filter(|v| !v.is_empty()).map(|v| {
                    String::from_utf16_lossy(v.units())
                        .split(',')
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .map(JsString::from)
                        .collect()
                })
            })
            .unwrap_or_else(|| vec![JsString::from("*")]);
        Self(Arc::new(Shared {
            key: ChannelKey::new(CHANNEL_KEY),
            rest_url: String::from_utf16_lossy(rest_url.units())
                .trim_end_matches('/')
                .to_owned(),
            app_id,
            client_secret,
            allow_groups,
            trigger_prefixes: options
                .trigger_prefixes
                .unwrap_or_else(|| vec![JsString::from("/ask"), JsString::from("/chat")]),
            max_message_length: options.max_message_length.unwrap_or(2000).max(1),
            on_state_change: options.on_state_change,
            token_url: options
                .token_url
                .unwrap_or_else(|| "https://bots.qq.com/app/getAppAccessToken".into()),
            token: Mutex::new(None),
            network: options
                .network
                .unwrap_or_else(|| Arc::new(crate::channel::network::ChannelNetwork::default())),
            transport: Arc::new(HeaderTransport(
                options
                    .transport
                    .unwrap_or_else(|| Arc::new(HyperModelTransport::new())),
            )),
            interaction,
            state: Mutex::new(State {
                mapper: options.mapper.unwrap_or_default(),
                recipients: HashMap::new(),
                message_ids: HashMap::new(),
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
        let logger = self.state.lock().expect("qq state").logger.clone();
        if let Some(log) = logger {
            log(level, text);
        }
    }
    fn gateway(&self) -> Option<Arc<dyn TurnGateway>> {
        self.state.lock().expect("qq state").gateway.clone()
    }
    async fn stopped(&self) {
        let mut stop = self.stopping.subscribe();
        if *stop.borrow() {
            return;
        }
        let _ = stop.changed().await;
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
        url: &str,
        body: Option<JsValue>,
        authorized: bool,
    ) -> Result<JsValue, String> {
        let url = url::Url::parse(url).map_err(|e| e.to_string())?;
        let mut headers = HeaderMap::new();
        let api_path = url.path().to_owned();
        let sending = api_path.ends_with("/messages");
        let operation = if api_path.contains("/groups/") {
            "sendGroupMessage"
        } else if api_path.contains("/users/") {
            "sendC2CMessage"
        } else if api_path.ends_with("/gateway") {
            "getGateway"
        } else {
            "getAppAccessToken"
        };
        if body.is_some() {
            headers.insert(
                "content-type",
                "application/json".parse().expect("content-type"),
            );
        }
        if authorized {
            let token = self.ensure_token().await?;
            headers.insert(
                "authorization",
                format!("QQBot {}", String::from_utf16_lossy(token.units()))
                    .parse()
                    .map_err(|_| "invalid token header")?,
            );
        }
        let body = body
            .map(|v| stringify_json(&v).expect("finite JSON").unwrap_or_default())
            .unwrap_or_default();
        let response = fetch(
            self.transport.as_ref(),
            url,
            method,
            headers,
            Bytes::from(body),
        )
        .await
        .map_err(|e| e.message)?;
        let status = response.status();
        if status.is_success() && sending {
            return Ok(JsValue::Undefined);
        }
        if !status.is_success() && operation == "getGateway" {
            return Err(format!("getGateway failed: {}", status.as_u16()));
        }
        let text = Self::response_text(response.into_body()).await?;
        if !status.is_success() {
            return Err(format!("{operation} failed: {} {text}", status.as_u16()));
        }
        parse_json(&text).map_err(|e| format!("{e:?}"))
    }

    fn ensure_token(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<JsString, String>> + Send + '_>>
    {
        Box::pin(async move {
            if let Some((token, expires)) = self.token.lock().expect("qq token").as_ref()
                && *expires > tokio::time::Instant::now() + Duration::from_secs(120)
            {
                return Ok(token.clone());
            }
            let body = object(&[
                ("appId", JsValue::String(self.app_id.clone())),
                ("clientSecret", JsValue::String(self.client_secret.clone())),
            ]);
            let result = self
                .json_request(Method::POST, &self.token_url, Some(body), false)
                .await?;
            let token = result
                .get("access_token")
                .and_then(JsValue::as_string)
                .cloned()
                .ok_or("getAppAccessToken missing access_token")?;
            let seconds = result
                .get("expires_in")
                .map(sms::js_string_of)
                .and_then(|v| String::from_utf16_lossy(v.units()).parse::<f64>().ok())
                .unwrap_or(0.0)
                .max(0.0);
            *self.token.lock().expect("qq token") = Some((
                token.clone(),
                tokio::time::Instant::now() + Duration::from_secs_f64(seconds),
            ));
            Ok(token)
        })
    }

    async fn socket_once(
        self: &Arc<Self>,
        stop: &mut watch::Receiver<bool>,
        session: &mut Option<JsString>,
        seq: &mut JsValue,
        connected: &mut bool,
        close_code: &mut u16,
        initial_url: Option<String>,
    ) -> Result<(), String> {
        let url = if let Some(url) = initial_url {
            url
        } else {
            let gateway_url = format!("{}/gateway", self.rest_url);
            let discovery = tokio::select! { biased; _ = stop.changed() => return Ok(()), result = self.json_request(Method::GET, &gateway_url, None, true) => result }?;
            discovery
                .get("url")
                .and_then(JsValue::as_string)
                .map(|url| String::from_utf16_lossy(url.units()))
                .ok_or("getGateway missing url")?
        };
        let (mut socket, _) = tokio::select! { biased; _ = stop.changed() => return Ok(()), result = self.network.connect_ws(url.as_str()) => result }.map_err(|e| e.to_string())?;
        *connected = true;
        self.log("info", "qq: WebSocket connected to QQ Gateway");
        let mut heartbeat: Option<tokio::time::Interval> = None;
        let mut heartbeat_ms = 41250;
        let mut identify_after: Option<tokio::time::Instant> = None;
        loop {
            tokio::select! {
                biased;
                _ = stop.changed() => { let _ = socket.close(None).await; return Ok(()); },
                _ = async { match heartbeat.as_mut() { Some(interval) => { interval.tick().await; }, None => std::future::pending::<()>().await } } => {
                    let payload = object(&[("op", JsValue::Number(1.0)), ("d", seq.clone())]);
                    socket.send(Message::Text(stringify_json(&payload).unwrap().unwrap().into())).await.map_err(|e| e.to_string())?;
                },
                _ = async { match identify_after { Some(at) => tokio::time::sleep_until(at).await, None => std::future::pending::<()>().await } } => {
                    identify_after = None;
                    let token = self.ensure_token().await?;
                    let payload = wire::identify(&token);
                    socket.send(Message::Text(stringify_json(&payload).unwrap().unwrap().into())).await.map_err(|e| e.to_string())?;
                },
                incoming = socket.next() => {
                    let Some(incoming) = incoming else { return Ok(()); };
                    let incoming = incoming.map_err(|e| e.to_string())?;
                    if let Message::Close(frame) = &incoming { *close_code = frame.as_ref().map(|f| u16::from(f.code)).unwrap_or(1005); return Ok(()); }
                    if let Message::Ping(bytes) = incoming { socket.send(Message::Pong(bytes)).await.map_err(|e| e.to_string())?; continue; }
                    let text = match incoming { Message::Text(text) => text.to_string(), Message::Binary(bytes) => String::from_utf8_lossy(&bytes).into_owned(), _ => continue };
                    let Ok(raw) = parse_json(&text) else { continue; };
                    if let Some(value) = raw.get("s").filter(|v| !sms::nullish(v)) { *seq = value.clone(); }
                    let op = raw.get("op").and_then(|v| if let JsValue::Number(n)=v {Some(*n as i32)} else {None});
                    match op {
                        Some(10) => {
                            heartbeat_ms = raw.get("d").and_then(|d| d.get("heartbeat_interval")).and_then(|v| if let JsValue::Number(n)=v {Some((*n as u64).max(1))} else {None}).unwrap_or(41250);
                            let token = self.ensure_token().await?;
                            let payload = if let Some(id) = session.as_ref() { object(&[("op", JsValue::Number(6.0)), ("d", object(&[("token", JsValue::String(JsString::from(format!("QQBot {}", String::from_utf16_lossy(token.units()))))), ("session_id", JsValue::String(id.clone())), ("seq", seq.clone())]))]) } else { wire::identify(&token) };
                            socket.send(Message::Text(stringify_json(&payload).unwrap().unwrap().into())).await.map_err(|e| e.to_string())?;
                        },
                        Some(0) => {
                            let kind = raw.get("t").and_then(JsValue::as_string);
                            let data = raw.get("d").cloned().unwrap_or(JsValue::Undefined);
                            let data_text = stringify_json(&data).ok().flatten().unwrap_or_else(|| "undefined".into());
                            let data_text = String::from_utf16_lossy(&data_text.encode_utf16().take(200).collect::<Vec<_>>());
                            self.log("info", &format!("qq: [dispatch] type={} data={data_text}", String::from_utf16_lossy(kind.unwrap_or(&JsString::from("undefined")).units())));
                            if kind.is_some_and(|v| v.equals_str("READY") || v.equals_str("RESUMED")) {
                                if kind.is_some_and(|v| v.equals_str("READY")) { *session = data.get("session_id").and_then(JsValue::as_string).cloned(); }
                                heartbeat = Some(tokio::time::interval_at(tokio::time::Instant::now()+Duration::from_millis(heartbeat_ms),Duration::from_millis(heartbeat_ms)));
                                self.log("info", if kind.is_some_and(|v| v.equals_str("READY")) { "qq: authenticated, listening for group messages" } else { "qq: session resumed" });
                            } else if kind.is_some_and(|v| v.equals_str("GROUP_AT_MESSAGE_CREATE") || v.equals_str("C2C_MESSAGE_CREATE")) {
                                let group = kind.is_some_and(|v| v.equals_str("GROUP_AT_MESSAGE_CREATE"));
                                let shared = self.clone();
                                let mut future = Box::pin(async move { shared.parse_message(&data, group).await; });
                                if future.as_mut().now_or_never().is_none() {
                                    let mut handlers = self.handlers.lock().expect("qq handlers");
                                    self.harvest_finished(&mut handlers, "handler");
                                    handlers.push(tokio::spawn(future));
                                }
                            }
                        },
                        Some(7) => { *close_code = 1005; let _ = socket.close(None).await; return Ok(()); },
                        Some(9) => { *session = None; *seq = JsValue::Null; identify_after = Some(tokio::time::Instant::now()+Duration::from_secs(2)); },
                        _ => {}
                    }
                }
            }
        }
    }

    async fn receive(self: Arc<Self>, initial_url: String, mut stop: watch::Receiver<bool>) {
        let mut initial_url = Some(initial_url);
        let mut session = None;
        let mut seq = JsValue::Null;
        loop {
            if *stop.borrow() {
                return;
            }
            let mut socket_stop = stop.clone();
            let mut connected = false;
            let mut close_code = 1006;
            let result = self
                .socket_once(
                    &mut socket_stop,
                    &mut session,
                    &mut seq,
                    &mut connected,
                    &mut close_code,
                    initial_url.take(),
                )
                .await;
            if *stop.borrow() {
                return;
            }
            if let Err(error) = result {
                self.log("error", &format!("qq: error: {error}"));
            }
            if !connected {
                return;
            }
            self.log(
                "warn",
                &format!("qq: disconnected (code={close_code}), will reconnect..."),
            );
            tokio::select! { biased; _ = stop.changed() => return, _ = tokio::time::sleep(Duration::from_secs(5)) => {} }
            self.log("info", "qq: reconnecting...");
        }
    }

    async fn parse_message(self: Arc<Self>, message: &JsValue, group: bool) {
        if *self.stopping.borrow() {
            return;
        }
        let field =
            |name: &str| sms::js_string_of(message.get(name).unwrap_or(&JsValue::Undefined));
        let author_value = message.get("author");
        let user_value = author_value
            .and_then(|v| {
                v.get(if group {
                    "member_openid"
                } else {
                    "user_openid"
                })
            })
            .map(sms::js_string_of)
            .unwrap_or_else(|| "undefined".into());
        if group {
            self.log(
                "info",
                &format!(
                    "qq: [group_msg] group={} user={} content=\"{}\"",
                    String::from_utf16_lossy(field("group_openid").units()),
                    String::from_utf16_lossy(user_value.units()),
                    String::from_utf16_lossy(field("content").units())
                ),
            );
        } else {
            self.log(
                "info",
                &format!(
                    "qq: [c2c_msg] user={} content=\"{}\"",
                    String::from_utf16_lossy(user_value.units()),
                    String::from_utf16_lossy(field("content").units())
                ),
            );
        }
        let Some(author) = message.get("author") else {
            return;
        };
        let Some(user) = author
            .get(if group {
                "member_openid"
            } else {
                "user_openid"
            })
            .and_then(JsValue::as_string)
            .filter(|s| !s.is_empty())
        else {
            return;
        };
        let group_id = message.get("group_openid").and_then(JsValue::as_string);
        if group
            && group_id.is_none_or(|g| {
                g.is_empty()
                    || !self
                        .allow_groups
                        .iter()
                        .any(|v| v.equals_str("*") || v == g)
            })
        {
            return;
        }
        let Some(raw_text) = message.get("content").and_then(JsValue::as_string) else {
            return;
        };
        let mut text = JsString::from_utf16(sms::trim_units(raw_text.units()).to_vec());
        if group {
            for prefix in &self.trigger_prefixes {
                if text.units().starts_with(prefix.units()) {
                    text = JsString::from_utf16(
                        sms::trim_units(&text.units()[prefix.units().len()..]).to_vec(),
                    );
                    break;
                }
            }
        }
        if text.is_empty() {
            return;
        }
        let chat = if let Some(group_id) = group_id.filter(|_| group) {
            sms::concat_units(&[group_id.units(), &[58], user.units()])
        } else {
            sms::concat_units(&[&"c2c:".encode_utf16().collect::<Vec<_>>(), user.units()])
        };
        let recipient = if let Some(group_id) = group_id.filter(|_| group) {
            format!("groups/{}", String::from_utf16_lossy(group_id.units()))
        } else {
            format!("users/{}", String::from_utf16_lossy(user.units()))
        };
        {
            let mut state = self.state.lock().expect("qq state");
            state
                .recipients
                .insert(chat.clone(), JsString::from(recipient));
        }
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
                    &format!(
                        "qq: elicitation answer error{}: {}",
                        if group { "" } else { " (c2c)" },
                        e.to_js_string()
                    ),
                ),
            }
            return;
        }
        if self.interaction.permission_pending(&chat) && self.gateway().is_some() {
            let answer = tokio::select! { biased; _ = self.stopped() => return, answer = self.interaction.permission_answer(&chat, &envelope.text) => answer };
            match answer {
                Err(e) => self.log(
                    "error",
                    &format!(
                        "qq: permission answer error{}: {}",
                        if group { "" } else { " (c2c)" },
                        e.to_js_string()
                    ),
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
        let mut changed_state = None;
        let admission = {
            let mut state = self.state.lock().expect("qq state");
            let mut active = self.active.lock().expect("qq active chats");
            if active.contains(&chat) {
                Admission::Busy
            } else {
                state.message_ids.insert(
                    chat.clone(),
                    message.get("id").cloned().unwrap_or(JsValue::Undefined),
                );
                let mapped = if group {
                    state
                        .mapper
                        .resolve(group_id.expect("validated group"), user, &envelope.text)
                } else {
                    MappedSession {
                        session_key: JsString::from(format!(
                            "qq:c2c:{}:general",
                            String::from_utf16_lossy(user.units())
                        )),
                        command_new: false,
                        message: envelope.text.clone(),
                    }
                };
                if mapped.command_new {
                    changed_state = Some(state.mapper.snapshot());
                }
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
        if let Some(state) = changed_state
            && let Some(callback) = &self.on_state_change
        {
            callback(state);
        }
        match admission {
            Admission::Busy => self.log(
                "info",
                &format!(
                    "qq: {} {} already active, skipping",
                    if group { "chat" } else { "c2c" },
                    String::from_utf16_lossy(chat.units())
                ),
            ),
            Admission::New => {
                self.send_reply_chunked(&chat, &JsString::from("已创建新会话。"))
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
                        if let Err(error) = stream.cancel_and_drain(JsString::from("qq channel stopped")).await {
                            self.cleanup_errors.lock().expect("qq cleanup errors").push(error.clone());
                            self.failed_streams.lock().expect("qq failed streams").push(stream);
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
                    self.send_reply_chunked(chat, &prompt).await;
                } else if kind.is_some_and(|s| s.equals_str("permission_request")) {
                    if let Some(prompt) = self.interaction.permission_capture(chat, &mapped.session_key, &event).filter(|p| !p.is_empty()) {
                        let delivered = self.send_reply_chunked(chat, &prompt).await;
                        self.interaction.permission_confirm_initial(chat, delivered, event.get("requestId").and_then(JsValue::as_string));
                    }
                } else if let Some(fragment) = wire::render_event(&event) { reply.extend_from_slice(fragment.units()); }
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
                &format!(
                    "qq: submitTurn error{}: {}",
                    if chat
                        .units()
                        .starts_with(&"c2c:".encode_utf16().collect::<Vec<_>>())
                    {
                        " (c2c)"
                    } else {
                        ""
                    },
                    error.to_js_string()
                ),
            );
            reply = "处理消息时发生错误，请重试。".encode_utf16().collect();
        }
        self.interaction.elicitation_clear(chat);
        self.interaction.permission_clear_after_turn(chat);
        let reply = sms::trim_units(&reply);
        if !reply.is_empty() {
            self.send_reply_chunked(chat, &JsString::from_utf16(reply.to_vec()))
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
                    self.cleanup_errors.lock().expect("qq cleanup errors").push(
                        ChannelError::error(format!("qq: {role} cleanup failed: {error}")),
                    );
                }
                None => {
                    tasks.push(task);
                    break;
                }
            }
        }
    }

    async fn send_reply(self: &Arc<Self>, chat: &JsString, text: &JsString) -> bool {
        self.send_internal(chat, text, false).await
    }
    async fn send_reply_chunked(self: &Arc<Self>, chat: &JsString, text: &JsString) -> bool {
        self.send_internal(chat, text, true).await
    }
    async fn send_internal(
        self: &Arc<Self>,
        chat: &JsString,
        text: &JsString,
        chunked: bool,
    ) -> bool {
        let receipt = {
            // Admission and registration share stop's synchronous boundary.
            // An external cron caller owns only the receipt, never the HTTP
            // exchange or the sole capability for waiting for its release.
            let mut outbound = self.outbound.lock().expect("qq outbound");
            if !outbound.accepting || *self.stopping.borrow() {
                return false;
            }
            self.harvest_finished(&mut outbound.tasks, "outbound");
            let recipient = self
                .state
                .lock()
                .expect("qq state")
                .recipients
                .get(chat)
                .cloned()
                .unwrap_or_else(|| chat.clone());
            if recipient.is_empty() {
                drop(outbound);
                self.log(
                    "warn",
                    &format!(
                        "qq: no recipient for {}, cannot send",
                        String::from_utf16_lossy(chat.units())
                    ),
                );
                return false;
            }
            let message_id = if chunked {
                self.state
                    .lock()
                    .expect("qq state")
                    .message_ids
                    .get(chat)
                    .cloned()
            } else {
                None
            };
            let (finished, receipt) = oneshot::channel();
            let shared = self.clone();
            let text = text.clone();
            outbound.tasks.push(tokio::spawn(async move {
                let delivered = shared
                    .send_reply_owned(&recipient, &text, message_id, chunked)
                    .await;
                // Dropping the external receipt does not cancel this owner.
                let _ = finished.send(delivered);
            }));
            receipt
        };
        // A panicked worker closes the receipt and its JoinError remains in
        // the registry, making subsequent stop explicitly fail.
        receipt.await.unwrap_or(false)
    }

    async fn send_reply_owned(
        &self,
        recipient: &JsString,
        text: &JsString,
        message_id: Option<JsValue>,
        chunked: bool,
    ) -> bool {
        if !self.running.load(Ordering::Acquire) || *self.stopping.borrow() {
            return false;
        }
        let chunks = if chunked {
            wire::chunk_text(text, self.max_message_length)
        } else {
            vec![text.clone()]
        };
        let multiple = chunks.len() > 1;
        for (index, chunk) in chunks.into_iter().enumerate() {
            let mut body = object(&[
                ("content", JsValue::String(chunk)),
                ("msg_type", JsValue::Number(0.0)),
            ]);
            if let JsValue::Object(fields) = &mut body
                && let Some(id) = message_id.as_ref().filter(|id| sms::truthy(id))
            {
                fields.insert("msg_id".into(), id.clone());
                fields.insert("msg_seq".into(), JsValue::Number((index + 1) as f64));
            }
            let url = format!(
                "{}/v2/{}/messages",
                self.rest_url,
                String::from_utf16_lossy(recipient.units())
            );
            let result = tokio::select! { biased; _ = self.stopped() => return false, result = self.json_request(Method::POST, &url, Some(body), true) => result };
            if let Err(error) = result {
                let operation = if recipient
                    .units()
                    .starts_with(&"users/".encode_utf16().collect::<Vec<_>>())
                {
                    "sendC2CMessage"
                } else {
                    "sendGroupMessage"
                };
                self.log("error", &format!("qq: {operation} failed: Error: {error}"));
                return false;
            }
            if multiple {
                tokio::select! { biased; _ = self.stopped() => return false, _ = tokio::time::sleep(Duration::from_millis(500)) => {} }
            }
        }
        true
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
            std::mem::take(&mut *self.failed_streams.lock().expect("qq failed streams"))
        } else {
            Vec::new()
        };
        let receiver = self.receiver.lock().expect("qq receiver").take();
        if let Some(receiver) = receiver
            && let Err(error) = receiver.task.await
        {
            self.fatal_cleanup.store(true, Ordering::Release);
            self.cleanup_errors
                .lock()
                .expect("qq cleanup errors")
                .push(ChannelError::error(format!(
                    "qq: receiver cleanup failed: {error}"
                )));
        }
        let handlers = std::mem::take(&mut *self.handlers.lock().expect("qq handlers"));
        for handler in handlers {
            if let Err(error) = handler.await {
                self.fatal_cleanup.store(true, Ordering::Release);
                self.log("error", &format!("qq: handler cleanup failed: {error}"));
                self.cleanup_errors
                    .lock()
                    .expect("qq cleanup errors")
                    .push(ChannelError::error(format!(
                        "qq: handler cleanup failed: {error}"
                    )));
            }
        }
        let outbound = std::mem::take(&mut self.outbound.lock().expect("qq outbound").tasks);
        for task in outbound {
            if let Err(error) = task.await {
                self.fatal_cleanup.store(true, Ordering::Release);
                self.cleanup_errors
                    .lock()
                    .expect("qq cleanup errors")
                    .push(ChannelError::error(format!(
                        "qq: outbound cleanup failed: {error}"
                    )));
            }
        }
        for mut stream in retry_streams {
            if let Err(error) = stream
                .cancel_and_drain(JsString::from("qq channel stopped"))
                .await
            {
                self.cleanup_errors
                    .lock()
                    .expect("qq cleanup errors")
                    .push(error);
                self.failed_streams
                    .lock()
                    .expect("qq failed streams")
                    .push(stream);
            }
        }
        let errors = std::mem::take(&mut *self.cleanup_errors.lock().expect("qq cleanup errors"));
        if !errors.is_empty() {
            return Err(ChannelError::Aggregate {
                message: "qq: owned turn cleanup failed".into(),
                errors,
            });
        }
        let chats: Vec<_> = self
            .state
            .lock()
            .expect("qq state")
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
        let mut attempt = self.cleanup.lock().expect("qq cleanup attempt");
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
                    .expect("qq failed streams")
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
        self.0.outbound.lock().expect("qq outbound").accepting = false;
        self.0.running.store(false, Ordering::Release);
        self.0.stopping.send_replace(true);
        if let Some(receiver) = self.0.receiver.lock().expect("qq receiver").as_ref() {
            let _ = receiver.stop.send(true);
        }
        self.0.log("info", &format!("qq: stopping ({reason})"));
        let mut receipt = self.0.cleanup_receipt();
        Box::pin(async move {
            loop {
                if let Some(outcome) = receipt.borrow().clone() {
                    return outcome.result;
                }
                receipt.changed().await.map_err(|_| {
                    ChannelError::error("qq: cleanup worker exited without a result")
                })?;
            }
        })
    }
}

impl ChannelAdapter for QQChannel {
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
                let mut cleanup = shared.cleanup.lock().expect("qq cleanup attempt");
                if let Some(attempt) = cleanup.as_ref() {
                    if !attempt
                        .result
                        .borrow()
                        .as_ref()
                        .is_some_and(|outcome| outcome.result.is_ok())
                    {
                        return Err(ChannelError::error("qq: prior stop has not drained"));
                    }
                    *cleanup = None;
                }
            }
            {
                let mut state = shared.state.lock().expect("qq state");
                state.gateway = deps.turn_gateway;
                state.logger = deps.log;
            }
            if shared.app_id.is_empty() || shared.client_secret.is_empty() {
                shared.log(
                    "info",
                    "qq: QQ_BOT_APPID or QQ_BOT_SECRET not set, QQ channel disabled.",
                );
                return Ok(Arc::new(NoopHandle) as Arc<dyn ChannelHandle>);
            }
            shared.stopping.send_replace(false);
            shared.running.store(true, Ordering::Release);
            shared.outbound.lock().expect("qq outbound").accepting = true;
            let gateway_url = format!("{}/gateway", shared.rest_url);
            let discovery = shared
                .json_request(Method::GET, &gateway_url, None, true)
                .await;
            let url = discovery.and_then(|value| {
                value
                    .get("url")
                    .and_then(JsValue::as_string)
                    .map(|url| String::from_utf16_lossy(url.units()))
                    .ok_or_else(|| "getGateway missing url".into())
            });
            let url = match url {
                Ok(url) => url,
                Err(error) => {
                    shared.log("error", &format!("qq: failed to connect: Error: {error}"));
                    return Ok(Arc::new(Handle(shared)) as Arc<dyn ChannelHandle>);
                }
            };
            shared.log(
                "info",
                &format!(
                    "qq: connecting to QQ Official Bot Gateway (appId={})...",
                    String::from_utf16_lossy(shared.app_id.units())
                ),
            );
            let (stop, rx) = watch::channel(false);
            let task = tokio::spawn(shared.clone().receive(url, rx));
            *shared.receiver.lock().expect("qq receiver") = Some(ReceiveTask { stop, task });
            Ok(Arc::new(Handle(shared)) as Arc<dyn ChannelHandle>)
        })
    }
}

/// Each loader construction gets its own helper state; the responder remains
/// the one native gateway owner shared by the host.
pub fn register(mounts: &mut crate::channel::loader::ChannelLoaderMounts, responder: ResponderRef) {
    mounts.register(
        CHANNEL_KEY,
        Arc::new(move |options: &JsObject| {
            Ok(Arc::new(QQChannel::new(
                QQChannelOptions::from_config(options),
                js_helper_interaction(responder.clone()),
            )) as Arc<dyn ChannelAdapter>)
        }),
    );
}

impl QQChannelOptions {
    pub(crate) fn from_config(options: &JsObject) -> Self {
        let strings = |key: &str| {
            options.get(key).and_then(|value| {
                if let JsValue::Array(values) = value {
                    Some(
                        values
                            .iter()
                            .filter_map(JsValue::as_string)
                            .cloned()
                            .collect(),
                    )
                } else {
                    None
                }
            })
        };
        Self {
            app_id: options.get("appId").and_then(JsValue::as_string).cloned(),
            client_secret: options
                .get("clientSecret")
                .and_then(JsValue::as_string)
                .cloned(),
            allow_groups: strings("allowGroups"),
            trigger_prefixes: strings("triggerPrefixes"),
            max_message_length: configured_message_limit(options.get("maxMessageLength")),
            ..Default::default()
        }
    }
    pub(crate) fn with_network(
        mut self,
        network: Arc<crate::channel::network::ChannelNetwork>,
    ) -> Self {
        self.transport = Some(network.clone());
        self.network = Some(network);
        self
    }
}

fn object(fields: &[(&str, JsValue)]) -> JsValue {
    let mut object = JsObject::new();
    for (key, value) in fields {
        object.insert((*key).into(), value.clone());
    }
    JsValue::Object(object)
}

fn configured_message_limit(value: Option<&JsValue>) -> Option<usize> {
    match value {
        Some(JsValue::Number(n)) if n.is_finite() => {
            Some(if *n <= 0.0 { 1 } else { (*n as usize).max(1) })
        }
        _ => None,
    }
}
