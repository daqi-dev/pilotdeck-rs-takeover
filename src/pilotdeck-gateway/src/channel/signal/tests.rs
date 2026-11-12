//! The formal CLI oracle is captured separately by h1-signal/capture.mjs.
//! These native component tests exercise real helper state and the native
//! fetch/stream boundary; they do not substitute for the formal server pair.
use super::*;
use crate::channel::im_interaction::InteractionResponder;
use crate::channel::sms::{ElicitationResponse, PermissionDecision, PermissionDecisionOutcome};
use crate::runtime::model::http::{BodyFailure, TransportFuture};
use http_body_util::{Full, StreamBody};
use hyper::{Request, body::Frame};
use std::{collections::VecDeque, future::Future, pin::Pin, sync::atomic::AtomicUsize};
use tokio::sync::mpsc;

#[derive(Default)]
struct BridgeState {
    requests: Vec<(String, String, HeaderMap, String)>,
    receivers: Vec<mpsc::UnboundedSender<Result<Frame<Bytes>, BodyFailure>>>,
    send_statuses: VecDeque<u16>,
    receive_statuses: VecDeque<u16>,
}
#[derive(Default)]
struct Bridge(Arc<Mutex<BridgeState>>);
impl Bridge {
    fn send_line(&self, line: &str) {
        self.0
            .lock()
            .unwrap()
            .receivers
            .last()
            .unwrap()
            .send(Ok(Frame::data(Bytes::from(line.to_owned()))))
            .unwrap();
    }
    fn inbound(&self, text: &str) {
        self.send_line(&format!(
            "{}\n",
            serde_json::json!({"envelope":{"source":"+10000000001","dataMessage":{"message":text}}})
        ));
    }
    fn sent(&self) -> Vec<JsValue> {
        self.0
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(method, _, _, _)| method == "POST")
            .map(|(_, _, _, body)| parse_json(body).unwrap())
            .collect()
    }
}
fn full(bytes: impl Into<Bytes>) -> ModelBody {
    Full::new(bytes.into())
        .map_err(|never| match never {})
        .boxed()
}
impl ModelHttpTransport for Bridge {
    fn send(&self, request: Request<Full<Bytes>>) -> TransportFuture {
        let state = self.0.clone();
        TransportFuture::io(async move {
            let (parts, body) = request.into_parts();
            let body = body.collect().await.unwrap().to_bytes();
            let mut state = state.lock().unwrap();
            state.requests.push((
                parts.method.to_string(),
                parts.uri.to_string(),
                parts.headers,
                String::from_utf8(body.to_vec()).unwrap(),
            ));
            if parts.method == Method::GET {
                let status = state.receive_statuses.pop_front().unwrap_or(200);
                if status != 200 {
                    return Ok(Response::builder()
                        .status(status)
                        .body(full("receive-error"))
                        .unwrap());
                }
                let (tx, rx) = mpsc::unbounded_channel();
                state.receivers.push(tx);
                let stream = futures_util::stream::unfold(rx, |mut rx| async {
                    rx.recv().await.map(|frame| (frame, rx))
                });
                Ok(Response::builder()
                    .status(status)
                    .body(StreamBody::new(stream).boxed())
                    .unwrap())
            } else {
                let status = state.send_statuses.pop_front().unwrap_or(200);
                Ok(Response::builder()
                    .status(status)
                    .body(full(if status == 200 {
                        "{}".to_owned()
                    } else {
                        "x".repeat(520)
                    }))
                    .unwrap())
            }
        })
    }
}

