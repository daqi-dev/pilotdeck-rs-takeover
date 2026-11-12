//! Email IMAP/SMTP adapter, fixed TS `cc7c4e6` EmailChannel.ts.
//! Native mail protocols. The production loader supplies a fresh JS-preserving
//! interaction helper per adapter; inbound turns share the host TurnGateway.
mod body;
mod mapper;
#[cfg(test)]
mod tests;
mod wire;
pub use body::extract_plain_text;

pub use mapper::{EmailSessionMapper, EmailSessionMapperState, MappedSession};

use crate::channel::{
    ActiveChat,
    im_cron::{CronResultDelivery, deliver_chat_cron_result},
    im_interaction::{JsInteractionPort, ResponderRef, js_helper_interaction},
    protocol::*,
    sms,
};
use futures_util::FutureExt;
use pilotdeck_protocol::{JsObject, JsString, JsValue};
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

pub const CHANNEL_KEY: &str = "email";
pub type Env = Arc<dyn Fn(&str) -> Option<JsString> + Send + Sync>;

#[derive(Default)]
pub struct EmailChannelOptions {
    pub extra: JsObject,
    pub mapper: Option<EmailSessionMapper>,
    pub env: Option<Env>,
}
#[derive(Clone)]
struct Config {
    address: String,
    password: String,
    imap_host: String,
    smtp_host: String,
    imap_port: u16,
    smtp_port: u16,
    imap_tls: bool,
    smtp_tls: bool,
    subject: String,
    poll_interval: Duration,
}
struct ReceiveTask {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
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
                result: Err(ChannelError::error("email: cleanup worker terminated before draining; cleanup ownership is unresolved")),
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
    mapper: EmailSessionMapper,
    recipients: HashMap<JsString, JsString>,
    gateway: Option<Arc<dyn TurnGateway>>,
    logger: Option<ChannelLog>,
}
struct Shared {
    key: ChannelKey,
    config: Config,
    seen: Mutex<HashSet<u64>>,
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
pub struct EmailChannel(Arc<Shared>);

impl EmailChannel {
    pub fn new(options: EmailChannelOptions, interaction: Arc<dyn JsInteractionPort>) -> Self {
        let env = options
            .env
            .unwrap_or_else(|| Arc::new(|key| std::env::var(key).ok().map(JsString::from)));
        let extra = &options.extra;
        let setting = |key: &str, environment: &str, default: &str| {
            extra
                .get(key)
                .filter(|v| !sms::nullish(v))
                .map(sms::js_string_of)
                .or_else(|| {
                    if environment.is_empty() {
                        None
                    } else {
                        env(environment)
                    }
                })
                .map(|s| String::from_utf16_lossy(s.units()))
                .unwrap_or_else(|| default.to_owned())
        };
        let numeric = |key: &str, environment: &str, default: f64| {
            extra
                .get(key)
                .filter(|v| !sms::nullish(v))
                .cloned()
                .or_else(|| {
                    if environment.is_empty() {
                        None
                    } else {
                        env(environment).map(JsValue::String)
                    }
                })
                .map(|v| crate::js_value::to_number(&v).unwrap_or(f64::NAN))
                .unwrap_or(default)
        };
        let number = |key: &str, environment: &str, default: u16| {
            let n = numeric(key, environment, f64::from(default));
            if n.is_finite() && n.fract() == 0.0 && (1.0..=65535.0).contains(&n) {
                n as u16
            } else {
                0
            }
        };
        let interval = numeric("pollIntervalMs", "", 45000.0);
        let interval = if interval.is_finite() && (1.0..=2147483647.0).contains(&interval) {
            interval as u64
        } else {
            1
        };
        let config = Config {
            address: setting("address", "EMAIL_ADDRESS", ""),
            password: setting("password", "EMAIL_PASSWORD", ""),
            imap_host: setting("imapHost", "IMAP_HOST", ""),
            smtp_host: setting("smtpHost", "SMTP_HOST", ""),
            imap_port: number("imapPort", "IMAP_PORT", 993),
            smtp_port: number("smtpPort", "SMTP_PORT", 587),
            imap_tls: !matches!(extra.get("imapTls"), Some(JsValue::Bool(false))),
            smtp_tls: !matches!(extra.get("smtpTls"), Some(JsValue::Bool(false))),
            subject: setting("defaultSubject", "", "Message"),
            poll_interval: Duration::from_millis(interval),
        };
        Self(Arc::new(Shared {
            key: ChannelKey::new(CHANNEL_KEY),
            config,
            seen: Mutex::new(HashSet::new()),
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
        let logger = self.state.lock().expect("email state").logger.clone();
        if let Some(log) = logger {
            log(level, text);
        }
    }
    fn gateway(&self) -> Option<Arc<dyn TurnGateway>> {
        self.state.lock().expect("email state").gateway.clone()
    }
    async fn stopped(&self) {
        let mut stop = self.stopping.subscribe();
        if *stop.borrow() {
            return;
        }
        let _ = stop.changed().await;
    }

    async fn poll_once(self: &Arc<Self>, imap: &mut wire::Imap) {
        let result = imap.poll().await;
        match result {
            Err(error) => self.log("error", &format!("email: poll error: {error}")),
            Ok(messages) => {
                let mut continuations = Vec::new();
                for message in messages {
                    if !self
                        .seen
                        .lock()
                        .expect("email seen UIDs")
                        .insert(message.uid)
                    {
                        continue;
                    }
                    let text = body::extract_plain_text(&message.source);
                    if sms::trim_units(text.units()).is_empty() {
                        continue;
                    }
                    let shared = self.clone();
                    let mut future = Box::pin(async move {
                        shared.incoming(JsString::from(message.from), text).await;
                    });
                    if future.as_mut().now_or_never().is_none() {
                        continuations.push(future);
                    }
                }
                let mut handlers = self.handlers.lock().expect("email handlers");
                self.harvest_finished(&mut handlers, "handler");
                for future in continuations {
                    handlers.push(tokio::spawn(future));
                }
            }
        }
    }
    async fn receive(self: Arc<Self>, mut imap: wire::Imap, mut stop: watch::Receiver<bool>) {
        loop {
            if *stop.borrow() || !imap.usable() {
                break;
            }
            tokio::select! { biased; _ = stop.changed() => break, _ = tokio::time::sleep(self.config.poll_interval) => {} }
            tokio::select! { biased; _ = stop.changed() => break, _ = self.poll_once(&mut imap) => {} }
        }
        if imap.usable() {
            let _ = imap.logout().await;
        }
    }

    async fn incoming(self: Arc<Self>, chat: JsString, text: JsString) {
        if *self.stopping.borrow() || chat.is_empty() || chat.equals_str("unknown") {
            return;
        }
        self.state
            .lock()
            .expect("email state")
            .recipients
            .insert(chat.clone(), chat.clone());
        if self.interaction.elicitation_pending(&chat) && self.gateway().is_some() {
            let answer = tokio::select! { biased; _ = self.stopped() => return, answer = self.interaction.elicitation_answer(&chat, &text) => answer };
            match answer {
                Ok(Some(text)) if !text.is_empty() => {
                    self.send_reply(&chat, &text).await;
                }
                Ok(_) => {}
                Err(e) => self.log(
                    "error",
                    &format!("email: elicitation answer error: {}", e.to_js_string()),
                ),
            }
            return;
        }
        if self.interaction.permission_pending(&chat) && self.gateway().is_some() {
            let answer = tokio::select! { biased; _ = self.stopped() => return, answer = self.interaction.permission_answer(&chat, &text) => answer };
            match answer {
                Err(e) => self.log(
                    "error",
                    &format!("email: permission answer error: {}", e.to_js_string()),
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
            let mut state = self.state.lock().expect("email state");
            let mut active = self.active.lock().expect("email active chats");
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
                    "email: chat {} already active, skipping",
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
                        if let Err(error) = stream.cancel_and_drain(JsString::from("email channel stopped")).await {
                            self.cleanup_errors.lock().expect("email cleanup errors").push(error.clone());
                            self.failed_streams.lock().expect("email failed streams").push(stream);
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
                &format!("email: submitTurn error: {}", error.to_js_string()),
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
                        .expect("email cleanup errors")
                        .push(ChannelError::error(format!(
                            "email: {role} cleanup failed: {error}"
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
            let mut outbound = self.outbound.lock().expect("email outbound");
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
        let recipient = String::from_utf16_lossy(recipient.units());
        let text = String::from_utf16_lossy(text.units());
        let result = tokio::select! {biased; _=self.stopped()=>return false, result=wire::send(&self.config,&recipient,&text)=>result};
        match result {
            Ok(()) => true,
            Err(error) => {
                self.log("error", &format!("email: sendMail failed: {error}"));
                false
            }
        }
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
            std::mem::take(&mut *self.failed_streams.lock().expect("email failed streams"))
        } else {
            Vec::new()
        };
        let receiver = self.receiver.lock().expect("email receiver").take();
        if let Some(receiver) = receiver {
            if let Err(error) = receiver.task.await {
                self.fatal_cleanup.store(true, Ordering::Release);
                self.cleanup_errors
                    .lock()
                    .expect("email cleanup errors")
                    .push(ChannelError::error(format!(
                        "email: receiver cleanup failed: {error}"
                    )));
            }
        }
        let handlers = std::mem::take(&mut *self.handlers.lock().expect("email handlers"));
        for handler in handlers {
            if let Err(error) = handler.await {
                self.fatal_cleanup.store(true, Ordering::Release);
                self.log("error", &format!("email: handler cleanup failed: {error}"));
                self.cleanup_errors
                    .lock()
                    .expect("email cleanup errors")
                    .push(ChannelError::error(format!(
                        "email: handler cleanup failed: {error}"
                    )));
            }
        }
        let outbound = std::mem::take(&mut self.outbound.lock().expect("email outbound").tasks);
        for task in outbound {
            if let Err(error) = task.await {
                self.fatal_cleanup.store(true, Ordering::Release);
                self.cleanup_errors
                    .lock()
                    .expect("email cleanup errors")
                    .push(ChannelError::error(format!(
                        "email: outbound cleanup failed: {error}"
                    )));
            }
        }
        for mut stream in retry_streams {
            if let Err(error) = stream
                .cancel_and_drain(JsString::from("email channel stopped"))
                .await
            {
                self.cleanup_errors
                    .lock()
                    .expect("email cleanup errors")
                    .push(error);
                self.failed_streams
                    .lock()
                    .expect("email failed streams")
                    .push(stream);
            }
        }
        let errors =
            std::mem::take(&mut *self.cleanup_errors.lock().expect("email cleanup errors"));
        if !errors.is_empty() {
            return Err(ChannelError::Aggregate {
                message: "email: owned turn cleanup failed".into(),
                errors,
            });
        }
        let chats: Vec<_> = self
            .state
            .lock()
            .expect("email state")
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
        let mut attempt = self.cleanup.lock().expect("email cleanup attempt");
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
                    .expect("email failed streams")
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
        self.0.outbound.lock().expect("email outbound").accepting = false;
        self.0.running.store(false, Ordering::Release);
        self.0.stopping.send_replace(true);
        if let Some(receiver) = self.0.receiver.lock().expect("email receiver").as_ref() {
            let _ = receiver.stop.send(true);
        }
        self.0.log("info", &format!("email: stopping ({reason})"));
        let mut receipt = self.0.cleanup_receipt();
        Box::pin(async move {
            loop {
                if let Some(outcome) = receipt.borrow().clone() {
                    return outcome.result;
                }
                receipt.changed().await.map_err(|_| {
                    ChannelError::error("email: cleanup worker exited without a result")
                })?;
            }
        })
    }
}

impl ChannelAdapter for EmailChannel {
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
                let mut cleanup = shared.cleanup.lock().expect("email cleanup attempt");
                if let Some(attempt) = cleanup.as_ref() {
                    if !attempt
                        .result
                        .borrow()
                        .as_ref()
                        .is_some_and(|outcome| outcome.result.is_ok())
                    {
                        return Err(ChannelError::error("email: prior stop has not drained"));
                    }
                    *cleanup = None;
                }
            }
            {
                let mut state = shared.state.lock().expect("email state");
                state.gateway = deps.turn_gateway;
                state.logger = deps.log;
            }
            let config = &shared.config;
            if config.address.is_empty()
                || config.password.is_empty()
                || config.imap_host.is_empty()
                || config.smtp_host.is_empty()
            {
                shared.log("error", "email: missing config; need extra.address, password, imapHost, smtpHost (or env equivalents)");
                return Ok(Arc::new(NoopHandle) as Arc<dyn ChannelHandle>);
            }
            let connected = async {
                let mut imap = wire::Imap::connect(config).await?;
                match imap.prime().await {
                    Ok(uids) => shared.seen.lock().expect("email seen UIDs").extend(uids),
                    Err(error) => {
                        shared.log("warn", &format!("email: UID priming skipped: {error}"))
                    }
                }
                if let Err(error) = wire::verify(config).await {
                    let _ = imap.logout().await;
                    return Err(error);
                }
                Ok::<_, std::io::Error>(imap)
            }
            .await;
            let mut imap = match connected {
                Ok(imap) => imap,
                Err(error) => {
                    shared.log("error", &format!("email: connect failed: {error}"));
                    return Ok(Arc::new(NoopHandle) as Arc<dyn ChannelHandle>);
                }
            };
            let (stop, rx) = watch::channel(false);
            shared.stopping.send_replace(false);
            shared.running.store(true, Ordering::Release);
            shared.outbound.lock().expect("email outbound").accepting = true;
            shared.poll_once(&mut imap).await;
            let task = tokio::spawn(shared.clone().receive(imap, rx));
            *shared.receiver.lock().expect("email receiver") = Some(ReceiveTask { stop, task });
            shared.log(
                "info",
                &format!(
                    "email: IMAP+SMTP connected ({} / {})",
                    config.imap_host, config.smtp_host
                ),
            );
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
    mounts.register(
        CHANNEL_KEY,
        Arc::new(move |options: &JsObject| {
            let extra = match options.get("extra") {
                Some(JsValue::Object(extra)) => extra.clone(),
                _ => JsObject::new(),
            };
            Ok(Arc::new(EmailChannel::new(
                EmailChannelOptions {
                    extra,
                    ..Default::default()
                },
                js_helper_interaction(responder.clone()),
            )) as Arc<dyn ChannelAdapter>)
        }),
    );
}

pub(crate) fn register_with_network(
    mounts: &mut crate::channel::loader::ChannelLoaderMounts,
    responder: ResponderRef,
    _network: Arc<crate::channel::network::ChannelNetwork>,
) {
    // IMAP/SMTP are direct TCP protocols; the baseline HTTP dispatcher does
    // not proxy these npm clients either. Each configuration owns new sockets.
    register(mounts, responder);
}
