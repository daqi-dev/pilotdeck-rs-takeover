//! Component checks against the independently captured TS values. Formal
//! native-server parity is owned by tests/fixtures/h1-whatsapp/capture.mjs.
use super::*;
use crate::channel::im_interaction::InteractionResponder;
use crate::channel::sms::{ElicitationResponse, PermissionDecision, PermissionDecisionOutcome};
use crate::runtime::model::http::{BodyFailure, ModelBody, TransportFuture};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::{HeaderMap, Request, Response, body::Frame};
use pilotdeck_protocol::parse_json;
use std::{collections::VecDeque, future::Future, pin::Pin, sync::atomic::AtomicUsize};
use tokio::sync::mpsc;

#[derive(Default)]
pub(super) struct Responder {
    elicitation: Mutex<Vec<JsValue>>,
    permissions: Mutex<Vec<JsValue>>,
    fail_permission: AtomicBool,
}
impl InteractionResponder for Responder {
    fn respond_elicitation(
        &self,
        response: ElicitationResponse,
    ) -> BoxFuture<Result<(), ChannelError>> {
        self.elicitation
            .lock()
            .unwrap()
            .push(JsValue::Object(response.to_js()));
        Box::pin(async { Ok(()) })
    }
    fn permission_decide(
        &self,
        decision: PermissionDecision,
    ) -> BoxFuture<Result<PermissionDecisionOutcome, ChannelError>> {
        self.permissions
            .lock()
            .unwrap()
            .push(JsValue::Object(decision.to_js()));
        let failed = self.fail_permission.swap(false, Ordering::AcqRel);
        Box::pin(async move {
            if failed {
                Err(ChannelError::error("fixture decision failed"))
            } else {
                Ok(PermissionDecisionOutcome { delivered: true })
            }
        })
    }
}

type TurnSender = mpsc::UnboundedSender<Result<Option<JsValue>, ChannelError>>;
#[derive(Default)]
struct Gateway {
    requests: Mutex<Vec<TurnRequest>>,
    streams: Mutex<Vec<TurnSender>>,
    cancelled: Arc<AtomicUsize>,
    fail_cancel: Arc<AtomicBool>,
    cancel_gate: Mutex<Option<watch::Receiver<bool>>>,
    events_read: Arc<AtomicUsize>,
}
struct Events {
    rx: mpsc::UnboundedReceiver<Result<Option<JsValue>, ChannelError>>,
    cancelled: Arc<AtomicUsize>,
    fail_cancel: Arc<AtomicBool>,
    cancel_gate: Option<watch::Receiver<bool>>,
    events_read: Arc<AtomicUsize>,
}
impl TurnEventStream for Events {
    fn next(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<JsValue>, ChannelError>> + Send + '_>> {
        Box::pin(async {
            let event = self.rx.recv().await.unwrap_or(Ok(None));
            if matches!(&event, Ok(Some(_))) {
                self.events_read.fetch_add(1, Ordering::AcqRel);
            }
            event
        })
    }
    fn cancel_and_drain(
        &mut self,
        _reason: JsString,
    ) -> Pin<Box<dyn Future<Output = Result<(), ChannelError>> + Send + '_>> {
        self.cancelled.fetch_add(1, Ordering::AcqRel);
        if self.fail_cancel.swap(false, Ordering::AcqRel) {
            return Box::pin(async { Err(ChannelError::error("fixture drain failed")) });
        }
        Box::pin(async move {
            if let Some(gate) = self.cancel_gate.as_mut() {
                while !*gate.borrow() {
                    gate.changed()
                        .await
                        .map_err(|_| ChannelError::error("fixture gate closed"))?;
                }
            }
            self.rx.close();
            Ok(())
        })
    }
}
impl TurnGateway for Gateway {
    fn submit(
        &self,
        request: TurnRequest,
    ) -> BoxFuture<Result<Box<dyn TurnEventStream>, ChannelError>> {
        self.requests.lock().unwrap().push(request);
        let (tx, rx) = mpsc::unbounded_channel();
        self.streams.lock().unwrap().push(tx);
        let cancelled = self.cancelled.clone();
        let fail_cancel = self.fail_cancel.clone();
        let cancel_gate = self.cancel_gate.lock().unwrap().clone();
        let events_read = self.events_read.clone();
        Box::pin(async move {
            Ok(Box::new(Events {
                rx,
                cancelled,
                fail_cancel,
                cancel_gate,
                events_read,
            }) as Box<dyn TurnEventStream>)
        })
    }
}
impl Gateway {
    fn event(&self, index: usize, event: &str) {
        self.streams.lock().unwrap()[index]
            .send(Ok(Some(parse_json(event).unwrap())))
            .unwrap();
    }
    fn finish(&self, index: usize) {
        self.streams.lock().unwrap()[index].send(Ok(None)).unwrap();
    }
}
async fn until(label: &str, predicate: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !predicate() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{label}"));
}

