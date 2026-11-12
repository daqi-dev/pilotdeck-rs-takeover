//! Enterprise WeChat aibot WebSocket adapter; distinct from wecomCallback.
mod commands;
mod deliverables;
mod mapper;
mod media;
#[cfg(test)]
mod tests;
mod wire;
use crate::channel::{
    ActiveChat,
    im_interaction::{JsInteractionPort, ResponderRef, js_helper_interaction},
    network::ChannelNetwork,
    protocol::*,
    sms,
};
use futures_util::{FutureExt, SinkExt, StreamExt};
pub use mapper::{MappedSession, Scope, WeComSessionMapper, WeComSessionMapperState};
use pilotdeck_protocol::{JsObject, JsString, JsValue, parse_json, stringify_json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::Instant,
};
use tokio_tungstenite::tungstenite::Message;
use wire::{lower, object, string, trim};
pub const CHANNEL_KEY: &str = "wecom";
pub type Env = Arc<dyn Fn(&str) -> Option<JsString> + Send + Sync>;
#[derive(Default)]
pub struct WeComChannelOptions {
    /// Test seam; production retains os.tmpdir()/pilotdeck-wecom-media.
    pub media_cache_dir: Option<std::path::PathBuf>,
    pub command_host: Option<Arc<dyn crate::channel::commands::CommandHost>>,
    pub bot_key: Option<JsString>,
    pub extra: Option<JsObject>,
    pub mapper: Option<WeComSessionMapper>,
    pub env: Option<Env>,
    pub on_state_change: Option<Arc<dyn Fn(WeComSessionMapperState) + Send + Sync>>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Policy {
    Open,
    Allowlist,
    Disabled,
    Pairing,
}
fn policy(v: JsString) -> Policy {
    match lower(&trim(v)).as_str() {
        "open" => Policy::Open,
        "allowlist" => Policy::Allowlist,
        "disabled" => Policy::Disabled,
        _ => Policy::Pairing,
    }
}
struct State {
    mapper: WeComSessionMapper,
    gateway: Option<Arc<dyn TurnGateway>>,
    logger: Option<ChannelLog>,
    seen: VecDeque<(JsString, Instant)>,
    reply_requests: VecDeque<(JsString, JsString)>,
    chat_requests: VecDeque<(JsString, JsString)>,
}
#[derive(Clone)]
struct Inbound {
    chat: JsString,
    user: JsString,
    group: bool,
    interaction: JsString,
    text: JsString,
    message_id: JsString,
    attachments: Option<Vec<JsValue>>,
}
struct Shared {
    admission: Mutex<()>,
    media_cache_dir: std::path::PathBuf,
    fatal_cleanup: std::sync::atomic::AtomicBool,
    command_host: Arc<dyn crate::channel::commands::CommandHost>,
    key: ChannelKey,
    bot_id: JsString,
    secret: JsString,
    ws_url: String,
    device_id: String,
    network: Arc<ChannelNetwork>,
    interaction: Arc<dyn JsInteractionPort>,
    dm_policy: Policy,
    allow_from: Vec<JsString>,
    group_policy: Policy,
    group_allow_from: Vec<JsString>,
    groups: JsObject,
    per_user: bool,
    batch_delay: Duration,
    split_delay: Duration,
    state: Mutex<State>,
    on_state_change: Option<Arc<dyn Fn(WeComSessionMapperState) + Send + Sync>>,
    active: Arc<Mutex<HashSet<JsString>>>,
    stopping: watch::Sender<bool>,
    writer: Mutex<Option<mpsc::UnboundedSender<JsValue>>>,
    pending: Mutex<HashMap<JsString, oneshot::Sender<Result<JsValue, ChannelError>>>>,
    receiver: Mutex<Option<JoinHandle<()>>>,
    handlers: Mutex<Vec<JoinHandle<()>>>,
    batches: Mutex<HashMap<JsString, (Inbound, u64)>>,
    batch_sequence: std::sync::atomic::AtomicU64,
    cleanup: Mutex<Option<CleanupAttempt>>,
    failed_streams: Mutex<Vec<Box<dyn TurnEventStream>>>,
    errors: Mutex<Vec<ChannelError>>,
}
#[derive(Clone)]
pub struct WeComChannel(Arc<Shared>);
impl WeComChannel {
    pub fn new(options: WeComChannelOptions, interaction: Arc<dyn JsInteractionPort>) -> Self {
        Self::with_network(options, interaction, Arc::new(ChannelNetwork::default()))
    }
    pub(crate) fn with_network(
        options: WeComChannelOptions,
        interaction: Arc<dyn JsInteractionPort>,
        network: Arc<ChannelNetwork>,
    ) -> Self {
        let env = options
            .env
            .unwrap_or_else(|| Arc::new(|key| std::env::var(key).ok().map(JsString::from)));
        let extra = options.extra.unwrap_or_default();
        let value = |keys: &[&str], environment: &str, fallback: &str| -> JsValue {
            keys.iter()
                .find_map(|k| extra.get(k).filter(|v| !sms::nullish(v)))
                .cloned()
                .or_else(|| env(environment).map(JsValue::String))
                .unwrap_or_else(|| JsValue::from(fallback))
        };
        let text = |keys: &[&str], environment: &str, fallback: &str| {
            trim(sms::js_string_of(&value(keys, environment, fallback)))
        };
        let delay = |keys: &[&str], fallback: u64| {
            let value = value(keys, "", "");
            let number = match value {
                JsValue::Number(n) => Some(n),
                JsValue::String(ref s) => String::from_utf16_lossy(s.units())
                    .trim_start()
                    .split(|c: char| !c.is_ascii_digit() && c != '-')
                    .next()
                    .and_then(|n| n.parse::<f64>().ok()),
                _ => None,
            };
            Duration::from_secs_f64(
                number
                    .filter(|v| v.is_finite() && *v >= 0.0)
                    .map(|v| v / 1000.0)
                    .unwrap_or(fallback as f64 / 1000.0),
            )
        };
        let bot_id = options
            .bot_key
            .map(trim)
            .unwrap_or_else(|| text(&["bot_id", "botId"], "WECOM_BOT_ID", ""));
        let secret = text(&["botSecret", "secret"], "WECOM_SECRET", "");
        let ws_url = text(
            &["websocket_url", "websocketUrl"],
            "WECOM_WEBSOCKET_URL",
            "",
        );
        let dm_policy = policy(text(
            &["dm_policy", "dmPolicy"],
            "WECOM_DM_POLICY",
            "pairing",
        ));
        let group_policy = policy(text(
            &["group_policy", "groupPolicy"],
            "WECOM_GROUP_POLICY",
            "pairing",
        ));
        let allow_from = wire::list(Some(&value(
            &["allow_from", "allowFrom"],
            "WECOM_ALLOWED_USERS",
            "",
        )));
        let group_allow_from = wire::list(
            extra
                .get("group_allow_from")
                .filter(|v| !sms::nullish(v))
                .or_else(|| extra.get("groupAllowFrom")),
        );
        let batch_delay = delay(&["text_batch_delay_ms", "textBatchDelayMs"], 600);
        let split_delay = delay(
            &["text_batch_split_delay_ms", "textBatchSplitDelayMs"],
            2000,
        );
        let per_user = extra
            .get("group_sessions_per_user")
            .filter(|v| matches!(v, JsValue::Bool(_)))
            .or_else(|| {
                extra
                    .get("groupSessionsPerUser")
                    .filter(|v| matches!(v, JsValue::Bool(_)))
            })
            .and_then(|v| {
                if let JsValue::Bool(v) = v {
                    Some(*v)
                } else {
                    None
                }
            })
            .unwrap_or(true);
        let groups = extra
            .get("groups")
            .and_then(JsValue::as_object)
            .cloned()
            .unwrap_or_default();
        Self(Arc::new(Shared {
            admission: Mutex::new(()),
            media_cache_dir: options
                .media_cache_dir
                .unwrap_or_else(|| std::env::temp_dir().join("pilotdeck-wecom-media")),
            fatal_cleanup: std::sync::atomic::AtomicBool::new(false),
            command_host: options
                .command_host
                .unwrap_or_else(|| Arc::new(commands::UnavailableHost)),
            key: ChannelKey::new(CHANNEL_KEY),
            bot_id,
            secret,
            ws_url: if ws_url.is_empty() {
                "wss://openws.work.weixin.qq.com".into()
            } else {
                String::from_utf16_lossy(ws_url.units())
            },
            device_id: uuid::Uuid::new_v4().simple().to_string(),
            network,
            interaction,
            dm_policy,
            allow_from,
            group_policy,
            group_allow_from,
            groups,
            per_user,
            batch_delay,
            split_delay,
            state: Mutex::new(State {
                mapper: options.mapper.unwrap_or_default(),
                gateway: None,
                logger: None,
                seen: VecDeque::new(),
                reply_requests: VecDeque::new(),
                chat_requests: VecDeque::new(),
            }),
            on_state_change: options.on_state_change,
            active: Arc::default(),
            stopping: watch::channel(false).0,
            writer: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            receiver: Mutex::new(None),
            handlers: Mutex::new(Vec::new()),
            batches: Mutex::new(HashMap::new()),
            batch_sequence: std::sync::atomic::AtomicU64::new(0),
            cleanup: Mutex::new(None),
            failed_streams: Mutex::new(Vec::new()),
            errors: Mutex::new(Vec::new()),
        }))
    }
}
fn encode(value: &JsValue) -> String {
    stringify_json(value).ok().flatten().unwrap_or_default()
}
fn req_id(prefix: &str) -> JsString {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple()).into()
}
fn frame(cmd: &str, id: JsString, body: JsValue) -> JsValue {
    object([
        ("cmd", JsValue::from(cmd)),
        ("headers", object([("req_id", JsValue::String(id))])),
        ("body", body),
    ])
}
fn payload_id(payload: &JsValue) -> JsString {
    payload
        .get("headers")
        .map(|v| string(v, "req_id"))
        .unwrap_or_else(|| "".into())
}
fn remember(map: &mut VecDeque<(JsString, JsString)>, key: JsString, value: JsString) {
    if let Some((_, v)) = map.iter_mut().find(|(k, _)| *k == key) {
        *v = value;
    } else {
        map.push_back((key, value));
    }
    while map.len() > 1000 {
        map.pop_front();
    }
}
impl Shared {
    fn spawn_handler(
        self: &Arc<Self>,
        future: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        // Reserve an owned worker before the first synchronous poll. Stop
        // serializes its admission barrier with this reservation, and thus
        // cannot observe an empty registry between effects and registration.
        let handoff = {
            let _admission = self.admission.lock().expect("channel admission");
            if *self.stopping.borrow() {
                return;
            }
            let mut handlers = self.handlers.lock().expect("channel handlers");
            let mut index = 0;
            while index < handlers.len() {
                if !handlers[index].is_finished() {
                    index += 1;
                    continue;
                }
                let mut task = handlers.swap_remove(index);
                if let Some(Err(error)) = (&mut task).now_or_never() {
                    self.fatal_cleanup
                        .store(true, std::sync::atomic::Ordering::Release);
                    self.errors
                        .lock()
                        .expect("channel errors")
                        .push(ChannelError::error(error.to_string()));
                }
            }
            let (sender, receiver) = tokio::sync::oneshot::channel::<Option<BoxFuture<()>>>();
            let shared = self.clone();
            handlers.push(tokio::spawn(async move {
                match receiver.await {
                    Ok(Some(future)) => future.await,
                    Ok(None) => {}
                    Err(_) => {
                        shared
                            .fatal_cleanup
                            .store(true, std::sync::atomic::Ordering::Release);
                        shared
                            .errors
                            .lock()
                            .expect("channel errors")
                            .push(ChannelError::error(
                                "Channel handler admission terminated before handing off its owner",
                            ));
                    }
                }
            }));
            sender
        };
        let mut future: BoxFuture<()> = Box::pin(future);
        if future.as_mut().now_or_never().is_some() {
            let _ = handoff.send(None);
        } else {
            let _ = handoff.send(Some(future));
        }
    }