#[derive(Default)]
struct Responder {
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
}
struct Events {
    rx: mpsc::UnboundedReceiver<Result<Option<JsValue>, ChannelError>>,
    cancelled: Arc<AtomicUsize>,
    fail_cancel: Arc<AtomicBool>,
    cancel_gate: Option<watch::Receiver<bool>>,
}
impl TurnEventStream for Events {
    fn next(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<JsValue>, ChannelError>> + Send + '_>> {
        Box::pin(async { self.rx.recv().await.unwrap_or(Ok(None)) })
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
        Box::pin(async move {
            Ok(Box::new(Events {
                rx,
                cancelled,
                fail_cancel,
                cancel_gate,
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
struct Fixture {
    channel: SignalChannel,
    handle: Arc<dyn ChannelHandle>,
    bridge: Arc<Bridge>,
    gateway: Arc<Gateway>,
    responder: Arc<Responder>,
    logs: Arc<Mutex<Vec<String>>>,
}
async fn fixture() -> Fixture {
    let bridge = Arc::new(Bridge::default());
    let gateway = Arc::new(Gateway::default());
    let responder = Arc::new(Responder::default());
    let logs = Arc::new(Mutex::new(Vec::new()));
    let channel = SignalChannel::new(
        SignalChannelOptions {
            rest_url: Some(JsString::from("http://127.0.0.1:18081///")),
            account: Some(JsValue::from("+10000000000")),
            transport: Some(bridge.clone()),
            env: Some(Arc::new(|_| None)),
            mapper: Some(SignalSessionMapper::new(
                SignalSessionMapperState::default(),
                Box::new(|| JsString::from("12345678-1234-4123-8123-123456789012")),
            )),
        },
        js_helper_interaction(responder.clone()),
    );
    let capture = logs.clone();
    let handle = channel
        .start(ChannelStartDeps {
            turn_gateway: Some(gateway.clone()),
            log: Some(Arc::new(move |_, line| {
                capture.lock().unwrap().push(line.to_owned())
            })),
            ..Default::default()
        })
        .await
        .unwrap();
    until("receive GET", || {
        !bridge.0.lock().unwrap().receivers.is_empty()
    })
    .await;
    Fixture {
        channel,
        handle,
        bridge,
        gateway,
        responder,
        logs,
    }
}

#[test]
fn utf16_chunks_match_the_formal_cli_oracle() {
    let text = JsString::from(format!("{}😀z", "a".repeat(1999)));
    let chunks = wire::chunk_text(&text);
    // serde_json deliberately cannot deserialize a lone surrogate string;
    // parse the raw oracle through the JS-preserving protocol codec instead.
    let oracle_js = parse_json(include_str!(
        "../../../tests/fixtures/h1-signal/formal-oracle.json"
    ))
    .unwrap();
    let JsValue::Array(sends) = oracle_js.get("sends").unwrap() else {
        panic!("oracle sends")
    };
    assert_eq!(
        chunks,
        vec![
            sends[3]
                .get("message")
                .unwrap()
                .as_string()
                .unwrap()
                .clone(),
            sends[4]
                .get("message")
                .unwrap()
                .as_string()
                .unwrap()
                .clone()
        ]
    );
    assert_eq!(oracle_js.get("receiveCount"), Some(&JsValue::Number(2.0)));
    let split = wire::chunk_text(&JsString::from(format!(
        "{} {}",
        "a".repeat(1000),
        "b".repeat(1500)
    )));
    assert_eq!(split[1].units()[0], 32, "space at split is retained");
}

#[tokio::test]
async fn streamed_lines_busy_new_and_mapper_are_separate_admission_steps() {
    let f = fixture().await;
    {
        let requests = &f.bridge.0.lock().unwrap().requests;
        assert_eq!(
            requests[0].1,
            "http://127.0.0.1:18081/v1/receive/%2B10000000000"
        );
        assert_eq!(
            requests[0].2.get("accept").unwrap(),
            "text/event-stream, application/json, */*"
        );
    }
    f.bridge.send_line("data: [DONE]\r\ninvalid\n\n");
    f.bridge.inbound("first");
    until("first turn", || {
        f.gateway.requests.lock().unwrap().len() == 1
    })
    .await;
    f.bridge.inbound("/new ignored while busy");
    until("busy skip", || {
        f.logs
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("already active"))
    })
    .await;
    f.gateway
        .event(0, r#"{"type":"assistant_text_delta","text":"one"}"#);
    f.gateway.finish(0);
    until("first output", || f.bridge.sent().len() == 1).await;
    f.bridge.inbound("/new");
    until("new acknowledgement", || f.bridge.sent().len() == 2).await;
    f.bridge.inbound("next");
    until("new turn", || f.gateway.requests.lock().unwrap().len() == 2).await;
    {
        let requests = f.gateway.requests.lock().unwrap();
        assert!(
            requests[0]
                .session_key
                .equals_str("signal:chat=dm:+10000000001:general")
        );
        assert!(
            requests[1]
                .session_key
                .equals_str("signal:chat=dm:+10000000001:s_12345678-1234-4123-8123-123456789012")
        );
        assert!(requests[1].message.equals_str("next"));
    }
    f.handle.stop("test").await.unwrap();
    assert_eq!(
        f.gateway.cancelled.load(Ordering::Acquire),
        1,
        "only still-active turn cancelled"
    );
    assert!(f.bridge.0.lock().unwrap().receivers[0].is_closed());
}

#[tokio::test]
async fn recipient_updates_before_busy_skip_and_sync_text_wins() {
    let f = fixture().await;
    f.bridge.send_line("{\"envelope\":{\"source\":\"first\",\"dataMessage\":{\"message\":\"ordinary\",\"groupInfo\":{\"groupId\":\"g\"}},\"syncMessage\":{\"sentMessage\":{\"message\":\"synced\"}}}}\n");
    until("group turn", || {
        f.gateway.requests.lock().unwrap().len() == 1
    })
    .await;
    f.bridge.send_line("{\"envelope\":{\"source\":\"second\",\"dataMessage\":{\"message\":\"busy\",\"groupInfo\":{\"groupId\":\"g\"}}}}\n");
    until("busy group", || {
        f.logs
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("already active"))
    })
    .await;
    f.gateway
        .event(0, r#"{"type":"assistant_text_delta","text":"reply"}"#);
    f.gateway.finish(0);
    until("group reply", || f.bridge.sent().len() == 1).await;
    assert!(
        f.gateway.requests.lock().unwrap()[0]
            .message
            .equals_str("synced")
    );
    assert_eq!(
        stringify_json(f.bridge.sent()[0].get("recipients").unwrap())
            .unwrap()
            .unwrap(),
        "[\"second\"]"
    );
    f.handle.stop("test").await.unwrap();
}

#[tokio::test]
async fn permission_prompt_failure_retries_before_decision_and_uses_real_helpers() {
    let f = fixture().await;
    f.bridge.0.lock().unwrap().send_statuses.push_back(503);
    f.bridge.inbound("permission");
    until("turn", || f.gateway.requests.lock().unwrap().len() == 1).await;
    f.gateway.event(0,r#"{"type":"permission_request","requestId":"p1","toolName":"write_file","payload":{"path":"fixture"}}"#);
    until("first failed prompt", || {
        f.logs
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.starts_with("signal: send HTTP 503:"))
    })
    .await;
    f.bridge.inbound("1");
    until("status and retried prompt", || f.bridge.sent().len() == 3).await;
    assert!(f.responder.permissions.lock().unwrap().is_empty());
    f.responder.fail_permission.store(true, Ordering::Release);
    f.bridge.inbound("2");
    until("permission decision RPC failure", || {
        f.logs
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("permission answer error: Error: fixture decision failed"))
    })
    .await;
    assert_eq!(f.responder.permissions.lock().unwrap().len(), 1);
    f.bridge.inbound("2");
    until("permission decision retry", || {
        f.responder.permissions.lock().unwrap().len() == 2
    })
    .await;
    assert_eq!(
        f.responder.permissions.lock().unwrap()[1].get("remember"),
        Some(&JsValue::Bool(true))
    );
    f.gateway.finish(0);
    f.handle.stop("test").await.unwrap();
}

#[tokio::test]
async fn stop_drains_owned_permission_turn_before_forcing_helper_clear() {
    let f = fixture().await;
    f.bridge.inbound("permission");
    until("owned permission turn", || {
        f.gateway.requests.lock().unwrap().len() == 1
    })
    .await;
    f.gateway.event(
        0,
        r#"{"type":"permission_request","requestId":"owned","toolName":"write_file","payload":{}}"#,
    );
    until("permission prompt", || f.bridge.sent().len() == 1).await;
    let chat = JsString::from("dm:+10000000001");
    assert!(f.channel.0.interaction.permission_pending(&chat));
    f.handle.stop("config-disabled").await.unwrap();
    assert_eq!(f.gateway.cancelled.load(Ordering::Acquire), 1);
    assert!(!f.channel.0.interaction.permission_pending(&chat));
    assert!(f.bridge.0.lock().unwrap().receivers[0].is_closed());
    f.handle.stop("repeat").await.unwrap();
    assert_eq!(f.gateway.cancelled.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn failed_drain_retains_the_stream_and_helper_until_stop_retry_succeeds() {
    let f = fixture().await;
    f.bridge.inbound("permission");
    until("owned turn", || {
        f.gateway.requests.lock().unwrap().len() == 1
    })
    .await;
    f.gateway.event(
        0,
        r#"{"type":"permission_request","requestId":"owned","toolName":"write_file","payload":{}}"#,
    );
    until("permission prompt", || f.bridge.sent().len() == 1).await;
    f.gateway.fail_cancel.store(true, Ordering::Release);
    assert!(f.handle.stop("config-disabled").await.is_err());
    let chat = JsString::from("dm:+10000000001");
    assert!(f.channel.0.interaction.permission_pending(&chat));
    assert_eq!(f.gateway.cancelled.load(Ordering::Acquire), 1);
    f.handle.stop("retry").await.unwrap();
    assert_eq!(f.gateway.cancelled.load(Ordering::Acquire), 2);
    assert!(!f.channel.0.interaction.permission_pending(&chat));
    assert!(f.channel.0.failed_streams.lock().unwrap().is_empty());
}

#[tokio::test]
async fn dropping_pending_stop_waiters_keeps_cleanup_owned_until_real_drain_finishes() {
    let f = fixture().await;
    let (release, gate) = watch::channel(false);
    *f.gateway.cancel_gate.lock().unwrap() = Some(gate);
    f.bridge.inbound("permission");
    until("owned turn", || {
        f.gateway.requests.lock().unwrap().len() == 1
    })
    .await;
    f.gateway.event(
        0,
        r#"{"type":"permission_request","requestId":"held","toolName":"write_file","payload":{}}"#,
    );
    until("permission prompt", || f.bridge.sent().len() == 1).await;
    let chat = JsString::from("dm:+10000000001");

    let unpolled = f.handle.stop("disable");
    assert!(
        !f.channel.0.running.load(Ordering::Acquire),
        "stop prefix is synchronous"
    );
    assert!(*f.channel.0.stopping.borrow());
    drop(unpolled);
    until(
        "worker entered the owned drain without a polled waiter",
        || f.gateway.cancelled.load(Ordering::Acquire) == 1,
    )
    .await;
    let mut first = f.handle.stop("first waiter");
    until("receive closes while owned drain is still blocked", || {
        f.bridge.0.lock().unwrap().receivers[0].is_closed()
    })
    .await;
    assert!(first.as_mut().now_or_never().is_none());
    drop(first);
    let mut retry = f.handle.stop("retry waiter");
    assert!(
        retry.as_mut().now_or_never().is_none(),
        "retry cannot claim success while the same drain is blocked"
    );
    assert!(f.channel.0.interaction.permission_pending(&chat));
    assert!(
        !f.gateway.streams.lock().unwrap()[0].is_closed(),
        "the actual owned stream is not yet drained"
    );
    assert!(
        f.bridge.0.lock().unwrap().receivers[0].is_closed(),
        "receive closes independently of the blocked turn"
    );

    release.send_replace(true);
    tokio::time::timeout(Duration::from_secs(3), retry)
        .await
        .unwrap()
        .unwrap();
    assert!(f.gateway.streams.lock().unwrap()[0].is_closed());
    assert!(!f.channel.0.interaction.permission_pending(&chat));
    assert!(f.channel.0.active.lock().unwrap().is_empty());
    assert_eq!(
        f.gateway.cancelled.load(Ordering::Acquire),
        1,
        "all waiters share the same drain"
    );
    f.handle.stop("after completion").await.unwrap();
    assert_eq!(f.gateway.cancelled.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn aborted_cleanup_worker_publishes_a_sticky_error_instead_of_retry_success() {
    let f = fixture().await;
    let (release, gate) = watch::channel(false);
    *f.gateway.cancel_gate.lock().unwrap() = Some(gate);
    f.bridge.inbound("held");
    until("owned turn", || {
        f.gateway.requests.lock().unwrap().len() == 1
    })
    .await;
    let waiter = f.handle.stop("disable");
    until("owned drain blocked", || {
        f.gateway.cancelled.load(Ordering::Acquire) == 1
    })
    .await;
    f.channel
        .0
        .cleanup
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        ._worker
        .abort();
    let error = tokio::time::timeout(Duration::from_secs(3), waiter)
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.message().contains("cleanup ownership is unresolved"));
    let retry = f.handle.stop("retry after worker abort").await.unwrap_err();
    assert_eq!(retry, error);
    assert_eq!(f.gateway.cancelled.load(Ordering::Acquire), 1);
    release.send_replace(true);
    until("test releases the underlying owner", || {
        f.gateway.streams.lock().unwrap()[0].is_closed()
    })
    .await;
    assert!(
        f.handle
            .stop("terminal failure remains visible")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn elicitation_answer_precedes_busy_and_preserves_js_question_keys() {
    let f = fixture().await;
    f.bridge.inbound("ask");
    until("turn", || f.gateway.requests.lock().unwrap().len() == 1).await;
    f.gateway.event(0,r#"{"type":"elicitation_request","requestId":"e1","questions":[{"header":"选择","question":"which\ud800","options":[{"label":"one\udfff","description":"desc"},{"label":"two","description":""}]}]}"#);
    until("question delivered", || f.bridge.sent().len() == 1).await;
    f.bridge.inbound("1");
    until("elicitation response", || {
        f.responder.elicitation.lock().unwrap().len() == 1
    })
    .await;
    let response = stringify_json(&f.responder.elicitation.lock().unwrap()[0])
        .unwrap()
        .unwrap();
    assert!(response.contains("which\\ud800"));
    assert!(response.contains("one\\udfff"));
    assert_eq!(f.gateway.requests.lock().unwrap().len(), 1);
    f.gateway.finish(0);
    f.handle.stop("test").await.unwrap();
}

#[tokio::test]
async fn cron_send_failure_continues_chunks_and_stop_rejects_further_delivery() {
    let f = fixture().await;
    f.bridge.0.lock().unwrap().send_statuses.push_back(500);
    let delivery = CronResultDelivery {
        task_id: "t".into(),
        run_id: "r".into(),
        session_key: "signal:chat=group:fixture:general".into(),
        channel_key: ChannelKey::new("signal"),
        origin_session_key: None,
        origin_channel_key: None,
        text: JsString::from("x".repeat(2001)),
    };
    assert!(!f.channel.deliver_cron_result(&delivery).await);
    assert_eq!(f.bridge.sent().len(), 2);
    assert_eq!(f.bridge.sent()[1].get("message"), Some(&JsValue::from("x")));
    assert!(
        f.logs
            .lock()
            .unwrap()
            .iter()
            .any(|l| l == &format!("signal: send HTTP 500: {}", "x".repeat(500)))
    );
    f.handle.stop("test").await.unwrap();
    assert!(!f.channel.deliver_cron_result(&delivery).await);
    assert_eq!(f.bridge.sent().len(), 2);
}

#[test]
fn carry_survives_connection_decoder_reset_and_utf8_tail_is_not_flushed() {
    let mut carry = String::new();
    let mut first = Utf8Stream::default();
    assert!(wire::lines(&mut first, &mut carry, b"\xef\xbb\xbfdata: {\"envelope\":").is_empty());
    let mut second = Utf8Stream::default();
    assert_eq!(
        wire::lines(&mut second, &mut carry, b"{}}\r\n"),
        vec!["data: {\"envelope\":{}}"]
    );
    assert!(wire::lines(&mut second, &mut carry, &[0xe4]).is_empty());
    let mut third = Utf8Stream::default();
    assert_eq!(wire::lines(&mut third, &mut carry, b"x\n"), vec!["x"]);
}

struct ReadyGateway(AtomicUsize);
struct PanickingGateway;
struct PanickingEvents;
impl TurnEventStream for PanickingEvents {
    fn next(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<JsValue>, ChannelError>> + Send + '_>> {
        Box::pin(async { panic!("fixture owned handler failure") })
    }
}
impl TurnGateway for PanickingGateway {
    fn submit(
        &self,
        _request: TurnRequest,
    ) -> BoxFuture<Result<Box<dyn TurnEventStream>, ChannelError>> {
        Box::pin(async { Ok(Box::new(PanickingEvents) as Box<dyn TurnEventStream>) })
    }
}

#[tokio::test]
async fn harvesting_a_panicked_handler_preserves_fatal_error_for_later_stop() {
    let f = fixture().await;
    f.channel.0.state.lock().unwrap().gateway = Some(Arc::new(PanickingGateway));
    f.bridge.inbound("panic in owned handler");
    until("owned handler finished with panic", || {
        f.channel
            .0
            .handlers
            .lock()
            .unwrap()
            .iter()
            .any(JoinHandle::is_finished)
    })
    .await;
    f.bridge.send_line(":ok\n");
    until("next reader batch harvested the failure", || {
        f.channel.0.fatal_cleanup.load(Ordering::Acquire)
    })
    .await;
    assert!(f.channel.0.handlers.lock().unwrap().is_empty());
    let error = f.handle.stop("disable").await.unwrap_err();
    assert!(error.to_js_string().contains("owned turn cleanup failed"));
    assert!(
        f.handle.stop("retry").await.is_err(),
        "harvested panic cannot become a successful retry"
    );
}

struct EmptyEvents;
impl TurnEventStream for EmptyEvents {
    fn next(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<JsValue>, ChannelError>> + Send + '_>> {
        Box::pin(async { Ok(None) })
    }
    fn cancel_and_drain(
        &mut self,
        _reason: JsString,
    ) -> Pin<Box<dyn Future<Output = Result<(), ChannelError>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }
}
impl TurnGateway for ReadyGateway {
    fn submit(
        &self,
        _request: TurnRequest,
    ) -> BoxFuture<Result<Box<dyn TurnEventStream>, ChannelError>> {
        self.0.fetch_add(1, Ordering::AcqRel);
        Box::pin(async { Ok(Box::new(EmptyEvents) as Box<dyn TurnEventStream>) })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_reader_batch_preserves_busy_even_when_the_first_turn_is_ready_and_empty() {
    let f = fixture().await;
    let gateway = Arc::new(ReadyGateway(AtomicUsize::new(0)));
    f.channel.0.state.lock().unwrap().gateway = Some(gateway.clone());
    let first = serde_json::json!({"envelope":{"source":"+1","dataMessage":{"message":"one"}}});
    let second = serde_json::json!({"envelope":{"source":"+1","dataMessage":{"message":"two"}}});
    f.bridge.send_line(&format!("{first}\n{second}\n"));
    until("same batch second line is busy", || {
        f.logs
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("already active"))
    })
    .await;
    until("first empty turn is finished", || {
        f.channel.0.active.lock().unwrap().is_empty()
    })
    .await;
    assert_eq!(gateway.0.load(Ordering::Acquire), 1);
    assert!(f.bridge.sent().is_empty());
    f.bridge.send_line(&format!("{second}\n"));
    until("later batch may start again", || {
        gateway.0.load(Ordering::Acquire) == 2
    })
    .await;
    f.handle.stop("test").await.unwrap();
}

struct RedirectTransport(AtomicUsize);

#[tokio::test]
async fn fetch_bad_port_is_rejected_before_the_injected_transport_is_called() {
    let bridge = Arc::new(Bridge::default());
    let channel = SignalChannel::new(
        SignalChannelOptions {
            transport: Some(bridge.clone()),
            env: Some(Arc::new(|_| None)),
            ..Default::default()
        },
        js_helper_interaction(Arc::new(Responder::default())),
    );
    assert!(
        channel
            .0
            .request(Method::GET, "http://127.0.0.1:1/first", None)
            .await
            .is_err()
    );
    assert!(bridge.0.lock().unwrap().requests.is_empty());
    let response = channel
        .0
        .request(Method::GET, "http://127.0.0.1:18081/first", None)
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(bridge.0.lock().unwrap().requests.len(), 1);
}

impl ModelHttpTransport for RedirectTransport {
    fn send(&self, _request: Request<Full<Bytes>>) -> TransportFuture {
        let hop = self.0.fetch_add(1, Ordering::AcqRel);
        let response = if hop == 0 {
            Response::builder()
                .status(307)
                .header("location", "/second")
                .body(full(""))
                .unwrap()
        } else {
            Response::new(full("ok"))
        };
        TransportFuture::after(Duration::from_secs(400), Ok(response))
    }
}

#[tokio::test(start_paused = true)]
async fn redirect_hops_receive_independent_header_deadlines() {
    let transport = Arc::new(RedirectTransport(AtomicUsize::new(0)));
    let responder = Arc::new(Responder::default());
    let channel = SignalChannel::new(
        SignalChannelOptions {
            transport: Some(transport.clone()),
            env: Some(Arc::new(|_| None)),
            ..Default::default()
        },
        js_helper_interaction(responder),
    );
    let start = tokio::time::Instant::now();
    let response = channel
        .0
        .request(Method::GET, "http://127.0.0.1:18081/first", None)
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(transport.0.load(Ordering::Acquire), 2);
    assert_eq!(start.elapsed(), Duration::from_secs(800));
}

struct PendingTcpBridge {
    url: String,
    receiving: oneshot::Receiver<()>,
    posted: oneshot::Receiver<String>,
    post_eof: oneshot::Receiver<()>,
    server: JoinHandle<()>,
}

struct PanickingPostTransport(Arc<Bridge>);
impl ModelHttpTransport for PanickingPostTransport {
    fn send(&self, request: Request<Full<Bytes>>) -> TransportFuture {
        if request.method() == Method::POST {
            TransportFuture::io(async { panic!("fixture outbound owner failure") })
        } else {
            self.0.send(request)
        }
    }
}

#[tokio::test]
async fn outbound_worker_failure_settles_reply_false_and_keeps_stop_error_visible() {
    let bridge = Arc::new(Bridge::default());
    let channel = SignalChannel::new(
        SignalChannelOptions {
            rest_url: Some("http://127.0.0.1:18081".into()),
            account: Some(JsValue::from("+10000000000")),
            transport: Some(Arc::new(PanickingPostTransport(bridge.clone()))),
            env: Some(Arc::new(|_| None)),
            ..Default::default()
        },
        js_helper_interaction(Arc::new(Responder::default())),
    );
    let handle = channel.start(ChannelStartDeps::default()).await.unwrap();
    until("receive", || !bridge.0.lock().unwrap().receivers.is_empty()).await;
    let delivery = CronResultDelivery {
        task_id: "t".into(),
        run_id: "r".into(),
        session_key: "signal:chat=dm:+10000000001:general".into(),
        channel_key: ChannelKey::new("signal"),
        origin_session_key: None,
        origin_channel_key: None,
        text: "cron".into(),
    };
    assert!(
        !tokio::time::timeout(
            Duration::from_secs(3),
            channel.deliver_cron_result(&delivery)
        )
        .await
        .unwrap()
    );
    assert!(handle.stop("disable").await.is_err());
    assert!(handle.stop("retry").await.is_err());
}

async fn pending_tcp_bridge() -> PendingTcpBridge {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (receiving_tx, receiving) = oneshot::channel();
    let (posted_tx, posted) = oneshot::channel();
    let (eof_tx, post_eof) = oneshot::channel();
    let receiving_tx = Arc::new(Mutex::new(Some(receiving_tx)));
    let posted_tx = Arc::new(Mutex::new(Some(posted_tx)));
    let eof_tx = Arc::new(Mutex::new(Some(eof_tx)));
    let server = tokio::spawn(async move {
        let mut connections = Vec::new();
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let receiving = receiving_tx.clone();
            let posted = posted_tx.clone();
            let eof = eof_tx.clone();
            connections.push(tokio::spawn(async move {
                let mut bytes = Vec::new();
                let mut part = [0; 4096];
                let header_end = loop {
                    let count = socket.read(&mut part).await.unwrap();
                    assert_ne!(count, 0, "client sent complete headers");
                    bytes.extend_from_slice(&part[..count]);
                    assert!(bytes.len() < 65536);
                    if let Some(at) = bytes.windows(4).position(|v|v == b"\r\n\r\n") { break at + 4; }
                };
                let header = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
                let is_post = header.starts_with("POST /v2/send ");
                if is_post {
                    let length = header.lines().find_map(|line| {
                        let (name,value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length").then(||value.trim().parse::<usize>().unwrap())
                    }).expect("request has explicit full-body length");
                    while bytes.len() < header_end + length {
                        let count = socket.read(&mut part).await.unwrap();
                        assert_ne!(count,0);
                        bytes.extend_from_slice(&part[..count]);
                    }
                    posted.lock().unwrap().take().unwrap()
                        .send(String::from_utf8(bytes[header_end..header_end+length].to_vec()).unwrap()).unwrap();
                    // No response headers: the actual HTTP exchange remains
                    // pending until its adapter-owned worker is cancelled.
                } else {
                    assert!(header.starts_with("GET /v1/receive/%2B10000000000 "));
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n:ok\n\r\n").await.unwrap();
                    receiving.lock().unwrap().take().unwrap().send(()).unwrap();
                }
                loop {
                    match socket.read(&mut part).await {
                        Ok(0) => break,
                        Ok(_) => {},
                        Err(e) if matches!(e.kind(),std::io::ErrorKind::ConnectionReset|std::io::ErrorKind::BrokenPipe) => break,
                        Err(e) => panic!("peer read failed: {e}"),
                    }
                }
                if is_post { eof.lock().unwrap().take().unwrap().send(()).unwrap(); }
            }));
        }
        for connection in connections {
            connection.await.unwrap();
        }
    });
    PendingTcpBridge {
        url,
        receiving,
        posted,
        post_eof,
        server,
    }
}

#[tokio::test]
async fn cron_http_is_drained_without_polling_or_retaining_the_external_delivery_waiter() {
    use crate::runtime::model::http::{ProxyInstallMode, ProxySettings};
    for drop_caller in [false, true] {
        let peer = pending_tcp_bridge().await;
        let direct = ProxySettings {
            url: String::new(),
            no_proxy: None,
            mode: ProxyInstallMode::Reload,
        };
        let channel = SignalChannel::new(
            SignalChannelOptions {
                rest_url: Some(peer.url.into()),
                account: Some(JsValue::from("+10000000000")),
                transport: Some(Arc::new(HyperModelTransport::with_proxy_settings(Some(
                    &direct,
                )))),
                env: Some(Arc::new(|_| None)),
                ..Default::default()
            },
            js_helper_interaction(Arc::new(Responder::default())),
        );
        let handle = channel.start(ChannelStartDeps::default()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), peer.receiving)
            .await
            .unwrap()
            .unwrap();
        let delivery = CronResultDelivery {
            task_id: "t".into(),
            run_id: "r".into(),
            session_key: "signal:chat=dm:+10000000001:general".into(),
            channel_key: ChannelKey::new("signal"),
            origin_session_key: None,
            origin_channel_key: None,
            text: "pending cron".into(),
        };
        let mut caller = Some(channel.deliver_cron_result(&delivery));
        assert!(caller.as_mut().unwrap().as_mut().now_or_never().is_none());
        let raw = tokio::time::timeout(Duration::from_secs(3), peer.posted)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            parse_json(&raw).unwrap().get("message"),
            Some(&JsValue::from("pending cron"))
        );
        if drop_caller {
            drop(caller.take());
        }
        // The remaining caller future is deliberately never polled again
        // before stop completes. Only adapter ownership can close this HTTP.
        let stopped = handle.stop("config-disabled");
        assert!(!channel.0.outbound.lock().unwrap().accepting);
        tokio::time::timeout(Duration::from_secs(3), stopped)
            .await
            .unwrap()
            .unwrap();
        assert!(
            channel.0.outbound.lock().unwrap().tasks.is_empty(),
            "stop joined the actual outbound worker"
        );
        // This is the real peer observing EOF/reset, not a cancellation flag
        // or a fake transport's token. No caller poll is needed to produce it.
        tokio::time::timeout(Duration::from_secs(3), peer.post_eof)
            .await
            .unwrap()
            .unwrap();
        if let Some(caller) = caller {
            assert!(!caller.await);
        }
        tokio::time::timeout(Duration::from_secs(3), peer.server)
            .await
            .unwrap()
            .unwrap();
        assert!(
            !channel.deliver_cron_result(&delivery).await,
            "synchronous stop admission refuses new sends"
        );
    }
}