#[derive(Default)]
struct HttpMock {
    incoming: Mutex<VecDeque<String>>,
    requests: Mutex<Vec<(String, String, HeaderMap, String)>>,
    statuses: Mutex<VecDeque<u16>>,
    hang_post: AtomicBool,
    hang_get_body: AtomicBool,
    dropped: Arc<AtomicUsize>,
}
fn full(text: impl Into<Bytes>) -> ModelBody {
    Full::new(text.into())
        .map_err(|never| match never {})
        .boxed()
}
struct Dropped(Arc<AtomicUsize>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}
impl ModelHttpTransport for HttpMock {
    fn send(&self, request: Request<Full<Bytes>>) -> TransportFuture {
        let (parts, body) = request.into_parts();
        let method = parts.method.to_string();
        let uri = parts.uri.to_string();
        let headers = parts.headers;
        let body = body
            .collect()
            .now_or_never()
            .expect("full request body")
            .unwrap()
            .to_bytes();
        self.requests.lock().unwrap().push((
            method.clone(),
            uri,
            headers,
            String::from_utf8(body.to_vec()).unwrap(),
        ));
        if method == "POST" && self.hang_post.load(Ordering::Acquire) {
            let dropped = Dropped(self.dropped.clone());
            return TransportFuture::io(async move {
                let _dropped = dropped;
                std::future::pending().await
            });
        }
        let status = self.statuses.lock().unwrap().pop_front().unwrap_or(200);
        if method == "GET" && self.hang_get_body.load(Ordering::Acquire) {
            let body = StreamBody::new(futures_util::stream::pending::<
                Result<Frame<Bytes>, BodyFailure>,
            >())
            .boxed();
            return TransportFuture::ready(Ok(Response::builder()
                .status(status)
                .body(body)
                .unwrap()));
        }
        let body = if method == "GET" {
            self.incoming
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| "[]".into())
        } else if status == 200 {
            "{}".into()
        } else {
            r#"{"error":"fixture denied"}"#.into()
        };
        TransportFuture::ready(Ok(Response::builder()
            .status(status)
            .body(full(body))
            .unwrap()))
    }
}
impl HttpMock {
    fn sent(&self) -> Vec<JsValue> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, _, _, _)| method == "POST")
            .map(|(_, _, _, body)| parse_json(body).unwrap())
            .collect()
    }
}
struct Fixture {
    channel: WhatsAppChannel,
    handle: Arc<dyn ChannelHandle>,
    bridge: Arc<HttpMock>,
    gateway: Arc<Gateway>,
    responder: Arc<Responder>,
    logs: Arc<Mutex<Vec<String>>>,
}
fn fixture() -> Fixture {
    let bridge = Arc::new(HttpMock::default());
    let gateway = Arc::new(Gateway::default());
    let responder = Arc::new(Responder::default());
    let logs = Arc::new(Mutex::new(Vec::new()));
    let channel = WhatsAppChannel::new(
        WhatsAppChannelOptions {
            bridge_url: Some("http://127.0.0.1:18081///".into()),
            transport: Some(bridge.clone()),
            env: Some(Arc::new(|_| None)),
            mapper: Some(WhatsAppSessionMapper::new(
                Default::default(),
                Box::new(|| "12345678-1234-4123-8123-123456789012".into()),
            )),
            ..Default::default()
        },
        js_helper_interaction(responder.clone()),
    );
    // This component fixture enters after readiness; actual child startup and
    // formal loading are exercised by the process tests and CLI fixture.
    {
        let mut state = channel.0.state.lock().unwrap();
        state.gateway = Some(gateway.clone());
        let capture = logs.clone();
        state.logger = Some(Arc::new(move |_, line| {
            capture.lock().unwrap().push(line.into())
        }));
    }
    channel.0.running.store(true, Ordering::Release);
    channel.0.outbound.lock().unwrap().accepting = true;
    let handle = Arc::new(Handle(channel.0.clone())) as Arc<dyn ChannelHandle>;
    Fixture {
        channel,
        handle,
        bridge,
        gateway,
        responder,
        logs,
    }
}
impl Fixture {
    async fn inbound(&self, id: &str, chat: &str, text: &str) {
        self.bridge
            .incoming
            .lock()
            .unwrap()
            .push_back(serde_json::json!([{ "id":id, "chatId":chat, "text":text }]).to_string());
        self.channel.0.clone().poll_once().await;
    }
}
fn array(value: &JsValue) -> &[JsValue] {
    match value {
        JsValue::Array(values) => values,
        _ => panic!("array expected"),
    }
}
fn oracle() -> JsValue {
    parse_json(include_str!(
        "../../../tests/fixtures/h1-whatsapp/value-oracle.json"
    ))
    .unwrap()
}