    fn log(&self, level: &str, text: &str) {
        let logger = self.state.lock().expect("wecom state").logger.clone();
        if let Some(logger) = logger {
            logger(level, text);
        }
    }
    async fn stopped(&self) {
        let mut stop = self.stopping.subscribe();
        if *stop.borrow() {
            return;
        }
        let _ = stop.changed().await;
    }
    async fn write_socket(
        &self,
        socket: &mut crate::channel::ws_transport::ChannelWebSocket,
        message: Message,
    ) -> bool {
        tokio::select! {biased;_=self.stopped()=>false,result=socket.send(message)=>result.is_ok()}
    }
    async fn connect(
        &self,
    ) -> Result<crate::channel::ws_transport::ChannelWebSocket, ChannelError> {
        let (mut socket, _) = tokio::select! {biased;_=self.stopped()=>return Err(ChannelError::error("WeCom adapter stopped")),result=tokio::time::timeout(Duration::from_secs(20),self.network.connect_ws(&self.ws_url))=>result.map_err(|_|ChannelError::error("WeCom WebSocket connect timeout"))?.map_err(|e|ChannelError::error(e.to_string()))?};
        let id = req_id("subscribe");
        let subscribe = frame(
            "aibot_subscribe",
            id.clone(),
            object([
                ("bot_id", JsValue::String(self.bot_id.clone())),
                ("secret", JsValue::String(self.secret.clone())),
                ("device_id", JsValue::from(self.device_id.as_str())),
            ]),
        );
        socket
            .send(Message::Text(encode(&subscribe).into()))
            .await
            .map_err(|e| ChannelError::error(e.to_string()))?;
        let auth=tokio::time::timeout(Duration::from_secs(20),async{
            loop {let message=tokio::select!{biased;_=self.stopped()=>return Err(ChannelError::error("WeCom adapter stopped")),message=socket.next()=>message};match message{Some(Ok(Message::Text(raw)))=>if let Ok(payload)=parse_json(&raw){if payload_id(&payload)==id{return Ok(payload);}},Some(Ok(Message::Ping(data)))=>{socket.send(Message::Pong(data)).await.map_err(|e|ChannelError::error(e.to_string()))?;},Some(Err(e))=>return Err(ChannelError::error(e.to_string())),Some(Ok(Message::Close(_)))|None=>return Err(ChannelError::error("WeCom connection interrupted")),_=>{}}}
        }).await.map_err(|_|ChannelError::error("Timeout waiting for WeCom response"))??;
        if wire::response_error(&auth).is_some() {
            let body = auth.get("body");
            let code = body
                .and_then(|b| b.get("errcode"))
                .filter(|v| !sms::nullish(v))
                .or_else(|| auth.get("errcode"))
                .map(sms::js_string_of)
                .unwrap_or_else(|| "".into());
            let message = body
                .and_then(|b| b.get("errmsg"))
                .filter(|v| !sms::nullish(v))
                .or_else(|| auth.get("errmsg"))
                .filter(|v| !sms::nullish(v))
                .map(sms::js_string_of)
                .unwrap_or_else(|| "auth failed".into());
            let _ = tokio::time::timeout(Duration::from_secs(2), socket.close(None)).await;
            return Err(ChannelError::error(format!(
                "{} (errcode={})",
                String::from_utf16_lossy(message.units()),
                String::from_utf16_lossy(code.units())
            )));
        }
        Ok(socket)
    }
    fn fail_pending(&self, message: &str) {
        let pending = std::mem::take(&mut *self.pending.lock().expect("wecom pending"));
        for (_, sender) in pending {
            let _ = sender.send(Err(ChannelError::error(message)));
        }
    }
    async fn request(
        &self,
        cmd: &str,
        id: JsString,
        body: JsValue,
    ) -> Result<JsValue, ChannelError> {
        if *self.stopping.borrow() {
            return Err(ChannelError::error("WeCom adapter stopped"));
        }
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("wecom pending")
            .insert(id.clone(), tx);
        let writer = self.writer.lock().expect("wecom writer").clone();
        let sent = writer.is_some_and(|writer| writer.send(frame(cmd, id.clone(), body)).is_ok());
        if !sent {
            self.pending.lock().unwrap().remove(&id);
            return Err(ChannelError::error("WeCom websocket is not connected"));
        }
        let result = tokio::select! {biased;_=self.stopped()=>Err(ChannelError::error("WeCom adapter stopped")),result=tokio::time::timeout(Duration::from_secs(15),rx)=>match result{Ok(Ok(result))=>result,Ok(Err(_))=>Err(ChannelError::error("WeCom connection interrupted")),Err(_)=>Err(ChannelError::error("Timeout waiting for WeCom response"))}};
        self.pending.lock().unwrap().remove(&id);
        result
    }
    async fn receive(self: Arc<Self>, mut socket: crate::channel::ws_transport::ChannelWebSocket) {
        let mut attempt = 0usize;
        loop {
            let (sender, mut outgoing) = mpsc::unbounded_channel();
            *self.writer.lock().expect("wecom writer") = Some(sender);
            let mut heartbeat = tokio::time::interval_at(
                tokio::time::Instant::now() + Duration::from_secs(30),
                Duration::from_secs(30),
            );
            loop {
                tokio::select! {biased;
                    _=self.stopped()=>{self.writer.lock().unwrap().take();self.fail_pending("WeCom adapter stopped");let _=tokio::time::timeout(Duration::from_secs(2),socket.close(None)).await;return;},
                    message=socket.next()=>match message {
                        Some(Ok(Message::Text(raw)))=>{let Ok(payload)=parse_json(&raw) else{continue;};let cmd=string(&payload,"cmd");let id=payload_id(&payload);if !cmd.equals_str("aibot_msg_callback") && !cmd.equals_str("aibot_callback") && !cmd.equals_str("aibot_event_callback") && !id.is_empty(){if let Some(sender)=self.pending.lock().unwrap().remove(&id){let _=sender.send(Ok(payload));continue;}}
                            if cmd.equals_str("aibot_msg_callback") || cmd.equals_str("aibot_callback"){let shared=self.clone();self.spawn_handler(async move{shared.callback(payload).await;});}},
                        Some(Ok(Message::Ping(data)))=>{if !self.write_socket(&mut socket,Message::Pong(data)).await{break;}},
                        Some(Ok(Message::Close(_)))|None|Some(Err(_))=>break,_=>{},
                    },
                    Some(payload)=outgoing.recv()=>{if !self.write_socket(&mut socket,Message::Text(encode(&payload).into())).await{break;}},
                    _=heartbeat.tick()=>{if !self.write_socket(&mut socket,Message::Text(encode(&frame("ping",req_id("ping"),object([]))).into())).await{break;}},
                }
            }
            self.writer.lock().unwrap().take();
            self.fail_pending("WeCom connection interrupted");
            self.log("warn", "wecom: WebSocket closed");
            loop {
                let delay = [2, 5, 10, 30, 60][attempt.min(4)];
                attempt += 1;
                tokio::select! {biased;_=self.stopped()=>return,_=tokio::time::sleep(Duration::from_secs(delay))=>{}}
                match self.connect().await {
                    Ok(next) => {
                        socket = next;
                        attempt = 0;
                        self.log("info", "wecom: reconnected");
                        break;
                    }
                    Err(e) => self.log("warn", &format!("wecom: reconnect failed: {e}")),
                }
            }
        }
    }
    fn allowed(&self, chat: &JsString, user: &JsString, group: bool) -> bool {
        if group {
            if chat.is_empty()
                || self.group_policy == Policy::Disabled
                || self.group_policy == Policy::Pairing
            {
                return false;
            }
            if self.group_policy == Policy::Allowlist
                && !wire::entry_matches(&self.group_allow_from, chat)
            {
                return false;
            }
            let exact = self
                .groups
                .get_key(chat)
                .filter(|value| matches!(value, JsValue::Object(_)))
                .or_else(|| {
                    self.groups
                        .entries()
                        .iter()
                        .find(|(k, value)| {
                            wire::equal_folded(k, chat) && matches!(value, JsValue::Object(_))
                        })
                        .map(|(_, v)| v)
                })
                .or_else(|| {
                    self.groups
                        .get("*")
                        .filter(|value| matches!(value, JsValue::Object(_)))
                });
            let allow = exact
                .map(|v| {
                    wire::list(
                        v.get("allow_from")
                            .filter(|v| !sms::nullish(v))
                            .or_else(|| v.get("allowFrom")),
                    )
                })
                .unwrap_or_default();
            allow.is_empty() || wire::entry_matches(&allow, user)
        } else {
            !user.is_empty()
                && (self.dm_policy == Policy::Open
                    || self.dm_policy == Policy::Allowlist
                        && wire::entry_matches(&self.allow_from, user))
        }
    }
    async fn callback(self: Arc<Self>, payload: JsValue) {
        if *self.stopping.borrow() {
            return;
        }
        let Some(body) = payload.get("body") else {
            return;
        };
        let id = payload_id(&payload);
        let message_id = body
            .get("msgid")
            .filter(|v| !sms::nullish(v))
            .map(sms::js_string_of)
            .map(trim)
            .unwrap_or_else(|| id.clone());
        {
            let mut state = self.state.lock().unwrap();
            let now = Instant::now();
            state
                .seen
                .retain(|(_, at)| now.duration_since(*at) <= Duration::from_secs(300));
            if state.seen.iter().any(|(key, _)| *key == message_id) {
                return;
            }
            state.seen.push_back((message_id.clone(), now));
            while state.seen.len() > 1000 {
                state.seen.pop_front();
            }
        }
        let user = body
            .get("from")
            .map(|v| trim(string(v, "userid")))
            .unwrap_or_else(|| "".into());
        let chat = body
            .get("chatid")
            .filter(|v| !sms::nullish(v))
            .map(sms::js_string_of)
            .map(trim)
            .unwrap_or_else(|| user.clone());
        let group = lower(&string(body, "chattype")) == "group";
        if !self.allowed(&chat, &user, group) {
            return;
        }
        if !id.is_empty() {
            let mut state = self.state.lock().unwrap();
            remember(&mut state.reply_requests, message_id.clone(), id.clone());
            remember(&mut state.chat_requests, chat.clone(), id);
        }
        let attachments = self.inbound_attachments(body).await;
        let text = wire::message_text(body);
        let text = if group {
            wire::strip_mention(&text)
        } else {
            text
        };
        if text.is_empty() && attachments.is_empty() {
            return;
        }
        let interaction = if group && self.per_user && !user.is_empty() {
            sms::concat_units(&[chat.units(), &[58], user.units()])
        } else {
            chat.clone()
        };
        let text = if text.is_empty() && !attachments.is_empty() {
            JsString::from("用户发送了企业微信附件。")
        } else {
            text
        };
        let input = Inbound {
            chat,
            user,
            group,
            interaction,
            text,
            message_id,
            attachments: Some(attachments),
        };
        if self.answer_interaction(&input).await {
            return;
        }
        if input.text.units().first() == Some(&47)
            || input.attachments.as_ref().is_some_and(|v| !v.is_empty())
        {
            self.dispatch(input).await;
            return;
        }
        let mut input = input;
        input.attachments = None;
        if self.batch_delay.is_zero() {
            self.dispatch(input).await;
            return;
        }
        let key = input.interaction.clone();
        let sequence = self
            .batch_sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let delay = if input.text.units().len() >= 3900 {
            self.split_delay
        } else {
            self.batch_delay
        };
        {
            let mut batches = self.batches.lock().unwrap();
            if let Some((prior, generation)) = batches.get_mut(&key) {
                prior.text = sms::concat_units(&[prior.text.units(), &[10], input.text.units()]);
                prior.message_id = input.message_id;
                *generation = sequence;
            } else {
                batches.insert(key.clone(), (input, sequence));
            }
        }
        let shared = self.clone();
        self.spawn_handler(async move {
            tokio::select! {biased;_=shared.stopped()=>return,_=tokio::time::sleep(delay)=>{}}
            let batch = {
                let mut batches = shared.batches.lock().unwrap();
                if batches
                    .get(&key)
                    .is_some_and(|(_, generation)| *generation == sequence)
                {
                    batches.remove(&key).map(|(batch, _)| batch)
                } else {
                    None
                }
            };
            if let Some(batch) = batch {
                shared.dispatch(batch).await;
            }
        });
    }
    async fn answer_interaction(&self, input: &Inbound) -> bool {
        let key = &input.interaction;
        if self.interaction.elicitation_pending(key) {
            match tokio::select! {biased;_=self.stopped()=>return true,result=self.interaction.elicitation_answer(key,&input.text)=>result}
            {
                Ok(Some(text)) if !text.is_empty() => {
                    self.send_reply(&input.chat, &text, Some(&input.message_id), input.group)
                        .await;
                }
                Err(e) => self.log("error", &format!("wecom: elicitation answer error: {e}")),
                _ => {}
            }
            return true;
        }
        if self.interaction.permission_pending(key) {
            match tokio::select! {biased;_=self.stopped()=>return true,result=self.interaction.permission_answer(key,&input.text)=>result}
            {
                Ok(Some(answer)) => {
                    if let Some(text) = answer.text.filter(|v| !v.is_empty()) {
                        if !self
                            .send_reply(&input.chat, &text, Some(&input.message_id), input.group)
                            .await
                        {
                            self.interaction
                                .permission_release_answer(key, answer.answer_token);
                            return true;
                        }
                        if !answer.can_advance && !answer.retry_prompt {
                            return true;
                        }
                        if let Some(prompt) = self
                            .interaction
                            .permission_take_next_prompt(key, answer.answer_token)
                        {
                            let id = self
                                .interaction
                                .permission_prompt_request_id(key, answer.answer_token);
                            let delivered = self
                                .send_reply(
                                    &input.chat,
                                    &prompt,
                                    Some(&input.message_id),
                                    input.group,
                                )
                                .await;
                            self.interaction.permission_confirm_next(
                                key,
                                delivered,
                                id.as_ref(),
                                answer.answer_token,
                            );
                        }
                    }
                }
                Err(e) => self.log("error", &format!("wecom: permission answer error: {e}")),
                _ => {}
            }
            return true;
        }
        false
    }
    async fn dispatch(self: Arc<Self>, input: Inbound) {
        if *self.stopping.borrow() {
            return;
        }
        let (mapped, gateway) = {
            let mut state = self.state.lock().unwrap();
            let scope = Scope {
                chat_id: &input.chat,
                user_id: Some(&input.user),
                group: input.group,
                group_sessions_per_user: self.per_user,
            };
            let mapped = state.mapper.resolve(&scope, &input.text);
            if mapped.command_new {
                if let Some(change) = &self.on_state_change {
                    change(state.mapper.snapshot());
                }
            }
            (mapped, state.gateway.clone())
        };
        if mapped.command_new && mapped.message.is_empty() {
            self.send_reply(
                &input.chat,
                &"已创建新会话。".into(),
                Some(&input.message_id),
                input.group,
            )
            .await;
            return;
        }
        if input.text.units().first() == Some(&47) {
            let context = commands::Context {
                shared: &self,
                input: &input,
            };
            match crate::channel::commands::execute(
                &input.text,
                self.command_host.as_ref(),
                &context,
            )
            .await
            {
                Ok(true) => return,
                Ok(false) => {}
                Err(error) => {
                    self.log(
                        "error",
                        &format!(
                            "wecom: command error: {}",
                            String::from_utf16_lossy(error.0.units())
                        ),
                    );
                    return;
                }
            }
        }
        if mapped.message.is_empty() {
            return;
        }
        let Some(gateway) = gateway else {
            return;
        };
        {
            let mut active = self.active.lock().unwrap();
            if !active.insert(mapped.session_key.clone()) {
                return;
            }
        }
        let _guard = ActiveChat::new(self.active.clone(), mapped.session_key.clone());
        let request = TurnRequest {
            session_key: mapped.session_key.clone(),
            channel_key: self.key.clone(),
            message: mapped.message,
            attachments: input.attachments.clone(),
            allow_plan_mode_tools: Some(false),
            synthetic_messages: Some(vec![object([
                ("text", JsValue::from(wire::DELIVERABLE_HINT)),
                ("purpose", JsValue::from("wecom_deliverable_hint")),
            ])]),
            project_key: mapped.project_key,
        };
        let mut reply = Vec::new();
        let result=async {
            let mut stream=tokio::select!{biased;_=self.stopped()=>return Ok(()),result=gateway.submit(request)=>result?};
            loop {let event=tokio::select!{biased;_=self.stopped()=>{if let Err(error)=stream.cancel_and_drain("wecom channel stopped".into()).await{self.failed_streams.lock().unwrap().push(stream);return Err(error);}break;},result=stream.next()=>result?};let Some(event)=event else{break;};let kind=string(&event,"type");
                if kind.equals_str("elicitation_request"){let prompt=self.interaction.elicitation_capture(&input.interaction,&mapped.session_key,&event);self.send_reply(&input.chat,&prompt,Some(&input.message_id),input.group).await;}
                else if kind.equals_str("permission_request"){if let Some(prompt)=self.interaction.permission_capture(&input.interaction,&mapped.session_key,&event){let delivered=self.send_reply(&input.chat,&prompt,Some(&input.message_id),input.group).await;self.interaction.permission_confirm_initial(&input.interaction,delivered,event.get("requestId").and_then(JsValue::as_string));}}
                else {self.send_event_media(&input,&event).await;if let Some(fragment)=wire::render(&event){reply.extend_from_slice(fragment.units());}}
            }Ok::<_,ChannelError>(())
        }.await;
        if let Err(error) = result {
            self.log("error", &format!("wecom: submitTurn error: {error}"));
            if *self.stopping.borrow() {
                self.errors.lock().unwrap().push(error);
                return;
            }
            reply = "处理消息时发生错误，请重试。".encode_utf16().collect();
        }
        self.interaction.elicitation_clear(&input.interaction);
        self.interaction
            .permission_clear_after_turn(&input.interaction);
        let reply = sms::trim_units(&reply);
        if !reply.is_empty() {
            let reply = JsString::from_utf16(reply.to_vec());
            let (files, warnings) = deliverables::extract(&reply).await;
            let mut visible = reply.units().to_vec();
            for warning in warnings {
                visible.push(10);
                visible.extend_from_slice(warning.units());
            }
            self.send_reply(
                &input.chat,
                &JsString::from_utf16(visible),
                Some(&input.message_id),
                input.group,
            )
            .await;
            for path in files {
                self.send_local_file(&input, &path).await;
            }
        }
    }
    fn reply_id(&self, chat: &JsString, message_id: Option<&JsString>) -> Option<JsString> {
        {
            let state = self.state.lock().unwrap();
            message_id
                .and_then(|id| {
                    state
                        .reply_requests
                        .iter()
                        .find(|(key, _)| key == id)
                        .map(|(_, value)| value.clone())
                })
                .or_else(|| {
                    state
                        .chat_requests
                        .iter()
                        .find(|(key, _)| key == chat)
                        .map(|(_, value)| value.clone())
                })
        }
    }
    async fn send_reply(
        &self,
        chat: &JsString,
        text: &JsString,
        message_id: Option<&JsString>,
        group: bool,
    ) -> bool {
        let reply_id = self.reply_id(chat, message_id);
        let body = wire::markdown(text);
        let result = if let Some(id) = reply_id {
            self.request("aibot_respond_msg", id, body).await
        } else if group {
            return false;
        } else {
            let mut body = body;
            if let JsValue::Object(obj) = &mut body {
                obj.insert("chatid".into(), JsValue::String(chat.clone()));
            }
            self.request("aibot_send_msg", req_id("aibot_send_msg"), body)
                .await
        };
        match result {
            Ok(response) => {
                if let Some(error) = wire::response_error(&response) {
                    self.log("error", &format!("wecom: sendReply error: {error}"));
                    false
                } else {
                    true
                }
            }
            Err(error) => {
                self.log("error", &format!("wecom: sendReply failed: {error}"));
                false
            }
        }
    }
}
struct Handle(Arc<Shared>);
impl Shared {
    async fn cleanup_all(self: Arc<Self>, reason: String) -> Result<(), ChannelError> {
        self.log("info", &format!("wecom: stopping ({reason})"));
        let receiver = self.receiver.lock().unwrap().take();
        if let Some(task) = receiver {
            if let Err(e) = task.await {
                self.fatal_cleanup
                    .store(true, std::sync::atomic::Ordering::Release);
                self.errors
                    .lock()
                    .unwrap()
                    .push(ChannelError::error(e.to_string()));
            }
        }
        loop {
            let handlers = std::mem::take(&mut *self.handlers.lock().unwrap());
            if handlers.is_empty() {
                break;
            }
            for task in handlers {
                if let Err(e) = task.await {
                    self.fatal_cleanup
                        .store(true, std::sync::atomic::Ordering::Release);
                    self.errors
                        .lock()
                        .unwrap()
                        .push(ChannelError::error(e.to_string()));
                }
            }
        }
        let mut streams = std::mem::take(&mut *self.failed_streams.lock().unwrap());
        while let Some(mut stream) = streams.pop() {
            if let Err(error) = stream
                .cancel_and_drain("wecom channel stopped".into())
                .await
            {
                streams.push(stream);
                self.failed_streams.lock().unwrap().extend(streams);
                return Err(error);
            }
        }
        if self
            .fatal_cleanup
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(ChannelError::error(
                "Channel task panicked; cleanup ownership is unresolved",
            ));
        }
        let errors = std::mem::take(&mut *self.errors.lock().unwrap());
        if !errors.is_empty() {
            return Err(ChannelError::Aggregate {
                message: "wecom cleanup failed".into(),
                errors,
            });
        }
        self.batches.lock().unwrap().clear();
        let mut state = self.state.lock().unwrap();
        state.seen.clear();
        state.reply_requests.clear();
        state.chat_requests.clear();
        Ok(())
    }
    fn begin_stop(
        self: &Arc<Self>,
        reason: String,
    ) -> watch::Receiver<Option<Result<(), ChannelError>>> {
        let admission = self.admission.lock().expect("channel admission");
        self.stopping.send_replace(true);
        self.fail_pending("WeCom adapter stopped");
        drop(admission);
        let mut cleanup = self.cleanup.lock().expect("channel cleanup");
        if let Some(attempt) = cleanup.as_ref() {
            let current = attempt.result.borrow().clone();
            if !matches!(current, Some(Err(_))) {
                return attempt.result.clone();
            }
        }
        let (sender, result) = watch::channel(None);
        let shared = self.clone();
        let worker = tokio::spawn(async move {
            let outcome = shared.cleanup_all(reason).await;
            sender.send_replace(Some(outcome));
        });
        *cleanup = Some(CleanupAttempt {
            result: result.clone(),
            _worker: worker,
        });
        result
    }
}
struct CleanupAttempt {
    result: watch::Receiver<Option<Result<(), ChannelError>>>,
    _worker: JoinHandle<()>,
}
impl ChannelHandle for Handle {
    fn stop(&self, reason: &str) -> BoxFuture<Result<(), ChannelError>> {
        let mut receipt = self.0.begin_stop(reason.to_owned());
        Box::pin(async move {
            loop {
                let outcome = receipt.borrow().clone();
                if let Some(outcome) = outcome {
                    return outcome;
                }
                if receipt.changed().await.is_err() {
                    return Err(ChannelError::error(
                        "Channel cleanup worker terminated before draining",
                    ));
                }
            }
        })
    }
}
struct NoopHandle;
impl ChannelHandle for NoopHandle {
    fn stop(&self, _: &str) -> BoxFuture<Result<(), ChannelError>> {
        Box::pin(async { Ok(()) })
    }
}
impl ChannelAdapter for WeComChannel {
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
                let mut state = shared.state.lock().unwrap();
                state.gateway = deps.turn_gateway;
                state.logger = deps.log;
            }
            if shared.bot_id.is_empty() || shared.secret.is_empty() {
                shared.log("error", "wecom: botKey (bot_id) and secret are required");
                return Ok(Arc::new(NoopHandle) as Arc<dyn ChannelHandle>);
            }
            shared.stopping.send_replace(false);
            match shared.connect().await {
                Ok(socket) => {
                    let cloned = shared.clone();
                    *shared.receiver.lock().unwrap() = Some(tokio::spawn(async move {
                        cloned.receive(socket).await;
                    }));
                    shared.log(
                        "info",
                        &format!(
                            "wecom: connected to {} as bot {}",
                            shared.ws_url,
                            String::from_utf16_lossy(shared.bot_id.units())
                        ),
                    );
                    Ok(Arc::new(Handle(shared)) as Arc<dyn ChannelHandle>)
                }
                Err(error) => {
                    shared.log("error", &format!("wecom: start failed: {error}"));
                    shared.stopping.send_replace(true);
                    shared.fail_pending("WeCom startup failed");
                    Ok(Arc::new(NoopHandle) as Arc<dyn ChannelHandle>)
                }
            }
        })
    }
}
pub fn register(mounts: &mut crate::channel::loader::ChannelLoaderMounts, responder: ResponderRef) {
    register_with_network(mounts, responder, Arc::new(ChannelNetwork::default()));
}
pub(crate) fn register_with_network(
    mounts: &mut crate::channel::loader::ChannelLoaderMounts,
    responder: ResponderRef,
    network: Arc<ChannelNetwork>,
) {
    register_with_services(mounts, responder, network, None);
}
pub(crate) fn register_with_services(
    mounts: &mut crate::channel::loader::ChannelLoaderMounts,
    responder: ResponderRef,
    network: Arc<ChannelNetwork>,
    command_host: Option<Arc<dyn crate::channel::commands::CommandHost>>,
) {
    mounts.register(
        CHANNEL_KEY,
        Arc::new(move |options: &JsObject| {
            Ok(Arc::new(WeComChannel::with_network(
                WeComChannelOptions {
                    command_host: command_host.clone(),
                    bot_key: options
                        .get("botKey")
                        .filter(|v| !sms::nullish(v))
                        .map(sms::js_string_of),
                    extra: options.get("extra").and_then(JsValue::as_object).cloned(),
                    ..Default::default()
                },
                js_helper_interaction(responder.clone()),
                network.clone(),
            )) as Arc<dyn ChannelAdapter>)
        }),
    );
}
