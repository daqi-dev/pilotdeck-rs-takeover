//! Signal HTTP bridge adapter, fixed TS `cc7c4e6` SignalChannel.ts.
//! Native HTTP only. The production loader supplies a fresh JS-preserving
//! interaction helper per adapter; inbound turns share the host TurnGateway.
mod mapper;
#[cfg(test)]
mod tests;
mod wire;

pub use mapper::{MappedSession, SignalSessionMapper, SignalSessionMapperState};

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

pub const CHANNEL_KEY: &str = "signal";
pub type Env = Arc<dyn Fn(&str) -> Option<JsString> + Send + Sync>;

#[derive(Default)]
pub struct SignalChannelOptions {
    pub rest_url: Option<JsString>,
    /// The loader forwards account without coercion; JSON output retains its
    /// type, while the receive URL uses encodeURIComponent's string coercion.
    pub account: Option<JsValue>,
    pub mapper: Option<SignalSessionMapper>,
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
                result: Err(ChannelError::error("signal: cleanup worker terminated before draining; cleanup ownership is unresolved")),
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
    mapper: SignalSessionMapper,
    recipients: HashMap<JsString, JsString>,
    gateway: Option<Arc<dyn TurnGateway>>,
    logger: Option<ChannelLog>,
}
struct Shared {
    key: ChannelKey,
    rest_url: String,
    account: JsValue,
    account_text: JsString,
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
pub struct SignalChannel(Arc<Shared>);

impl SignalChannel {
    pub fn new(options: SignalChannelOptions, interaction: Arc<dyn JsInteractionPort>) -> Self {
        let env = options
            .env
            .unwrap_or_else(|| Arc::new(|key| std::env::var(key).ok().map(JsString::from)));
        let rest_url = options
            .rest_url
            .or_else(|| env("SIGNAL_HTTP_URL"))
            .unwrap_or_else(|| JsString::from("http://127.0.0.1:8080"));
        let account = options
            .account
            .filter(|v| !sms::nullish(v))
            .or_else(|| env("SIGNAL_ACCOUNT").map(JsValue::String))
            .unwrap_or_else(|| JsValue::from(""));
        Self(Arc::new(Shared {
            key: ChannelKey::new(CHANNEL_KEY),
            rest_url: String::from_utf16_lossy(rest_url.units())
                .trim_end_matches('/')
                .to_owned(),
            account_text: sms::js_string_of(&account),
            account,
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
        let logger = self.state.lock().expect("signal state").logger.clone();
        if let Some(log) = logger {
            log(level, text);
        }
    }
    fn gateway(&self) -> Option<Arc<dyn TurnGateway>> {
        self.state.lock().expect("signal state").gateway.clone()
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

    async fn receive_once(self: &Arc<Self>, url: &str, carry: &mut String) -> Result<bool, String> {
        let response = self.request(Method::GET, url, None).await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = Self::response_text(response.into_body())
                .await
                .unwrap_or_default();
            self.log("error", &format!("signal: receive HTTP {status}: {text}"));
            return Ok(false);
        }
        // Fetch gives 204/205 a null body, unlike an empty 200 body. The
        // baseline takes its three-second backoff only for the null body.
        if matches!(response.status().as_u16(), 204 | 205) {
            return Ok(false);
        }
        let mut body = response.into_body();
        let mut decoder = Utf8Stream::default();
        loop {
            let frame = tokio::time::timeout(UNDICI_BODY_TIMEOUT, body.frame())
                .await
                .map_err(|_| "TypeError: terminated".to_owned())?;
            let Some(frame) = frame else {
                return Ok(true);
            };
            let frame = frame.map_err(|_| "TypeError: terminated".to_owned())?;
            if let Some(bytes) = frame.data_ref() {
                let mut continuations = Vec::new();
                for line in wire::lines(&mut decoder, carry, bytes) {
                    let shared = self.clone();
                    let mut future = Box::pin(async move {
                        shared.parse_line(&line).await;
                    });
                    // JS starts each async handler synchronously, through its
                    // first await, before reading the next line. Preserve the
                    // busy/new admission order on multithreaded runtimes too.
                    if future.as_mut().now_or_never().is_none() {
                        continuations.push(future);
                    }
                }
                // A TS reader result dispatches every line before running any
                // handler continuation. Defer spawning until the whole batch
                // has taken admission, including on a multithreaded runtime.
                let mut handlers = self.handlers.lock().expect("signal handlers");
                self.harvest_finished(&mut handlers, "handler");
                for future in continuations {
                    handlers.push(tokio::spawn(future));
                }
            }
        }
    }

    async fn receive(self: Arc<Self>, url: String, mut stop: watch::Receiver<bool>) {
        let mut carry = String::new();
        loop {
            if *stop.borrow() {
                return;
            }
            let result = tokio::select! { biased; _ = stop.changed() => return, result = self.receive_once(&url, &mut carry) => result };
            match result {
                Ok(true) => continue,
                Ok(false) => {}
                Err(error) => self.log("error", &format!("signal: receive stream error: {error}")),
            }
            tokio::select! { biased; _ = stop.changed() => return, _ = tokio::time::sleep(Duration::from_secs(3)) => {} }
        }
    }

    async fn parse_line(self: Arc<Self>, line: &str) {
        if *self.stopping.borrow() {
            return;
        }
        let trimmed = JsString::from(line);
        let trimmed = JsString::from_utf16(sms::trim_units(trimmed.units()).to_vec());
        let payload = String::from_utf16_lossy(trimmed.units());
        let payload = JsString::from(payload.strip_prefix("data:").unwrap_or(payload.as_ref()));
        let payload = String::from_utf16_lossy(sms::trim_units(payload.units()));
        if payload.is_empty() || payload == "[DONE]" || payload == ":ok" {
            return;
        }
        let Ok(raw) = parse_json(&payload) else {
            return;
        };
        if matches!(raw, JsValue::Null) {
            self.log("error", "signal: parseLine error: TypeError: Cannot read properties of null (reading 'envelope')");
            return;
        }
        let Some(envelope) = wire::extract(&raw) else {
            return;
        };
        if sms::trim_units(envelope.text.units()).is_empty() {
            return;
        }
        let chat = envelope.chat.unwrap_or_else(|| self.account_text.clone());
        let recipient = envelope
            .source
            .unwrap_or_else(|| wire::strip_chat_prefix(&chat));
        if !recipient.is_empty() {
            self.state
                .lock()
                .expect("signal state")
                .recipients
                .insert(chat.clone(), recipient);
        }

        if self.interaction.elicitation_pending(&chat) && self.gateway().is_some() {
            let answer = tokio::select! { biased; _ = self.stopped() => return, answer = self.interaction.elicitation_answer(&chat, &envelope.text) => answer };
            match answer {
                Ok(Some(text)) if !text.is_empty() => {
                    self.send_reply(&chat, &text).await;
                }
                Ok(_) => {}
                Err(e) => self.log(
                    "error",
                    &format!("signal: elicitation answer error: {}", e.to_js_string()),
                ),
            }
            return;
        }
        if self.interaction.permission_pending(&chat) && self.gateway().is_some() {
            let answer = tokio::select! { biased; _ = self.stopped() => return, answer = self.interaction.permission_answer(&chat, &envelope.text) => answer };
            match answer {
                Err(e) => self.log(
                    "error",
                    &format!("signal: permission answer error: {}", e.to_js_string()),
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
            let mut state = self.state.lock().expect("signal state");
            let mut active = self.active.lock().expect("signal active chats");
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
                    "signal: chat {} already active, skipping",
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
                        if let Err(error) = stream.cancel_and_drain(JsString::from("signal channel stopped")).await {
                            self.cleanup_errors.lock().expect("signal cleanup errors").push(error.clone());
                            self.failed_streams.lock().expect("signal failed streams").push(stream);
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
                &format!("signal: submitTurn error: {}", error.to_js_string()),
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
                        .expect("signal cleanup errors")
                        .push(ChannelError::error(format!(
                            "signal: {role} cleanup failed: {error}"
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
            let mut outbound = self.outbound.lock().expect("signal outbound");
            if !outbound.accepting || *self.stopping.borrow() {
                return false;
            }
            self.harvest_finished(&mut outbound.tasks, "outbound");
            let recipient = self
                .state
                .lock()
                .expect("signal state")
                .recipients
                .get(chat)
                .cloned()
                .unwrap_or_else(|| wire::strip_chat_prefix(chat));
            if recipient.is_empty() {
                drop(outbound);
                self.log(
                    "warn",
                    &format!(
                        "signal: no recipient for {}, cannot send",
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
            let mut body = JsObject::new();
            body.insert("message".into(), JsValue::String(chunk));
            body.insert("number".into(), self.account.clone());
            body.insert(
                "recipients".into(),
                JsValue::Array(vec![JsValue::String(recipient.clone())]),
            );
            let body = stringify_json(&JsValue::Object(body))
                .expect("finite Signal body")
                .expect("object JSON");
            let url = format!("{}/v2/send", self.rest_url);
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
                            "signal: send HTTP {status}: {}",
                            String::from_utf16_lossy(&units)
                        ),
                    );
                    ok = false;
                }
                Err(error) => {
                    self.log("error", &format!("signal: send failed: {error}"));
                    ok = false;
                }
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
            std::mem::take(&mut *self.failed_streams.lock().expect("signal failed streams"))
        } else {
            Vec::new()
        };
        let receiver = self.receiver.lock().expect("signal receiver").take();
        if let Some(receiver) = receiver
            && let Err(error) = receiver.task.await
        {
            self.fatal_cleanup.store(true, Ordering::Release);
            self.cleanup_errors
                .lock()
                .expect("signal cleanup errors")
                .push(ChannelError::error(format!(
                    "signal: receiver cleanup failed: {error}"
                )));
        }
        let handlers = std::mem::take(&mut *self.handlers.lock().expect("signal handlers"));
        for handler in handlers {
            if let Err(error) = handler.await {
                self.fatal_cleanup.store(true, Ordering::Release);
                self.log("error", &format!("signal: handler cleanup failed: {error}"));
                self.cleanup_errors
                    .lock()
                    .expect("signal cleanup errors")
                    .push(ChannelError::error(format!(
                        "signal: handler cleanup failed: {error}"
                    )));
            }
        }
        let outbound = std::mem::take(&mut self.outbound.lock().expect("signal outbound").tasks);
        for task in outbound {
            if let Err(error) = task.await {
                self.fatal_cleanup.store(true, Ordering::Release);
                self.cleanup_errors
                    .lock()
                    .expect("signal cleanup errors")
                    .push(ChannelError::error(format!(
                        "signal: outbound cleanup failed: {error}"
                    )));
            }
        }
        for mut stream in retry_streams {
            if let Err(error) = stream
                .cancel_and_drain(JsString::from("signal channel stopped"))
                .await
            {
                self.cleanup_errors
                    .lock()
                    .expect("signal cleanup errors")
                    .push(error);
                self.failed_streams
                    .lock()
                    .expect("signal failed streams")
                    .push(stream);
            }
        }
        let errors =
            std::mem::take(&mut *self.cleanup_errors.lock().expect("signal cleanup errors"));
        if !errors.is_empty() {
            return Err(ChannelError::Aggregate {
                message: "signal: owned turn cleanup failed".into(),
                errors,
            });
        }
        let chats: Vec<_> = self
            .state
            .lock()
            .expect("signal state")
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
        let mut attempt = self.cleanup.lock().expect("signal cleanup attempt");
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
                    .expect("signal failed streams")
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
        self.0.outbound.lock().expect("signal outbound").accepting = false;
        self.0.running.store(false, Ordering::Release);
        self.0.stopping.send_replace(true);
        if let Some(receiver) = self.0.receiver.lock().expect("signal receiver").as_ref() {
            let _ = receiver.stop.send(true);
        }
        self.0.log("info", &format!("signal: stopping ({reason})"));
        let mut receipt = self.0.cleanup_receipt();
        Box::pin(async move {
            loop {
                if let Some(outcome) = receipt.borrow().clone() {
                    return outcome.result;
                }
                receipt.changed().await.map_err(|_| {
                    ChannelError::error("signal: cleanup worker exited without a result")
                })?;
            }
        })
    }
}

impl ChannelAdapter for SignalChannel {
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
                let mut cleanup = shared.cleanup.lock().expect("signal cleanup attempt");
                if let Some(attempt) = cleanup.as_ref() {
                    if !attempt
                        .result
                        .borrow()
                        .as_ref()
                        .is_some_and(|outcome| outcome.result.is_ok())
                    {
                        return Err(ChannelError::error("signal: prior stop has not drained"));
                    }
                    *cleanup = None;
                }
            }
            {
                let mut state = shared.state.lock().expect("signal state");
                state.gateway = deps.turn_gateway;
                state.logger = deps.log;
            }
            if !sms::truthy(&shared.account) {
                shared.log("error", "signal: SIGNAL_ACCOUNT / account not set");
                return Ok(Arc::new(NoopHandle) as Arc<dyn ChannelHandle>);
            }
            let account = String::from_utf16(shared.account_text.units())
                .map_err(|_| ChannelError::Plain("URIError: URI malformed".into()))?;
            let url = format!(
                "{}/v1/receive/{}",
                shared.rest_url,
                crate::channel::http_client::encode_uri_component(&account)
            );
            let (stop, rx) = watch::channel(false);
            shared.stopping.send_replace(false);
            shared.running.store(true, Ordering::Release);
            shared.outbound.lock().expect("signal outbound").accepting = true;
            let task = tokio::spawn(shared.clone().receive(url.clone(), rx));
            *shared.receiver.lock().expect("signal receiver") = Some(ReceiveTask { stop, task });
            shared.log("info", &format!("signal: SSE receive at {url}"));
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
            let rest = options.get("restUrl").filter(|v| !sms::nullish(v));
            if rest.is_some_and(|v| !matches!(v, JsValue::String(_))) {
                return Err(ChannelError::Plain(
                    "TypeError: url.replace is not a function".into(),
                ));
            }
            Ok(Arc::new(SignalChannel::new(
                SignalChannelOptions {
                    rest_url: rest.and_then(JsValue::as_string).cloned(),
                    account: options.get("account").cloned(),
                    transport: transport.clone(),
                    ..Default::default()
                },
                js_helper_interaction(responder.clone()),
            )) as Arc<dyn ChannelAdapter>)
        }),
    );
}