#[test]
fn normalization_matches_real_ts_nullish_alias_and_utf16_cases() {
    let oracle = oracle();
    for row in array(oracle.get("normalizations").unwrap()) {
        let actual = wire::messages(row.get("input").unwrap());
        if let Some(error) = row.get("error").and_then(JsValue::as_string) {
            assert_eq!(actual.unwrap_err(), String::from_utf16_lossy(error.units()));
            continue;
        }
        let actual = actual.unwrap();
        let expected = array(row.get("output").unwrap());
        assert_eq!(actual.len(), expected.len());
        for (actual, expected) in actual.iter().zip(expected) {
            assert_eq!(&actual.id, expected.get("id").unwrap().as_string().unwrap());
            assert_eq!(
                &actual.chat,
                expected.get("chatId").unwrap().as_string().unwrap()
            );
            assert_eq!(
                &actual.text,
                expected.get("text").unwrap().as_string().unwrap()
            );
        }
    }
}

#[test]
fn rendered_fragments_match_real_ts_accumulation_including_null() {
    let oracle = oracle();
    for row in array(oracle.get("rendered").unwrap()) {
        let fragment = render::fragment(row.get("event").unwrap()).unwrap_or_default();
        assert_eq!(
            &fragment,
            row.get("accumulated").unwrap().as_string().unwrap()
        );
    }
}

#[test]
fn mapper_matches_real_ts_new_commands_and_instance_state() {
    let mut mapper = WhatsAppSessionMapper::new(
        Default::default(),
        Box::new(|| "12345678-1234-4123-8123-123456789012".into()),
    );
    let oracle = oracle();
    for row in array(oracle.get("mapped").unwrap()) {
        let input = row.get("input").unwrap();
        let expected = row.get("output").unwrap();
        let actual = mapper.resolve(
            input.get("chatId").unwrap().as_string().unwrap(),
            input.get("text").unwrap().as_string().unwrap(),
        );
        assert_eq!(
            &actual.session_key,
            expected.get("sessionKey").unwrap().as_string().unwrap()
        );
        assert_eq!(
            &actual.message,
            expected.get("message").unwrap().as_string().unwrap()
        );
        assert_eq!(actual.command_new, expected.get("command").is_some());
    }
    let JsValue::Object(expected) = oracle
        .get("snapshot")
        .unwrap()
        .get("activeByChatId")
        .unwrap()
    else {
        panic!("state")
    };
    let actual = mapper.snapshot();
    for (chat, session) in expected.own_entries() {
        assert_eq!(actual.active_by_chat_id.get(chat), session.as_string());
    }
}

