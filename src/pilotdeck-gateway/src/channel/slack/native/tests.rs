use super::*;
use crate::channel::im_interaction::InteractionResponder;
use crate::channel::sms::{ElicitationResponse, PermissionDecision, PermissionDecisionOutcome};
use std::collections::VecDeque;
use std::sync::atomic::AtomicUsize;
use tokio::sync::mpsc;

type Exchange = (String, HeaderMap, JsValue);
struct Rest {
    websocket: String,
    requests: Mutex<Vec<Exchange>>,
    post_results: Mutex<VecDeque<bool>>,
}
impl ModelHttpTransport for Rest {
    fn send(&self, request: Request<Full<Bytes>>) -> TransportFuture {
        let (parts, body) = request.into_parts();
        let body = body.collect().now_or_never().unwrap().unwrap().to_bytes();
        let path = parts.uri.path().to_owned();
        self.requests.lock().unwrap().push((
            path.clone(),
            parts.headers,
            parse_json(std::str::from_utf8(&body).unwrap()).unwrap(),
        ));
        let response = if path == "/api/apps.connections.open" {
            serde_json::json!({"ok":true,"url":self.websocket})
        } else if path == "/api/auth.test" {
            serde_json::json!({"ok":true,"user_id":"bot","user":"fixture"})
        } else if path == "/api/chat.postMessage" {
            if self
                .post_results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(true)
            {
                serde_json::json!({"ok":true})
            } else {
                serde_json::json!({"ok":false,"error":"fixture_post_failure"})
            }
        } else {
            panic!("unexpected production API path {path}");
        };
        TransportFuture::ready(Ok(Response::new(
            Full::new(Bytes::from(response.to_string()))
                .map_err(|never| match never {})
                .boxed(),
        )))
    }
}
#[derive(Default)]
struct Responder {
    elicitation: Mutex<Vec<ElicitationResponse>>,
    permission: Mutex<Vec<PermissionDecision>>,
}
impl InteractionResponder for Responder {
    fn respond_elicitation(
        &self,
        response: ElicitationResponse,
    ) -> BoxFuture<Result<(), ChannelError>> {
        self.elicitation.lock().unwrap().push(response);
        Box::pin(async { Ok(()) })
    }
    fn permission_decide(
        &self,
        decision: PermissionDecision,
    ) -> BoxFuture<Result<PermissionDecisionOutcome, ChannelError>> {
        self.permission.lock().unwrap().push(decision);
        Box::pin(async { Ok(PermissionDecisionOutcome { delivered: true }) })
    }
}
#[derive(Default)]
struct Gateway {
    requests: Mutex<Vec<TurnRequest>>,
    senders: Mutex<Vec<mpsc::UnboundedSender<Option<JsValue>>>>,
    cancelled: Arc<AtomicUsize>,
    fail_cancel: Arc<AtomicBool>,
    cancel_gate: Mutex<Option<watch::Receiver<bool>>>,
}
struct Events {
    events: mpsc::UnboundedReceiver<Option<JsValue>>,
    cancelled: Arc<AtomicUsize>,
    fail_cancel: Arc<AtomicBool>,
    cancel_gate: Option<watch::Receiver<bool>>,
}
impl TurnEventStream for Events {
    fn next(
        &mut self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<JsValue>, ChannelError>> + Send + '_>,
    > {
        Box::pin(async move { Ok(self.events.recv().await.flatten()) })
    }
    fn cancel_and_drain(
        &mut self,
        _: JsString,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ChannelError>> + Send + '_>>
    {
        Box::pin(async move {
            self.cancelled.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &mut self.cancel_gate {
                while !*gate.borrow() {
                    gate.changed()
                        .await
                        .map_err(|_| ChannelError::error("fixture gate closed"))?;
                }
            }
            if self.fail_cancel.swap(false, Ordering::SeqCst) {
                Err(ChannelError::error("fixture cancellation failure"))
            } else {
                Ok(())
            }
        })
    }
}
impl TurnGateway for Gateway {
    fn submit(
        &self,
        request: TurnRequest,
    ) -> BoxFuture<Result<Box<dyn TurnEventStream>, ChannelError>> {
        self.requests.lock().unwrap().push(request);
        let (sender, events) = mpsc::unbounded_channel();
        self.senders.lock().unwrap().push(sender);
        let stream = Events {
            events,
            cancelled: self.cancelled.clone(),
            fail_cancel: self.fail_cancel.clone(),
            cancel_gate: self.cancel_gate.lock().unwrap().clone(),
        };
        Box::pin(async { Ok(Box::new(stream) as Box<dyn TurnEventStream>) })
    }
}
enum Command {
    Packet(String),
    Disconnect,
}
struct Fixture {
    channel: NativeSlackChannel,
    handle: Arc<dyn ChannelHandle>,
    rest: Arc<Rest>,
    gateway: Arc<Gateway>,
    responder: Arc<Responder>,
    command: mpsc::UnboundedSender<Command>,
    acks: Arc<Mutex<Vec<JsValue>>>,
    connections: Arc<AtomicUsize>,
    peer: JoinHandle<()>,
}
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(4), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Slack protocol condition");
}
async fn fixture() -> Fixture {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let websocket = format!("ws://{}/socket", listener.local_addr().unwrap());
    let (command, mut commands) = mpsc::unbounded_channel();
    let acks = Arc::new(Mutex::new(Vec::new()));
    let captured = acks.clone();
    let connections = Arc::new(AtomicUsize::new(0));
    let count = connections.clone();
    let peer = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            count.fetch_add(1, Ordering::SeqCst);
            ws.send(Message::Text(
                r#"{"type":"hello","num_connections":1}"#.into(),
            ))
            .await
            .unwrap();
            loop {
                tokio::select! {
                    command = commands.recv() => match command {
                        Some(Command::Packet(packet)) => ws.send(Message::Text(packet.into())).await.unwrap(),
                        Some(Command::Disconnect) => { ws.send(Message::Text(r#"{"type":"disconnect","reason":"refresh_requested"}"#.into())).await.unwrap(); break; },
                        None => return,
                    },
                    frame = ws.next() => match frame {
                        Some(Ok(Message::Text(text))) => captured.lock().unwrap().push(parse_json(text.as_str()).unwrap()),
                        Some(Ok(Message::Close(_))) | None => return,
                        Some(Err(_)) => return,
                        _ => {},
                    }
                }
            }
        }
    });
    let rest = Arc::new(Rest {
        websocket,
        requests: Mutex::default(),
        post_results: Mutex::default(),
    });
    let gateway = Arc::new(Gateway::default());
    let responder = Arc::new(Responder::default());
    let channel = NativeSlackChannel::new(
        NativeSlackChannelOptions {
            bot_token: Some(JsValue::from("xoxb-fixture")),
            app_token: Some(JsValue::from("xapp-fixture")),
            transport: Some(rest.clone()),
            api_base: Some("http://mock.invalid/api".into()),
            env: Some(Arc::new(|_| None)),
            ..Default::default()
        },
        js_helper_interaction(responder.clone()),
    );
    let handle = channel
        .start(ChannelStartDeps {
            turn_gateway: Some(gateway.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    Fixture {
        channel,
        handle,
        rest,
        gateway,
        responder,
        command,
        acks,
        connections,
        peer,
    }
}
impl Fixture {
    fn inbound(&self, id: &str, text: &str, thread: Option<&str>) {
        let event = serde_json::json!({"type":"message","user":"human","channel":"C1","text":text,"thread_ts":thread,"ts":"200"});
        self.command.send(Command::Packet(serde_json::json!({"type":"events_api","envelope_id":id,"payload":{"type":"event_callback","event":event}}).to_string())).unwrap();
    }
    fn posted(&self) -> Vec<JsValue> {
        self.rest
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _, _)| path.ends_with("chat.postMessage"))
            .map(|(_, _, body)| body.clone())
            .collect()
    }
    async fn stop(self) {
        self.handle.stop("fixture complete").await.unwrap();
        self.peer.await.unwrap();
    }
}

#[test]
fn slack_native_message_preserves_filters_mentions_and_root_thread_contract() {
    let own = JsString::from("bot");
    for raw in [
        r#"{"channel":"C1","user":"bot","text":"self"}"#,
        r#"{"channel":"C1","bot_id":"B1","text":"bot"}"#,
        r#"{"channel":"C1","subtype":"message_changed","text":"edit"}"#,
    ] {
        assert!(message(&parse_json(raw).unwrap(), Some(&own)).is_none());
    }
    let (context, chat, text) = message(
        &parse_json(r#"{"channel":"C1","text":" hi <@U1> ","thread_ts":"100","ts":"200"}"#)
            .unwrap(),
        None,
    )
    .unwrap();
    assert_eq!(chat, JsString::from("C1:100"));
    assert_eq!(context.helper_chat(), chat);
    assert_eq!(text, JsString::from("hi"));
    let (context, chat, _) = message(
        &parse_json(r#"{"channel":"C1","text":"hi","thread_ts":"100","ts":"100"}"#).unwrap(),
        None,
    )
    .unwrap();
    assert_eq!(chat, JsString::from("C1"));
    assert_eq!(context.helper_chat(), JsString::from("C1:100"));
}

#[tokio::test]
async fn slack_native_socket_ack_submits_real_turn_and_sends_thread_reply() {
    let f = fixture().await;
    f.inbound("envelope-1", "hello <@U1>", Some("100"));
    until(|| f.gateway.requests.lock().unwrap().len() == 1 && f.acks.lock().unwrap().len() == 1)
        .await;
    let requests = f.gateway.requests.lock().unwrap();
    assert_eq!(
        requests[0].session_key,
        JsString::from("slack:chat=C1:100:general")
    );
    assert_eq!(requests[0].message, JsString::from("hello"));
    assert_eq!(requests[0].channel_key, ChannelKey::new("slack"));
    drop(requests);
    f.gateway.senders.lock().unwrap()[0]
        .send(Some(
            parse_json(r#"{"type":"assistant_text_delta","text":" answer **bold** "}"#).unwrap(),
        ))
        .unwrap();
    f.gateway.senders.lock().unwrap()[0].send(None).unwrap();
    until(|| f.posted().len() == 1).await;
    assert_eq!(
        f.acks.lock().unwrap()[0].get("envelope_id"),
        Some(&JsValue::from("envelope-1"))
    );
    let posted = f.posted();
    assert_eq!(posted[0].get("thread_ts"), Some(&JsValue::from("100")));
    assert_eq!(posted[0].get("text"), Some(&JsValue::from("answer *bold*")));
    let requests = f.rest.requests.lock().unwrap();
    assert_eq!(requests[0].0, "/api/apps.connections.open");
    assert_eq!(
        requests[0].1.get("authorization").unwrap(),
        "Bearer xapp-fixture"
    );
    assert!(
        requests
            .iter()
            .skip(1)
            .all(|(_, headers, _)| headers.get("authorization").unwrap() == "Bearer xoxb-fixture")
    );
    drop(requests);
    f.stop().await;
}

#[tokio::test]
async fn slack_native_disconnect_reopens_endpoint_and_reconnects() {
    let f = fixture().await;
    f.command.send(Command::Disconnect).unwrap();
    until(|| f.connections.load(Ordering::SeqCst) == 2).await;
    assert_eq!(
        f.rest
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _, _)| path.ends_with("apps.connections.open"))
            .count(),
        2
    );
    f.inbound("after-reconnect", "/new", None);
    until(|| f.posted().len() == 1 && f.acks.lock().unwrap().len() == 1).await;
    assert_eq!(
        f.posted()[0].get("text"),
        Some(&JsValue::from("已创建新会话。"))
    );
    f.stop().await;
}

#[tokio::test]
async fn slack_native_permission_answer_reaches_shared_responder_before_busy_gate() {
    let f = fixture().await;
    f.inbound("turn", "hello", Some("100"));
    until(|| f.gateway.requests.lock().unwrap().len() == 1).await;
    f.gateway.senders.lock().unwrap()[0].send(Some(parse_json(r#"{"type":"permission_request","requestId":"p1","toolName":"bash","toolInput":{"command":"echo fixture"}}"#).unwrap())).unwrap();
    until(|| f.channel.0.interaction.permission_pending(&"C1:100".into()) && f.posted().len() == 1)
        .await;
    f.inbound("answer", "1", Some("100"));
    until(|| f.responder.permission.lock().unwrap().len() == 1).await;
    assert_eq!(f.gateway.requests.lock().unwrap().len(), 1);
    assert_eq!(
        f.responder.permission.lock().unwrap()[0].request_id,
        Some("p1".into())
    );
    f.stop().await;
}

#[tokio::test]
async fn slack_native_failed_owned_cancel_retains_stream_and_helper_for_retry() {
    let f = fixture().await;
    f.inbound("turn", "hello", None);
    until(|| f.gateway.requests.lock().unwrap().len() == 1).await;
    f.gateway.senders.lock().unwrap()[0]
        .send(Some(
            parse_json(r#"{"type":"permission_request","requestId":"p1","toolName":"bash"}"#)
                .unwrap(),
        ))
        .unwrap();
    until(|| f.channel.0.interaction.permission_pending(&"C1".into()) && f.posted().len() == 1)
        .await;
    f.gateway.fail_cancel.store(true, Ordering::SeqCst);
    assert!(f.handle.stop("first failure").await.is_err());
    assert_eq!(f.channel.0.failed_streams.lock().unwrap().len(), 1);
    assert!(f.channel.0.interaction.permission_pending(&"C1".into()));
    f.handle.stop("retry").await.unwrap();
    assert!(!f.channel.0.interaction.permission_pending(&"C1".into()));
    assert_eq!(f.gateway.cancelled.load(Ordering::SeqCst), 2);
    f.peer.await.unwrap();
}

#[tokio::test]
async fn slack_native_dropped_stop_waiter_keeps_owned_cleanup_until_real_drain() {
    let f = fixture().await;
    let (gate, rx) = watch::channel(false);
    *f.gateway.cancel_gate.lock().unwrap() = Some(rx);
    f.inbound("turn", "held", None);
    until(|| f.gateway.requests.lock().unwrap().len() == 1).await;
    drop(f.handle.stop("dropped waiter"));
    until(|| f.gateway.cancelled.load(Ordering::SeqCst) == 1).await;
    let mut receipt = f.handle.stop("second waiter");
    assert!(receipt.as_mut().now_or_never().is_none());
    gate.send_replace(true);
    receipt.await.unwrap();
    let context = SendContext {
        channel: "C1".into(),
        thread: JsValue::Undefined,
    };
    assert!(!f.channel.0.send_reply(&context, &"late".into()).await);
    f.peer.await.unwrap();
}

struct HeldAuth(Arc<Rest>);
impl ModelHttpTransport for HeldAuth {
    fn send(&self, request: Request<Full<Bytes>>) -> TransportFuture {
        if request.uri().path().ends_with("/auth.test") {
            TransportFuture::io(async { std::future::pending().await })
        } else {
            self.0.send(request)
        }
    }
}
#[tokio::test]
async fn slack_native_dropping_start_during_auth_releases_open_receiver() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let websocket = format!("ws://{}/socket", listener.local_addr().unwrap());
    let peer = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
        while let Some(frame) = ws.next().await {
            if matches!(frame, Ok(Message::Close(_)) | Err(_)) {
                return;
            }
        }
    });
    let rest = Arc::new(Rest {
        websocket,
        requests: Mutex::default(),
        post_results: Mutex::default(),
    });
    let channel = NativeSlackChannel::new(
        NativeSlackChannelOptions {
            bot_token: Some(JsValue::from("xoxb-fixture")),
            app_token: Some(JsValue::from("xapp-fixture")),
            transport: Some(Arc::new(HeldAuth(rest))),
            api_base: Some("http://mock.invalid/api".into()),
            env: Some(Arc::new(|_| None)),
            ..Default::default()
        },
        js_helper_interaction(Arc::new(Responder::default())),
    );
    let started = channel.start(Default::default());
    let task = tokio::spawn(started);
    until(|| channel.0.receiver.lock().unwrap().is_some()).await;
    task.abort();
    match task.await {
        Err(error) => assert!(error.is_cancelled()),
        Ok(_) => panic!("startup should be cancelled"),
    }
    until(|| {
        channel
            .0
            .cleanup
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|attempt| {
                attempt
                    .result
                    .borrow()
                    .as_ref()
                    .is_some_and(|outcome| outcome.result.is_ok())
            })
    })
    .await;
    assert!(!channel.0.running.load(Ordering::Acquire));
    assert!(channel.0.receiver.lock().unwrap().is_none());
    peer.await.unwrap();
}

struct HeldPosts {
    posted: Arc<AtomicBool>,
    released: Arc<AtomicBool>,
}
struct Release(Arc<AtomicBool>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
impl ModelHttpTransport for HeldPosts {
    fn send(&self, _: Request<Full<Bytes>>) -> TransportFuture {
        let posted = self.posted.clone();
        let released = self.released.clone();
        TransportFuture::io(async move {
            let _release = Release(released);
            posted.store(true, Ordering::SeqCst);
            std::future::pending().await
        })
    }
}
#[tokio::test]
async fn slack_native_dropped_outbound_receipt_is_still_cancelled_and_drained() {
    let posted = Arc::new(AtomicBool::new(false));
    let released = Arc::new(AtomicBool::new(false));
    let channel = NativeSlackChannel::new(
        NativeSlackChannelOptions {
            transport: Some(Arc::new(HeldPosts {
                posted: posted.clone(),
                released: released.clone(),
            })),
            env: Some(Arc::new(|_| None)),
            ..Default::default()
        },
        js_helper_interaction(Arc::new(Responder::default())),
    );
    channel.0.running.store(true, Ordering::Release);
    channel.0.outbound.lock().unwrap().accepting = true;
    let shared = channel.0.clone();
    let waiter = tokio::spawn(async move {
        shared
            .send_reply(
                &SendContext {
                    channel: "C1".into(),
                    thread: JsValue::Undefined,
                },
                &"held".into(),
            )
            .await
    });
    until(|| posted.load(Ordering::SeqCst)).await;
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    assert!(!released.load(Ordering::SeqCst));
    Handle(channel.0.clone())
        .stop("fixture stop")
        .await
        .unwrap();
    assert!(released.load(Ordering::SeqCst));
    assert!(channel.0.outbound.lock().unwrap().tasks.is_empty());
}
