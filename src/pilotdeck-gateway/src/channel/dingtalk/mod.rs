//! Native DingTalk Stream channel. The adapter preserves cc7c4e6 message,
//! session and markdown behavior; native callback subscription repairs the
//! baseline's unavailable optional ESM SDK path (see delivery record).
mod mapper;
#[cfg(test)]
mod tests;
pub(crate) mod wire;
use crate::channel::{
    ActiveChat,
    im_cron::{CronResultDelivery, deliver_chat_cron_result},
    im_interaction::{JsInteractionPort, ResponderRef, js_helper_interaction},
    network::ChannelNetwork,
    protocol::*,
    sms,
};
use crate::runtime::model::http::{ModelHttpTransport, fetch::fetch};
use bytes::Bytes;
use futures_util::{FutureExt, SinkExt, StreamExt};
use http_body_util::BodyExt;
use hyper::{HeaderMap, Method};
pub use mapper::{DingTalkSessionMapper, DingTalkSessionMapperState, MappedSession};
use pilotdeck_protocol::{JsObject, JsString, JsValue, parse_json, stringify_json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::watch, task::JoinHandle};
use tokio_tungstenite::tungstenite::Message;
pub const CHANNEL_KEY: &str = "dingtalk";
pub type Env = Arc<dyn Fn(&str) -> Option<JsString> + Send + Sync>;
#[derive(Default)]
pub struct DingTalkChannelOptions {
    pub client_id: Option<JsString>,
    pub client_secret: Option<JsString>,
    pub mapper: Option<DingTalkSessionMapper>,
    pub env: Option<Env>,
    /// Injectable control endpoint is a test seam, never a loader option.
    pub gateway_url: Option<String>,
    pub transport: Option<Arc<dyn ModelHttpTransport>>,
}
struct State {
    mapper: DingTalkSessionMapper,
    webhooks: HashMap<JsString, JsString>,
    webhook_order: VecDeque<JsString>,
    seen: HashSet<JsString>,
    seen_order: VecDeque<JsString>,
    gateway: Option<Arc<dyn TurnGateway>>,
    logger: Option<ChannelLog>,
}
struct Shared {
    admission: Mutex<()>,
    fatal_cleanup: std::sync::atomic::AtomicBool,
    key: ChannelKey,
    client_id: JsString,
    client_secret: JsString,
    gateway_url: String,
    network: Arc<ChannelNetwork>,
    transport: Arc<dyn ModelHttpTransport>,
    interaction: Arc<dyn JsInteractionPort>,
    state: Mutex<State>,
    active: Arc<Mutex<HashSet<JsString>>>,
    stopping: watch::Sender<bool>,
    receiver: Mutex<Option<JoinHandle<()>>>,
    handlers: Mutex<Vec<JoinHandle<()>>>,
    cleanup: Mutex<Option<CleanupAttempt>>,
    failed_streams: Mutex<Vec<Box<dyn TurnEventStream>>>,
    errors: Mutex<Vec<ChannelError>>,
}
#[derive(Clone)]
pub struct DingTalkChannel(Arc<Shared>);
fn lossy(value: &JsString) -> String {
    String::from_utf16_lossy(value.units())
}
fn string(data: &JsValue, key: &str) -> JsString {
    sms::js_string_of(
        data.get(key)
            .filter(|v| !sms::nullish(v))
            .unwrap_or(&JsValue::String(JsString::from(""))),
    )
}
fn object(entries: impl IntoIterator<Item = (&'static str, JsValue)>) -> JsValue {
    {
        let mut object = JsObject::new();
        for (k, v) in entries {
            object.insert(JsString::from(k), v);
        }
        JsValue::Object(object)
    }
}
impl DingTalkChannel {
    pub fn new(options: DingTalkChannelOptions, interaction: Arc<dyn JsInteractionPort>) -> Self {
        Self::with_network(options, interaction, Arc::new(ChannelNetwork::default()))
    }
    pub(crate) fn with_network(
        options: DingTalkChannelOptions,
        interaction: Arc<dyn JsInteractionPort>,
        network: Arc<ChannelNetwork>,
    ) -> Self {
        let env = options
            .env
            .unwrap_or_else(|| Arc::new(|key| std::env::var(key).ok().map(JsString::from)));
        let trim = |s: JsString| JsString::from_utf16(sms::trim_units(s.units()).to_vec());
        Self(Arc::new(Shared {
            admission: Mutex::new(()),
            fatal_cleanup: std::sync::atomic::AtomicBool::new(false),
            key: ChannelKey::new(CHANNEL_KEY),
            client_id: trim(
                options
                    .client_id
                    .or_else(|| env("DINGTALK_CLIENT_ID"))
                    .unwrap_or_else(|| "".into()),
            ),
            client_secret: trim(
                options
                    .client_secret
                    .or_else(|| env("DINGTALK_CLIENT_SECRET"))
                    .unwrap_or_else(|| "".into()),
            ),
            gateway_url: options
                .gateway_url
                .unwrap_or_else(|| "https://api.dingtalk.com/v1.0/gateway/connections/open".into()),
            transport: options.transport.unwrap_or_else(|| network.clone()),
            network,
            interaction,
            state: Mutex::new(State {
                mapper: options.mapper.unwrap_or_default(),
                webhooks: HashMap::new(),
                webhook_order: VecDeque::new(),
                seen: HashSet::new(),
                seen_order: VecDeque::new(),
                gateway: None,
                logger: None,
            }),
            active: Arc::default(),
            stopping: watch::channel(false).0,
            receiver: Mutex::new(None),
            handlers: Mutex::new(Vec::new()),
            cleanup: Mutex::new(None),
            failed_streams: Mutex::new(Vec::new()),
            errors: Mutex::new(Vec::new()),
        }))
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
        let logger = self.state.lock().expect("dingtalk state").logger.clone();
        if let Some(logger) = logger {
            logger(level, text);
        }
    }
    async fn stopped(&self) {
        let mut rx = self.stopping.subscribe();
        if *rx.borrow() {
            return;
        }
        let _ = rx.changed().await;
    }
    async fn http(&self, url: &str, body: JsValue) -> Result<(u16, Bytes), String> {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("accept", "application/json".parse().unwrap());
        let url = url::Url::parse(url).map_err(|e| format!("TypeError: {e}"))?;
        let body = stringify_json(&body).ok().flatten().unwrap_or_default();
        let exchange = async {
            let response = fetch(
                self.transport.as_ref(),
                url,
                Method::POST,
                headers,
                Bytes::from(body),
            )
            .await
            .map_err(|e| format!("TypeError: {}", e.message))?;
            let status = response.status().as_u16();
            let bytes = response
                .into_body()
                .collect()
                .await
                .map_err(|e| format!("TypeError: {e}"))?
                .to_bytes();
            Ok((status, bytes))
        };
        tokio::select! {biased; _=self.stopped()=>Err("dingtalk channel stopped".into()),result=tokio::time::timeout(Duration::from_secs(600),exchange)=>result.map_err(|_|"TypeError: fetch failed".to_owned())?}
    }
    async fn connect(&self) -> Result<crate::channel::ws_transport::ChannelWebSocket, String> {
        let body = object([
            ("clientId", JsValue::String(self.client_id.clone())),
            ("clientSecret", JsValue::String(self.client_secret.clone())),
            ("ua", JsValue::from("")),
            (
                "subscriptions",
                JsValue::Array(vec![object([
                    ("type", JsValue::from("CALLBACK")),
                    ("topic", JsValue::from(wire::ROBOT_TOPIC)),
                ])]),
            ),
        ]);
        let (status, bytes) = self.http(&self.gateway_url, body).await?;
        if !(200..300).contains(&status) {
            return Err(format!("gateway HTTP {status}"));
        }
        let response =
            parse_json(&String::from_utf8_lossy(&bytes)).map_err(|e| format!("{e:?}"))?;
        let endpoint = lossy(&string(&response, "endpoint"));
        let ticket = lossy(&string(&response, "ticket"));
        if endpoint.is_empty() || ticket.is_empty() {
            return Err("endpoint or ticket is null".into());
        }
        tokio::select! {biased; _=self.stopped()=>Err("dingtalk channel stopped".into()),result=self.network.connect_ws(format!("{endpoint}?ticket={ticket}"))=>result.map(|(ws,_)|ws).map_err(|e|e.to_string())}
    }
    async fn reconnect(&self) -> Option<crate::channel::ws_transport::ChannelWebSocket> {
        self.log("warn", "dingtalk: Stream connection closed");
        let mut delay = 1;
        loop {
            tokio::select! {biased;_=self.stopped()=>return None,_=tokio::time::sleep(Duration::from_secs(delay))=>{}}
            match self.connect().await {
                Ok(socket) => return Some(socket),
                Err(error) => {
                    self.log("warn", &format!("dingtalk: reconnect failed: {error}"));
                    delay = (delay * 2).min(60);
                }
            }
        }
    }
    async fn write_socket(
        &self,
        socket: &mut crate::channel::ws_transport::ChannelWebSocket,
        message: Message,
    ) -> bool {
        tokio::select! {biased;_=self.stopped()=>false,result=socket.send(message)=>result.is_ok()}
    }
    async fn receive(self: Arc<Self>, mut socket: crate::channel::ws_transport::ChannelWebSocket) {
        loop {
            let packet = tokio::select! {biased;_=self.stopped()=>{let _=tokio::time::timeout(Duration::from_secs(2),socket.close(None)).await;return;},packet=socket.next()=>packet};
            let disconnected = match packet {
                Some(Ok(Message::Text(raw))) => {
                    let Ok(frame) = parse_json(&raw) else {
                        continue;
                    };
                    let kind = string(&frame, "type");
                    let headers = frame.get("headers").cloned().unwrap_or(JsValue::Null);
                    let topic = string(&headers, "topic");
                    if kind.equals_str("SYSTEM") && topic.equals_str("ping") {
                        let ack = object([
                            ("code", JsValue::Number(200.0)),
                            ("headers", headers),
                            ("message", JsValue::from("OK")),
                            ("data", frame.get("data").cloned().unwrap_or(JsValue::Null)),
                        ]);
                        !self
                            .write_socket(
                                &mut socket,
                                Message::Text(
                                    stringify_json(&ack)
                                        .ok()
                                        .flatten()
                                        .unwrap_or_default()
                                        .into(),
                                ),
                            )
                            .await
                    } else if (kind.equals_str("CALLBACK") || kind.equals_str("EVENT"))
                        && (topic.is_empty() || topic.equals_str(wire::ROBOT_TOPIC))
                    {
                        let ack = object([
                            ("code", JsValue::Number(200.0)),
                            (
                                "headers",
                                object([
                                    ("contentType", JsValue::from("application/json")),
                                    (
                                        "messageId",
                                        headers.get("messageId").cloned().unwrap_or(JsValue::Null),
                                    ),
                                ]),
                            ),
                            ("message", JsValue::from("OK")),
                            (
                                "data",
                                JsValue::from(if kind.equals_str("EVENT") {
                                    r#"{"status":"SUCCESS"}"#
                                } else {
                                    r#"{"response":null}"#
                                }),
                            ),
                        ]);
                        let delivered = self
                            .write_socket(
                                &mut socket,
                                Message::Text(
                                    stringify_json(&ack)
                                        .ok()
                                        .flatten()
                                        .unwrap_or_default()
                                        .into(),
                                ),
                            )
                            .await;
                        if delivered {
                            let shared = self.clone();
                            self.spawn_handler(async move {
                                shared.on_downstream(frame).await;
                            });
                        }
                        !delivered
                    } else {
                        false
                    }
                }
                Some(Ok(Message::Ping(data))) => {
                    !self.write_socket(&mut socket, Message::Pong(data)).await
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => true,
                _ => false,
            };
            if disconnected {
                drop(socket);
                let Some(next) = self.reconnect().await else {
                    return;
                };
                socket = next;
            }
        }
    }
    async fn on_downstream(self: Arc<Self>, frame: JsValue) {
        if *self.stopping.borrow() {
            return;
        }
        let raw = string(&frame, "data");
        let Ok(data) = parse_json(&lossy(&raw)) else {
            return;
        };
        let null_headers = JsValue::Null;
        let headers = frame.get("headers").unwrap_or(&null_headers);
        let id = data
            .get("msgId")
            .filter(|v| !sms::nullish(v))
            .or_else(|| data.get("messageId").filter(|v| !sms::nullish(v)))
            .or_else(|| headers.get("messageId").filter(|v| !sms::nullish(v)))
            .map(sms::js_string_of)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string().into());
        {
            let mut state = self.state.lock().expect("dingtalk state");
            if state.seen.contains(&id) {
                return;
            }
            state.seen.insert(id.clone());
            state.seen_order.push_back(id);
            if state.seen_order.len() > 2000
                && let Some(old) = state.seen_order.pop_front()
            {
                state.seen.remove(&old);
            }
        }
        let text = wire::text(&data);
        if text.is_empty() {
            return;
        }
        let conversation = string(&data, "conversationId");
        let chat = if conversation.is_empty() {
            string(&data, "senderId")
        } else {
            conversation
        };
        if chat.is_empty() {
            return;
        }
        let webhook = string(&data, "sessionWebhook");
        if !webhook.is_empty() && wire::trusted_webhook(&webhook) {
            let mut state = self.state.lock().expect("dingtalk state");
            if state.webhooks.len() >= 500
                && let Some(old) = state.webhook_order.pop_front()
            {
                state.webhooks.remove(&old);
            }
            if !state.webhooks.contains_key(&chat) {
                state.webhook_order.push_back(chat.clone());
            }
            state.webhooks.insert(chat.clone(), webhook);
        }
        let gateway = self.state.lock().expect("dingtalk state").gateway.clone();
        if self.interaction.elicitation_pending(&chat) && gateway.is_some() {
            match tokio::select! { biased; _=self.stopped()=>return, result=self.interaction.elicitation_answer(&chat, &text)=>result }
            {
                Ok(Some(answer)) if !answer.is_empty() => {
                    self.send_reply(&chat, &answer).await;
                }
                Err(e) => self.log("error", &format!("dingtalk: elicitation answer error: {e}")),
                _ => {}
            }
            return;
        }
        if self.interaction.permission_pending(&chat) && gateway.is_some() {
            match tokio::select! { biased; _=self.stopped()=>return, result=self.interaction.permission_answer(&chat, &text)=>result }
            {
                Ok(Some(answer)) => {
                    if let Some(reply) = answer.text.filter(|t| !t.is_empty()) {
                        if !self.send_reply(&chat, &reply).await {
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
                Err(e) => self.log("error", &format!("dingtalk: permission answer error: {e}")),
                _ => {}
            }
            return;
        }
        let mapped = {
            let mut state = self.state.lock().expect("dingtalk state");
            let mut active = self.active.lock().expect("dingtalk active");
            if active.contains(&chat) {
                drop(active);
                drop(state);
                self.log(
                    "info",
                    &format!("dingtalk: chat {} already active, skipping", lossy(&chat)),
                );
                return;
            }
            let mapped = state.mapper.resolve(&chat, &text);
            if !mapped.message.is_empty() {
                active.insert(chat.clone());
            }
            mapped
        };
        if mapped.command_new && mapped.message.is_empty() {
            self.send_reply(&chat, &"已创建新会话。".into()).await;
            return;
        }
        if mapped.message.is_empty() {
            return;
        }
        let _guard = ActiveChat::new(self.active.clone(), chat.clone());
        let Some(gateway) = gateway else {
            return;
        };
        let request = TurnRequest {
            session_key: mapped.session_key.clone(),
            channel_key: self.key.clone(),
            message: mapped.message,
            ..Default::default()
        };
        let mut reply = Vec::new();
        let result=async {
            let mut stream=tokio::select!{biased;_=self.stopped()=>return Ok(()),result=gateway.submit(request)=>result?};
            tokio::task::yield_now().await;
            loop {
                let event=tokio::select!{biased;_=self.stopped()=>{
                    if let Err(error)=stream.cancel_and_drain("dingtalk channel stopped".into()).await {self.failed_streams.lock().expect("dingtalk streams").push(stream);return Err(error);}break;
                },event=stream.next()=>event?};
                let Some(event)=event else{break;};let kind=string(&event,"type");
                if kind.equals_str("elicitation_request"){let prompt=self.interaction.elicitation_capture(&chat,&mapped.session_key,&event);self.send_reply(&chat,&prompt).await;}
                else if kind.equals_str("permission_request"){if let Some(prompt)=self.interaction.permission_capture(&chat,&mapped.session_key,&event).filter(|p|!p.is_empty()){let delivered=self.send_reply(&chat,&prompt).await;self.interaction.permission_confirm_initial(&chat,delivered,event.get("requestId").and_then(JsValue::as_string));}}
                else if let Some(fragment)=wire::render(&event){reply.extend_from_slice(fragment.units());}
            }Ok::<_,ChannelError>(())
        }.await;
        if let Err(error) = result {
            self.log("error", &format!("dingtalk: submitTurn error: {error}"));
            if *self.stopping.borrow() {
                self.errors.lock().expect("dingtalk errors").push(error);
                return;
            }
            reply = "处理消息时发生错误，请重试。".encode_utf16().collect();
        }
        self.interaction.elicitation_clear(&chat);
        self.interaction.permission_clear_after_turn(&chat);
        let reply = sms::trim_units(&reply);
        if !reply.is_empty() {
            self.send_reply(&chat, &JsString::from_utf16(reply.to_vec()))
                .await;
        }
    }
    async fn send_reply(&self, chat: &JsString, text: &JsString) -> bool {
        if *self.stopping.borrow() {
            return false;
        }
        let webhook = self
            .state
            .lock()
            .expect("dingtalk state")
            .webhooks
            .get(chat)
            .cloned();
        let Some(webhook) = webhook else {
            self.log(
                "warn",
                &format!(
                    "dingtalk: no sessionWebhook for chat {}, cannot send",
                    lossy(chat)
                ),
            );
            return false;
        };
        if !wire::trusted_webhook(&webhook) {
            return false;
        }
        let text = JsString::from_utf16(text.units()[..text.units().len().min(20_000)].to_vec());
        let body = object([
            ("msgtype", JsValue::from("markdown")),
            (
                "markdown",
                object([
                    ("title", JsValue::from("Reply")),
                    ("text", JsValue::String(text)),
                ]),
            ),
        ]);
        match self.http(&lossy(&webhook), body).await {
            Ok((status, _)) if (200..300).contains(&status) => true,
            Ok((status, body)) => {
                self.log(
                    "error",
                    &format!(
                        "dingtalk: sendReply HTTP {status}: {}",
                        String::from_utf8_lossy(&body)
                            .chars()
                            .take(200)
                            .collect::<String>()
                    ),
                );
                false
            }
            Err(e) => {
                self.log("error", &format!("dingtalk: sendReply failed: {e}"));
                false
            }
        }
    }
}
struct Handle(Arc<Shared>);
impl Shared {
    async fn cleanup_all(self: Arc<Self>, reason: String) -> Result<(), ChannelError> {
        self.log("info", &format!("dingtalk: stopping ({reason})"));
        let receiver = self.receiver.lock().expect("dingtalk receiver").take();
        if let Some(task) = receiver
            && let Err(e) = task.await
        {
            self.fatal_cleanup
                .store(true, std::sync::atomic::Ordering::Release);
            self.errors
                .lock()
                .unwrap()
                .push(ChannelError::error(e.to_string()));
        }
        let handlers = std::mem::take(&mut *self.handlers.lock().expect("dingtalk handlers"));
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
        let mut streams =
            std::mem::take(&mut *self.failed_streams.lock().expect("dingtalk streams"));
        while let Some(mut stream) = streams.pop() {
            if let Err(error) = stream
                .cancel_and_drain("dingtalk channel stopped".into())
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
        let errors = std::mem::take(&mut *self.errors.lock().expect("dingtalk errors"));
        if !errors.is_empty() {
            return Err(ChannelError::Aggregate {
                message: "dingtalk cleanup failed".into(),
                errors,
            });
        }
        let mut state = self.state.lock().expect("dingtalk state");
        state.webhooks.clear();
        state.webhook_order.clear();
        state.seen.clear();
        state.seen_order.clear();
        Ok(())
    }
    fn begin_stop(
        self: &Arc<Self>,
        reason: String,
    ) -> watch::Receiver<Option<Result<(), ChannelError>>> {
        let admission = self.admission.lock().expect("channel admission");
        self.stopping.send_replace(true);
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
impl ChannelAdapter for DingTalkChannel {
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
                let mut state = shared.state.lock().expect("dingtalk state");
                state.gateway = deps.turn_gateway;
                state.logger = deps.log;
            }
            if shared.client_id.is_empty() || shared.client_secret.is_empty() {
                shared.log("error", "dingtalk: clientId and clientSecret are required");
                return Ok(Arc::new(NoopHandle) as Arc<dyn ChannelHandle>);
            }
            shared.stopping.send_replace(false);
            match shared.connect().await {
                Ok(socket) => {
                    let cloned = shared.clone();
                    let task = tokio::spawn(async move {
                        cloned.receive(socket).await;
                    });
                    *shared.receiver.lock().unwrap() = Some(task);
                    shared.log("info", "dingtalk: connected via Stream mode");
                    Ok(Arc::new(Handle(shared)) as Arc<dyn ChannelHandle>)
                }
                Err(e) => {
                    shared.log("error", &format!("dingtalk: start failed: {e}"));
                    Ok(Arc::new(NoopHandle) as Arc<dyn ChannelHandle>)
                }
            }
        })
    }
    fn deliver_cron_result(&self, delivery: &CronResultDelivery) -> BoxFuture<bool> {
        let shared = self.0.clone();
        deliver_chat_cron_result(delivery, &self.0.key, move |chat, text| {
            let (sender, result) = tokio::sync::oneshot::channel();
            let owner = shared.clone();
            shared.spawn_handler(async move {
                let delivered = owner.send_reply(&chat, &text).await;
                let _ = sender.send(delivered);
            });
            Box::pin(async move { result.await.unwrap_or(false) })
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
    mounts.register(
        CHANNEL_KEY,
        Arc::new(move |options: &JsObject| {
            let option = |key| {
                options
                    .get(key)
                    .filter(|v| !sms::nullish(v))
                    .map(sms::js_string_of)
            };
            Ok(Arc::new(DingTalkChannel::with_network(
                DingTalkChannelOptions {
                    client_id: option("clientId"),
                    client_secret: option("clientSecret"),
                    ..Default::default()
                },
                js_helper_interaction(responder.clone()),
                network.clone(),
            )) as Arc<dyn ChannelAdapter>)
        }),
    );
}