#[tokio::test]
async fn poll_preserves_dedup_busy_and_new_session_admission() {
    let f = fixture();
    f.inbound("one", "chat", "first").await;
    until("first turn", || {
        f.gateway.requests.lock().unwrap().len() == 1
    })
    .await;
    f.inbound("two", "chat", "/new ignored while busy").await;
    f.inbound("one", "other", "same id still ignored").await;
    assert_eq!(f.gateway.requests.lock().unwrap().len(), 1);
    assert!(
        f.logs
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("already active"))
    );
    f.gateway
        .event(0, r#"{"type":"assistant_text_delta","text":" answer "}"#);
    f.gateway.finish(0);
    until("answer", || f.bridge.sent().len() == 1).await;
    f.inbound("new", "chat", "/new").await;
    until("new reply", || f.bridge.sent().len() == 2).await;
    f.inbound("next", "chat", "next").await;
    until("next turn", || {
        f.gateway.requests.lock().unwrap().len() == 2
    })
    .await;
    {
        let requests = f.gateway.requests.lock().unwrap();
        assert!(
            requests[0]
                .session_key
                .equals_str("whatsapp:chat=chat:general")
        );
        assert!(
            requests[1]
                .session_key
                .equals_str("whatsapp:chat=chat:s_12345678-1234-4123-8123-123456789012")
        );
        assert!(requests[1].channel_key.as_js().equals_str("whatsapp"));
    }
    assert_eq!(
        f.bridge.sent()[0].get("message"),
        Some(&JsValue::from("answer"))
    );
    assert_eq!(
        f.bridge.sent()[1].get("message"),
        Some(&JsValue::from("已创建新会话。"))
    );
    f.handle.stop("test").await.unwrap();
    assert_eq!(f.gateway.cancelled.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn empty_text_claims_the_id_before_dispatch_and_arrays_admit_in_order() {
    let f = fixture();
    f.inbound("empty", "chat", "").await;
    f.inbound("empty", "chat", "later ignored").await;
    assert!(f.gateway.requests.lock().unwrap().is_empty());
    f.bridge.incoming.lock().unwrap().push_back(r#"[{"id":"a","chatId":"chat","text":"first"},{"id":"b","chatId":"chat","text":"second"},{"id":"c","chatId":"other","text":"third"}]"#.into());
    f.channel.0.clone().poll_once().await;
    until("two chats", || {
        f.gateway.requests.lock().unwrap().len() == 2
    })
    .await;
    let messages: Vec<_> = f
        .gateway
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.message.clone())
        .collect();
    assert_eq!(
        messages,
        vec![JsString::from("first"), JsString::from("third")]
    );
    f.handle.stop("test").await.unwrap();
}

#[tokio::test]
async fn permission_failure_releases_answer_and_real_helper_retries() {
    let f = fixture();
    f.inbound("initial", "chat", "permission").await;
    until("turn", || f.gateway.requests.lock().unwrap().len() == 1).await;
    f.bridge.statuses.lock().unwrap().push_back(503);
    f.gateway.event(
        0,
        r#"{"type":"permission_request","requestId":"p1","toolName":"write_file","payload":{}}"#,
    );
    until("failed prompt", || {
        f.logs
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("send HTTP 503: fixture denied"))
    })
    .await;
    f.inbound("retry", "chat", "1").await;
    until("status and retry prompt", || f.bridge.sent().len() == 3).await;
    assert!(f.responder.permissions.lock().unwrap().is_empty());
    f.responder.fail_permission.store(true, Ordering::Release);
    f.inbound("decision1", "chat", "2").await;
    until("failed decision", || {
        f.logs
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("permission answer error"))
    })
    .await;
    f.inbound("decision2", "chat", "2").await;
    until("retried decision", || {
        f.responder.permissions.lock().unwrap().len() == 2
    })
    .await;
    assert_eq!(
        f.responder.permissions.lock().unwrap()[1].get("remember"),
        Some(&JsValue::Bool(true))
    );
    f.handle.stop("test").await.unwrap();
    assert!(!f.channel.0.interaction.permission_pending(&"chat".into()));
}

