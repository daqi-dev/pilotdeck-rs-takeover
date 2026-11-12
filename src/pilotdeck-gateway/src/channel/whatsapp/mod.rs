//! Native cc7c4e6 WhatsApp adapter. The configured bridge is a user-owned
//! external Node tool; all PilotDeck channel and turn logic runs in Rust.
mod bridge;
mod lifecycle;
mod mapper;
mod network;
#[cfg(all(test, unix))]
mod process_tests;
mod render;
#[cfg(test)]
mod tests;
mod wire;

pub use mapper::{MappedSession, WhatsAppSessionMapper, WhatsAppSessionMapperState};

use crate::channel::{
    ActiveChat,
    im_cron::{CronResultDelivery, deliver_chat_cron_result},
    im_interaction::{JsInteractionPort, ResponderRef, js_helper_interaction},
    protocol::*,
    sms,
};
use crate::runtime::model::http::ModelHttpTransport;
use futures_util::FutureExt;
use pilotdeck_protocol::{JsObject, JsString, JsValue, stringify_json};
use std::{
    collections::HashSet,
    ffi::OsString,
    path::PathBuf,
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

use lifecycle::CleanupAttempt;

pub const CHANNEL_KEY: &str = "whatsapp";
pub type Env = Arc<dyn Fn(&str) -> Option<JsString> + Send + Sync>;

#[derive(Default)]
pub struct WhatsAppChannelOptions {
    pub bridge_path: Option<JsString>,
    pub bridge_url: Option<JsString>,
    pub mapper: Option<WhatsAppSessionMapper>,
    pub env: Option<Env>,
    pub transport: Option<Arc<dyn ModelHttpTransport>>,
    /// Embedding/test seam, not an extra external configuration field.
    /// Production resolves the optional Node tool through the owner's PATH.
    pub node_executable: Option<PathBuf>,
    pub bridge_env: Vec<(OsString, OsString)>,
}

#[derive(Default)]
struct OutboundState {
    accepting: bool,
    tasks: Vec<JoinHandle<()>>,
}

struct State {
    mapper: WhatsAppSessionMapper,
    seen_ids: HashSet<JsString>,
    known_chats: HashSet<JsString>,
    gateway: Option<Arc<dyn TurnGateway>>,
    logger: Option<ChannelLog>,
}

struct Shared {
    key: ChannelKey,
    bridge_path: String,
    bridge_url: String,
    node_executable: PathBuf,
    bridge_env: Vec<(OsString, OsString)>,
    transport: Arc<dyn ModelHttpTransport>,
    interaction: Arc<dyn JsInteractionPort>,
    state: Mutex<State>,
    active: Arc<Mutex<HashSet<JsString>>>,
    running: AtomicBool,
    stopping: watch::Sender<bool>,
    bridge: Mutex<Option<bridge::BridgeTask>>,
    poller: Mutex<Option<JoinHandle<()>>>,
    polls: Mutex<Vec<JoinHandle<()>>>,
    handlers: Mutex<Vec<JoinHandle<()>>>,
    outbound: Mutex<OutboundState>,
    cleanup_errors: Mutex<Vec<ChannelError>>,
    failed_streams: Mutex<Vec<Box<dyn TurnEventStream>>>,
    cleanup: Mutex<Option<CleanupAttempt>>,
    fatal_cleanup: AtomicBool,
}

#[derive(Clone)]
pub struct WhatsAppChannel(Arc<Shared>);

impl WhatsAppChannel {
    pub fn new(options: WhatsAppChannelOptions, interaction: Arc<dyn JsInteractionPort>) -> Self {
        let env = options
            .env
            .unwrap_or_else(|| Arc::new(|key| std::env::var(key).ok().map(JsString::from)));
        let path = options
            .bridge_path
            .or_else(|| env("WHATSAPP_BRIDGE_PATH"))
            .unwrap_or_default();
        let url = options
            .bridge_url
            .or_else(|| env("WHATSAPP_BRIDGE_URL"))
            .unwrap_or_else(|| "http://127.0.0.1:3100".into());
        Self(Arc::new(Shared {
            key: ChannelKey::new(CHANNEL_KEY),
            bridge_path: String::from_utf16_lossy(sms::trim_units(path.units())),
            bridge_url: String::from_utf16_lossy(url.units())
                .trim_end_matches('/')
                .to_owned(),
            node_executable: options
                .node_executable
                .unwrap_or_else(|| PathBuf::from("node")),
            bridge_env: options.bridge_env,
            transport: options
                .transport
                .unwrap_or_else(|| Arc::new(crate::channel::network::ChannelNetwork::default())),
            interaction,
            state: Mutex::new(State {
                mapper: options.mapper.unwrap_or_default(),
                seen_ids: HashSet::new(),
                known_chats: HashSet::new(),
                gateway: None,
                logger: None,
            }),
            active: Arc::default(),
            running: AtomicBool::new(false),
            stopping: watch::channel(false).0,
            bridge: Mutex::new(None),
            poller: Mutex::new(None),
            polls: Mutex::new(Vec::new()),
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
        let logger = self.state.lock().expect("whatsapp state").logger.clone();
        if let Some(log) = logger {
            log(level, text);
        }
    }

    fn gateway(&self) -> Option<Arc<dyn TurnGateway>> {
        self.state.lock().expect("whatsapp state").gateway.clone()
    }

    async fn stopped(&self) {
        let mut stop = self.stopping.subscribe();
        if *stop.borrow() {
            return;
        }
        let _ = stop.changed().await;
    }

    async fn dispatch(self: Arc<Self>, message: wire::InboundMessage) {
        if *self.stopping.borrow() || message.text.is_empty() {
            return;
        }
        let chat = message.chat;
        self.state
            .lock()
            .expect("whatsapp state")
            .known_chats
            .insert(chat.clone());
        if self.interaction.elicitation_pending(&chat) && self.gateway().is_some() {
            let answer = tokio::select! { biased; _ = self.stopped() => return, answer = self.interaction.elicitation_answer(&chat, &message.text) => answer };
            match answer {
                Ok(Some(text)) if !text.is_empty() => {
                    self.send_reply(&chat, &text).await;
                }
                Ok(_) => {}
                Err(error) => self.log(
                    "error",
                    &format!(
                        "whatsapp: elicitation answer error: {}",
                        error.to_js_string()
                    ),
                ),
            }
            return;
        }
        if self.interaction.permission_pending(&chat) && self.gateway().is_some() {
            let answer = tokio::select! { biased; _ = self.stopped() => return, answer = self.interaction.permission_answer(&chat, &message.text) => answer };
            match answer {
                Err(error) => self.log(
                    "error",
                    &format!(
                        "whatsapp: permission answer error: {}",
                        error.to_js_string()
                    ),
                ),
                Ok(None) => {}
                Ok(Some(answer)) => {
                    if let Some(text) = answer.text.filter(|text| !text.is_empty()) {
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
                            .filter(|prompt| !prompt.is_empty())
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
            let mut state = self.state.lock().expect("whatsapp state");
            let mut active = self.active.lock().expect("whatsapp active chats");
            if active.contains(&chat) {
                Admission::Busy
            } else {
                let mapped = state.mapper.resolve(&chat, &message.text);
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
                    "whatsapp: chat {} already active, skipping",
                    String::from_utf16_lossy(chat.units())
                ),
            ),
            Admission::New => {
                self.send_reply(&chat, &"已创建新会话。".into()).await;
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
            tokio::task::yield_now().await;
            loop {
                let event = tokio::select! {
                    biased;
                    _ = self.stopped() => {
                        if let Err(error) = stream.cancel_and_drain("whatsapp channel stopped".into()).await {
                            self.cleanup_errors.lock().expect("whatsapp cleanup errors").push(error.clone());
                            self.failed_streams.lock().expect("whatsapp failed streams").push(stream);
                            return Err(error);
                        }
                        break;
                    }
                    event = stream.next() => event?,
                };
                let Some(event) = event else { break; };
                let kind = event.get("type").and_then(JsValue::as_string);
                if kind.is_some_and(|kind| kind.equals_str("elicitation_request")) {
                    let prompt = self.interaction.elicitation_capture(chat, &mapped.session_key, &event);
                    self.send_reply(chat, &prompt).await;
                } else if kind.is_some_and(|kind| kind.equals_str("permission_request")) {
                    if let Some(prompt) = self.interaction.permission_capture(chat, &mapped.session_key, &event).filter(|prompt| !prompt.is_empty()) {
                        let delivered = self.send_reply(chat, &prompt).await;
                        self.interaction.permission_confirm_initial(chat, delivered, event.get("requestId").and_then(JsValue::as_string));
                    }
                } else if let Some(fragment) = render::fragment(&event) { reply.extend_from_slice(fragment.units()); }
            }
            Ok::<_, ChannelError>(())
        }.await;
        if result.is_err() && *self.stopping.borrow() {
            return;
        }
        if let Err(error) = result {
            self.log(
                "error",
                &format!("whatsapp: submitTurn error: {}", error.to_js_string()),
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
                    self.fatal_cleanup.store(true, Ordering::Release);
                    self.cleanup_errors
                        .lock()
                        .expect("whatsapp cleanup errors")
                        .push(ChannelError::error(format!(
                            "whatsapp: {role} cleanup failed: {error}"
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
            let mut outbound = self.outbound.lock().expect("whatsapp outbound");
            if !outbound.accepting || *self.stopping.borrow() {
                return false;
            }
            self.harvest_finished(&mut outbound.tasks, "outbound");
            let (finished, receipt) = oneshot::channel();
            let shared = self.clone();
            let chat = chat.clone();
            let text = text.clone();
            outbound.tasks.push(tokio::spawn(async move {
                let delivered = shared.send_reply_owned(&chat, &text).await;
                let _ = finished.send(delivered);
            }));
            receipt
        };
        receipt.await.unwrap_or(false)
    }
}

struct Handle(Arc<Shared>);
struct NoopHandle;
impl ChannelHandle for NoopHandle {
    fn stop(&self, _reason: &str) -> BoxFuture<Result<(), ChannelError>> {
        Box::pin(async { Ok(()) })
    }
}

struct Starting(Option<Arc<Shared>>);
impl Drop for Starting {
    fn drop(&mut self) {
        if let Some(shared) = self.0.take() {
            // Cancellation of a start waiter cannot discard the sole child
            // owner. The cleanup worker retains its independent receipt.
            let _receipt = shared.stop_receipt(None);
        }
    }
}

impl ChannelAdapter for WhatsAppChannel {
    fn channel_key(&self) -> &ChannelKey {
        &self.0.key
    }
    fn start(
        &self,
        deps: ChannelStartDeps,
    ) -> BoxFuture<Result<Arc<dyn ChannelHandle>, ChannelError>> {
        let shared = self.0.clone();
        Box::pin(async move {
            shared.prepare_start()?;
            {
                let mut state = shared.state.lock().expect("whatsapp state");
                state.gateway = deps.turn_gateway;
                state.logger = deps.log;
                state.seen_ids.clear();
            }
            if shared.bridge_path.is_empty() {
                shared.log(
                    "error",
                    "whatsapp: bridgePath / WHATSAPP_BRIDGE_PATH is required",
                );
                return Ok(Arc::new(NoopHandle) as Arc<dyn ChannelHandle>);
            }
            shared.stopping.send_replace(false);
            let mut starting = Starting(Some(shared.clone()));
            let port = url::Url::parse(&shared.bridge_url)
                .ok()
                .and_then(|url| url.port())
                .filter(|port| *port > 0);
            let logger = shared.state.lock().expect("whatsapp state").logger.clone();
            let bridge = match bridge::spawn(
                &shared.node_executable,
                &shared.bridge_path,
                port,
                &shared.bridge_env,
                logger,
            ) {
                Ok(bridge) => Some(bridge),
                Err(bridge::SpawnFailure::Synchronous(error)) => {
                    shared.log(
                        "error",
                        &format!("whatsapp: start failed: {}", error.to_js_string()),
                    );
                    shared.stop_receipt(None).await?;
                    starting.0 = None;
                    return Ok(Arc::new(NoopHandle) as Arc<dyn ChannelHandle>);
                }
                Err(bridge::SpawnFailure::Child(error)) => {
                    shared.log(
                        "error",
                        &format!("whatsapp: bridge spawn error: {}", error.to_js_string()),
                    );
                    None
                }
            };
            *shared.bridge.lock().expect("whatsapp bridge") = bridge;
            if !shared.wait_for_ready().await {
                shared.stop_receipt(None).await?;
                starting.0 = None;
                shared.log("error", "whatsapp: bridge HTTP did not become ready");
                return Ok(Arc::new(NoopHandle) as Arc<dyn ChannelHandle>);
            }
            shared.running.store(true, Ordering::Release);
            if let Some(bridge) = shared.bridge.lock().expect("whatsapp bridge").as_ref() {
                bridge.activate();
            }
            shared.outbound.lock().expect("whatsapp outbound").accepting = true;
            *shared.poller.lock().expect("whatsapp poller") =
                Some(tokio::spawn(shared.clone().poll_loop()));
            shared.log(
                "info",
                &format!(
                    "whatsapp: connected, polling {}/messages",
                    shared.bridge_url
                ),
            );
            starting.0 = None;
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
    mounts.register(CHANNEL_KEY, Arc::new(move |options: &JsObject| {
        let path = options.get("bridgePath").filter(|value| !sms::nullish(value));
        if path.is_some_and(|value| !matches!(value, JsValue::String(_))) { return Err(ChannelError::Plain("TypeError: (options.bridgePath ?? process.env.WHATSAPP_BRIDGE_PATH ?? \"\").trim is not a function".into())); }
        let url = options.get("bridgeUrl").filter(|value| !sms::nullish(value));
        if url.is_some_and(|value| !matches!(value, JsValue::String(_))) { return Err(ChannelError::Plain("TypeError: url.replace is not a function".into())); }
        Ok(Arc::new(WhatsAppChannel::new(WhatsAppChannelOptions { bridge_path: path.and_then(JsValue::as_string).cloned(), bridge_url: url.and_then(JsValue::as_string).cloned(), transport: transport.clone(), ..Default::default() }, js_helper_interaction(responder.clone()))) as Arc<dyn ChannelAdapter>)
    }));
}