#[tokio::test]
async fn elicitation_answers_precede_busy_and_reach_the_real_helper_responder() {
    let f = fixture();
    f.inbound("initial", "chat", "question").await;
    until("turn", || f.gateway.requests.lock().unwrap().len() == 1).await;
    f.gateway.event(0, r#"{"type":"elicitation_request","requestId":"e1","questions":[{"header":"Choice","question":"Select","options":[{"label":"one"},{"label":"two"}]}]}"#);
    until("question prompt", || f.bridge.sent().len() == 1).await;
    f.inbound("answer", "chat", "2").await;
    until("elicitation responder", || {
        f.responder.elicitation.lock().unwrap().len() == 1
    })
    .await;
    assert_eq!(f.gateway.requests.lock().unwrap().len(), 1);
    f.handle.stop("test").await.unwrap();
}

#[tokio::test]
async fn dropped_stop_waiter_keeps_cleanup_owned_and_failed_cancel_is_retryable() {
    let f = fixture();
    f.inbound("initial", "chat", "hang").await;
    until("turn", || f.gateway.requests.lock().unwrap().len() == 1).await;
    f.gateway.fail_cancel.store(true, Ordering::Release);
    drop(f.handle.stop("config-disabled"));
    until("first cancellation", || {
        f.gateway.cancelled.load(Ordering::Acquire) == 1
    })
    .await;
    until("failed cancellation retained", || {
        !f.channel.0.failed_streams.lock().unwrap().is_empty()
    })
    .await;
    // Wait for the independent first cleanup worker to publish its outcome.
    tokio::task::yield_now().await;
    let first_or_retry = f.handle.stop("retry").await;
    if first_or_retry.is_err() {
        f.handle.stop("retry-after-result").await.unwrap();
    }
    assert_eq!(f.gateway.cancelled.load(Ordering::Acquire), 2);
    assert!(f.channel.0.failed_streams.lock().unwrap().is_empty());
    assert!(!f.channel.0.outbound.lock().unwrap().accepting);
}

#[tokio::test]
async fn cron_sender_is_owned_even_when_caller_drops_and_stop_cancels_http() {
    let f = fixture();
    f.bridge.hang_post.store(true, Ordering::Release);
    let delivery = CronResultDelivery {
        task_id: "task".into(),
        run_id: "run".into(),
        session_key: "whatsapp:chat=chat:general".into(),
        channel_key: ChannelKey::new("whatsapp"),
        origin_session_key: None,
        origin_channel_key: None,
        text: "cron result".into(),
    };
    let future = f.channel.deliver_cron_result(&delivery);
    let caller = tokio::spawn(future);
    until("cron POST", || f.bridge.sent().len() == 1).await;
    caller.abort();
    f.handle.stop("shutdown").await.unwrap();
    assert_eq!(f.bridge.dropped.load(Ordering::Acquire), 1);
    assert!(!f.channel.deliver_cron_result(&delivery).await);
    let request = &f.bridge.requests.lock().unwrap()[0];
    assert_eq!(request.0, "POST");
    assert_eq!(request.1, "http://127.0.0.1:18081/send");
    assert_eq!(request.2.get("content-type").unwrap(), "application/json");
}

#[tokio::test(start_paused = true)]
async fn send_deadline_is_sixty_seconds_and_response_failure_is_observable() {
    let f = fixture();
    f.bridge.hang_post.store(true, Ordering::Release);
    let began = tokio::time::Instant::now();
    assert!(!f.channel.0.send_reply(&"chat".into(), &"body".into()).await);
    assert_eq!(began.elapsed(), Duration::from_secs(60));
    assert_eq!(f.bridge.dropped.load(Ordering::Acquire), 1);
    assert!(
        f.logs
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("TimeoutError"))
    );
    f.handle.stop("test").await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn readiness_accepts_404_without_reading_its_stalled_body() {
    let f = fixture();
    f.bridge.statuses.lock().unwrap().push_back(404);
    f.bridge.hang_get_body.store(true, Ordering::Release);
    let began = tokio::time::Instant::now();
    assert!(f.channel.0.wait_for_ready().await);
    assert_eq!(began.elapsed(), Duration::ZERO);
    f.handle.stop("test").await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn overlapping_polls_are_all_owned_and_cancelled_during_stop() {
    let f = fixture();
    f.bridge.hang_get_body.store(true, Ordering::Release);
    *f.channel.0.poller.lock().unwrap() = Some(tokio::spawn(f.channel.0.clone().poll_loop()));
    until("first poll", || {
        f.bridge.requests.lock().unwrap().len() == 1
    })
    .await;
    tokio::time::advance(Duration::from_secs(2)).await;
    until("second overlapping poll", || {
        f.bridge.requests.lock().unwrap().len() == 2
    })
    .await;
    f.handle.stop("test").await.unwrap();
    assert!(f.channel.0.polls.lock().unwrap().is_empty());
    tokio::time::advance(Duration::from_secs(4)).await;
    assert_eq!(f.bridge.requests.lock().unwrap().len(), 2);
}

#[test]
fn loader_rejects_bad_constructor_values_with_captured_ts_errors() {
    use crate::channel::loader::{
        ChannelLoaderMounts, LoadEnabledChannelsOptions, load_enabled_channels,
    };
    let mut mounts = ChannelLoaderMounts::default();
    register(&mut mounts, Arc::new(Responder::default()));
    let oracle = oracle();
    for row in array(oracle.get("constructors").unwrap()) {
        let Some(expected) = row.get("error").and_then(JsValue::as_string) else {
            continue;
        };
        let mut cfg = JsObject::new();
        cfg.insert("enabled".into(), JsValue::Bool(true));
        cfg.insert("extra".into(), row.get("options").unwrap().clone());
        let mut adapters = JsObject::new();
        adapters.insert("whatsapp".into(), JsValue::Object(cfg));
        let mut errors = Vec::new();
        let result = load_enabled_channels(
            Some(&JsValue::Object(adapters)),
            &LoadEnabledChannelsOptions { pilot_home: None },
            &mounts,
            &mut |line| errors.push(line),
        );
        assert!(result.is_empty());
        assert_eq!(
            errors,
            vec![format!(
                "[adapters] Failed to load channel \"whatsapp\": {}",
                String::from_utf16_lossy(expected.units())
            )]
        );
    }
}

#[tokio::test]
async fn send_keeps_one_json_message_with_long_and_lone_utf16_content() {
    let f = fixture();
    let mut units = vec![97; 9000];
    units.extend([0xd800, 0x62]);
    let text = JsString::from_utf16(units);
    assert!(f.channel.0.send_reply(&"chat".into(), &text).await);
    assert_eq!(f.bridge.sent().len(), 1);
    assert_eq!(
        f.bridge.sent()[0].get("message"),
        Some(&JsValue::String(text))
    );
    f.handle.stop("test").await.unwrap();
}

#[tokio::test]
async fn submit_failure_replaces_partial_reply_with_the_captured_fallback() {
    let f = fixture();
    f.inbound("one", "chat", "fail").await;
    until("turn", || f.gateway.requests.lock().unwrap().len() == 1).await;
    f.gateway
        .event(0, r#"{"type":"assistant_text_delta","text":"partial"}"#);
    f.gateway.streams.lock().unwrap()[0]
        .send(Err(ChannelError::error("fixture failure")))
        .unwrap();
    until("error reply", || f.bridge.sent().len() == 1).await;
    assert_eq!(
        f.bridge.sent()[0].get("message"),
        Some(&JsValue::from("处理消息时发生错误，请重试。"))
    );
    assert!(
        f.logs
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("submitTurn error: Error: fixture failure"))
    );
    f.handle.stop("test").await.unwrap();
}

#[tokio::test]
async fn complete_interaction_payloads_and_prompt_delivery_match_ts_module_traces() {
    let oracle = parse_json(include_str!(
        "../../../tests/fixtures/h1-whatsapp/interaction-oracle.json"
    ))
    .unwrap();
    for row in array(oracle.get("records").unwrap()) {
        let mode = row.get("mode").unwrap().as_string().unwrap();
        let elicitation = mode.equals_str("elicitation");
        let failed = mode.equals_str("permission-initial-failure");
        let f = fixture();
        f.inbound("start", "chat", "start").await;
        until("turn", || f.gateway.requests.lock().unwrap().len() == 1).await;
        if failed {
            f.bridge.statuses.lock().unwrap().push_back(503);
        }
        if elicitation {
            f.gateway.event(0,r#"{"type":"elicitation_request","requestId":"e1","questions":[{"id":"q","header":"h","question":"Choose","options":[{"label":"A"},{"label":"B"}]}]}"#);
        } else {
            f.gateway.event(0,r#"{"type":"permission_request","requestId":"p1","toolName":"write","payload":{"path":"fixture.txt"}}"#);
            f.gateway.event(0,r#"{"type":"permission_request","requestId":"p2","toolName":"read","payload":{"path":"fixture2.txt"}}"#);
        }
        until("prompts captured", || {
            f.gateway.events_read.load(Ordering::Acquire) == if elicitation { 1 } else { 2 }
        })
        .await;
        until("first prompt", || f.bridge.sent().len() == 1).await;
        if failed {
            until("initial failure settled", || {
                f.logs
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|line| line.contains("send HTTP 503"))
            })
            .await;
            f.inbound("retry", "chat", "1").await;
            until("retry reply drained", || {
                f.bridge.sent().len() == 3
                    && f.channel
                        .0
                        .handlers
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|task| !task.is_finished())
                        .count()
                        == 1
            })
            .await;
            assert!(f.responder.permissions.lock().unwrap().is_empty());
        }
        f.inbound("answer-one", "chat", "1").await;
        until("answer one drained", || {
            let count = if elicitation {
                f.responder.elicitation.lock().unwrap().len()
            } else {
                f.responder.permissions.lock().unwrap().len()
            };
            count == 1
                && f.channel
                    .0
                    .handlers
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|task| !task.is_finished())
                    .count()
                    == 1
        })
        .await;
        if !elicitation {
            f.inbound("answer-two", "chat", "2").await;
            until("answer two drained", || {
                f.responder.permissions.lock().unwrap().len() == 2
                    && f.channel
                        .0
                        .handlers
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|task| !task.is_finished())
                        .count()
                        == 1
            })
            .await;
        }
        f.gateway
            .event(0, r#"{"type":"assistant_text_delta","text":"finished"}"#);
        f.gateway.finish(0);
        let expected_posts = array(row.get("posts").unwrap());
        until("final response", || {
            f.bridge.sent().len() == expected_posts.len()
        })
        .await;
        assert_eq!(
            f.bridge.sent(),
            expected_posts
                .iter()
                .map(|post| post.get("body").unwrap().clone())
                .collect::<Vec<_>>()
        );
        let answers = if elicitation {
            f.responder.elicitation.lock().unwrap().clone()
        } else {
            f.responder.permissions.lock().unwrap().clone()
        };
        assert_eq!(answers, array(row.get("answers").unwrap()));
        assert_eq!(
            f.logs
                .lock()
                .unwrap()
                .iter()
                .map(|line| JsValue::from(line.as_str()))
                .collect::<Vec<_>>(),
            array(row.get("logs").unwrap())
        );
        f.handle.stop("test").await.unwrap();
    }
}

#[tokio::test]
async fn synchronous_start_failure_matches_real_ts_and_never_polls_http() {
    let oracle = parse_json(include_str!(
        "../../../tests/fixtures/h1-whatsapp/start-oracle.json"
    ))
    .unwrap();
    for case in array(oracle.get("records").unwrap()) {
        let bridge = Arc::new(HttpMock::default());
        let logs = Arc::new(Mutex::new(Vec::<JsValue>::new()));
        let capture = logs.clone();
        let channel = WhatsAppChannel::new(
            WhatsAppChannelOptions {
                bridge_path: Some(case.get("bridgePath").unwrap().as_string().unwrap().clone()),
                bridge_url: Some("http://127.0.0.1:18081".into()),
                transport: Some(bridge.clone()),
                env: Some(Arc::new(|_| None)),
                ..Default::default()
            },
            js_helper_interaction(Arc::new(Responder::default())),
        );
        let handle = channel
            .start(ChannelStartDeps {
                log: Some(Arc::new(move |level, line| {
                    capture.lock().unwrap().push(JsValue::Array(vec![
                        JsValue::String(level.into()),
                        JsValue::String(line.into()),
                    ]));
                })),
                ..Default::default()
            })
            .await
            .expect("TS returns noop, not a failed start");
        handle.stop("noop").await.unwrap();
        assert_eq!(
            JsValue::Array(logs.lock().unwrap().clone()),
            *case.get("logs").unwrap()
        );
        assert!(
            bridge.requests.lock().unwrap().is_empty(),
            "spawn validation precedes readiness"
        );
        assert!(channel.0.bridge.lock().unwrap().is_none());
        assert!(!channel.0.running.load(Ordering::Acquire));
    }
}

#[tokio::test]
async fn os_spawn_failure_emits_child_error_and_still_checks_readiness() {
    let bridge = Arc::new(HttpMock::default());
    let logs = Arc::new(Mutex::new(Vec::<String>::new()));
    let capture = logs.clone();
    let executable =
        std::env::temp_dir().join(format!("missing-whatsapp-node-{}", uuid::Uuid::new_v4()));
    let channel = WhatsAppChannel::new(
        WhatsAppChannelOptions {
            bridge_path: Some("fixture.mjs".into()),
            bridge_url: Some("http://127.0.0.1:18081".into()),
            node_executable: Some(executable.clone()),
            transport: Some(bridge.clone()),
            env: Some(Arc::new(|_| None)),
            ..Default::default()
        },
        js_helper_interaction(Arc::new(Responder::default())),
    );
    let handle = channel
        .start(ChannelStartDeps {
            log: Some(Arc::new(move |_, line| {
                capture.lock().unwrap().push(line.into())
            })),
            ..Default::default()
        })
        .await
        .expect("async child error is distinct from synchronous spawn throw");
    assert_eq!(
        logs.lock().unwrap()[0],
        format!(
            "whatsapp: bridge spawn error: Error: spawn {} ENOENT",
            executable.display()
        )
    );
    assert!(!bridge.requests.lock().unwrap().is_empty());
    assert!(channel.0.running.load(Ordering::Acquire));
    handle.stop("fixture").await.unwrap();
    let oracle = parse_json(include_str!(
        "../../../tests/fixtures/h1-whatsapp/start-oracle.json"
    ))
    .unwrap();
    let expected = array(oracle.get("asyncRecord").unwrap().get("logs").unwrap())
        .iter()
        .map(|entry| {
            String::from_utf16_lossy(array(entry)[1].as_string().unwrap().units())
                .replace("<missing-executable>", &executable.to_string_lossy())
        })
        .collect::<Vec<_>>();
    assert_eq!(*logs.lock().unwrap(), expected);
}
