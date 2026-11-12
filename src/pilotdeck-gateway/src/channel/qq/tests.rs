use super::*;
use crate::channel::im_interaction::InteractionResponder;
use crate::channel::sms::{ElicitationResponse, PermissionDecision, PermissionDecisionOutcome};
use hyper::Response;
use std::{future::Future, pin::Pin, sync::atomic::AtomicUsize};
use tokio::sync::mpsc;

#[derive(Default, Clone)]
struct Bridge(Arc<Mutex<Vec<(String, JsValue)>>>);
impl ModelHttpTransport for Bridge {
    fn send(&self, request: Request<Full<Bytes>>) -> TransportFuture {
        let state = self.0.clone();
        TransportFuture::io(async move {
            let (parts, body) = request.into_parts();
            let text = body.collect().await.unwrap().to_bytes();
            let value = if text.is_empty() {
                JsValue::Undefined
            } else {
                parse_json(std::str::from_utf8(&text).unwrap()).unwrap()
            };
            let path = parts.uri.path().to_owned();
            let rejected = value
                .get("text")
                .or_else(|| value.get("content"))
                .and_then(JsValue::as_string)
                .is_some_and(|v| v.equals_str("reject"));
            state.lock().unwrap().push((path.clone(), value));
            if rejected {
                return Ok(Response::builder()
                    .status(500)
                    .body(
                        Full::new(Bytes::from_static(
                            br#"{"ok":false,"description":"fixture rejected"}"#,
                        ))
                        .map_err(|never| match never {})
                        .boxed(),
                    )
                    .unwrap());
            }
            let response = if path.ends_with("getMe") {
                r#"{"ok":true,"result":{"username":"fixture"}}"#
            } else if path.contains("getAppAccessToken") {
                r#"{"access_token":"fixture-token","expires_in":"7200"}"#
            } else {
                r#"{"ok":true,"result":true}"#
            };
            Ok(Response::new(
                Full::new(Bytes::from_static(response.as_bytes()))
                    .map_err(|never| match never {})
                    .boxed(),
            ))
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

fn fixture() -> (QQChannel, Bridge, Arc<Gateway>) {
    let bridge = Bridge::default();
    let channel = QQChannel::new(
        QQChannelOptions {
            app_id: Some("fixture-app".into()),
            client_secret: Some("fixture-secret".into()),
            rest_url: Some("http://fixture.invalid".into()),
            token_url: Some("http://fixture.invalid/getAppAccessToken".into()),
            transport: Some(Arc::new(bridge.clone())),
            mapper: Some(QQSessionMapper::new(
                QQSessionMapperState::default(),
                Box::new(|| "fixed".into()),
            )),
            ..Default::default()
        },
        js_helper_interaction(Arc::new(Responder::default())),
    );
    let gateway = Arc::new(Gateway::default());
    channel.0.state.lock().unwrap().gateway = Some(gateway.clone());
    channel.0.running.store(true, Ordering::Release);
    channel.0.outbound.lock().unwrap().accepting = true;
    (channel, bridge, gateway)
}
fn incoming(text: &str, group: bool) -> JsValue {
    object(&[
        ("id", JsValue::from("m1")),
        ("group_openid", JsValue::from("g")),
        (
            "author",
            object(&[(
                if group {
                    "member_openid"
                } else {
                    "user_openid"
                },
                JsValue::from("u"),
            )]),
        ),
        ("content", JsValue::from(text)),
    ])
}

#[tokio::test]
async fn group_trigger_new_and_c2c_new_use_distinct_baseline_semantics() {
    let (channel, bridge, gateway) = fixture();
    channel
        .0
        .clone()
        .parse_message(&incoming("/ask /new", true), true)
        .await;
    let owner = channel.0.clone();
    let task = tokio::spawn(async move {
        owner.parse_message(&incoming("/new", false), false).await;
    });
    until("turn", || gateway.requests.lock().unwrap().len() == 1).await;
    {
        let requests = gateway.requests.lock().unwrap();
        assert_eq!(requests[0].session_key, JsString::from("qq:c2c:u:general"));
        assert_eq!(requests[0].message, JsString::from("/new"));
    }
    gateway.event(0, r#"{"type":"assistant_text_delta","text":"reply"}"#);
    gateway.finish(0);
    task.await.unwrap();
    let requests = bridge.0.lock().unwrap();
    let group = &requests
        .iter()
        .find(|(p, _)| p.contains("/groups/"))
        .unwrap()
        .1;
    assert_eq!(group.get("msg_id"), Some(&JsValue::from("m1")));
    assert_eq!(group.get("msg_seq"), Some(&JsValue::Number(1.0)));
    assert!(
        requests
            .iter()
            .any(|(p, _)| p.contains("/users/u/messages"))
    );
}

#[test]
fn identify_and_chunking_preserve_qq_wire_shape() {
    let identify = wire::identify(&"access".into());
    assert_eq!(identify.get("op"), Some(&JsValue::Number(2.0)));
    let d = identify.get("d").unwrap();
    assert_eq!(d.get("token"), Some(&JsValue::from("QQBot access")));
    assert_eq!(d.get("intents"), Some(&JsValue::Number(33554432.0)));
    let chunks = wire::chunk_text(
        &JsString::from(format!("{}\n\nrest", "x".repeat(1999))),
        2000,
    );
    assert_eq!(
        chunks,
        vec![JsString::from("x".repeat(1999)), JsString::from("\nrest")]
    );
}

#[tokio::test]
async fn disabled_credentials_produce_no_network_exchange() {
    let bridge = Bridge::default();
    let channel = QQChannel::new(
        QQChannelOptions {
            env: Some(Arc::new(|_| None)),
            transport: Some(Arc::new(bridge.clone())),
            ..Default::default()
        },
        js_helper_interaction(Arc::new(Responder::default())),
    );
    let handle = channel
        .start(ChannelStartDeps {
            turn_gateway: None,
            log: None,
            ..Default::default()
        })
        .await
        .unwrap();
    handle.stop("fixture").await.unwrap();
    assert!(bridge.0.lock().unwrap().is_empty());
}

struct GatewayTransport {
    bridge: Bridge,
    gateway_url: String,
}
impl ModelHttpTransport for GatewayTransport {
    fn send(&self, request: Request<Full<Bytes>>) -> TransportFuture {
        if request.uri().path().ends_with("/gateway") {
            let body = stringify_json(&object(&[(
                "url",
                JsValue::String(JsString::from(self.gateway_url.clone())),
            )]))
            .unwrap()
            .unwrap();
            return TransportFuture::ready(Ok(Response::new(
                Full::new(Bytes::from(body))
                    .map_err(|never| match never {})
                    .boxed(),
            )));
        }
        self.bridge.send(request)
    }
}

#[tokio::test]
async fn websocket_identify_heartbeat_resume_invalid_session_and_stop() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let (ready_tx, ready_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
        socket
            .send(Message::Text(
                r#"{"op":10,"d":{"heartbeat_interval":30}}"#.into(),
            ))
            .await
            .unwrap();
        let identify = socket.next().await.unwrap().unwrap().into_text().unwrap();
        assert_eq!(
            parse_json(&identify).unwrap().get("op"),
            Some(&JsValue::Number(2.0))
        );
        socket
            .send(Message::Text(
                r#"{"op":0,"s":12,"t":"READY","d":{"session_id":"fixed-session"}}"#.into(),
            ))
            .await
            .unwrap();
        let heartbeat = socket.next().await.unwrap().unwrap().into_text().unwrap();
        let heartbeat = parse_json(&heartbeat).unwrap();
        assert_eq!(heartbeat.get("op"), Some(&JsValue::Number(1.0)));
        assert_eq!(heartbeat.get("d"), Some(&JsValue::Number(12.0)));
        socket
            .send(Message::Text(r#"{"op":7}"#.into()))
            .await
            .unwrap();
        drop(socket);
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
        socket
            .send(Message::Text(
                r#"{"op":10,"d":{"heartbeat_interval":30}}"#.into(),
            ))
            .await
            .unwrap();
        let resumed = socket.next().await.unwrap().unwrap().into_text().unwrap();
        let resumed = parse_json(&resumed).unwrap();
        assert_eq!(resumed.get("op"), Some(&JsValue::Number(6.0)));
        let data = resumed.get("d").unwrap();
        assert_eq!(
            data.get("session_id"),
            Some(&JsValue::from("fixed-session"))
        );
        assert_eq!(data.get("seq"), Some(&JsValue::Number(12.0)));
        socket
            .send(Message::Text(r#"{"op":9}"#.into()))
            .await
            .unwrap();
        let fresh = socket.next().await.unwrap().unwrap().into_text().unwrap();
        assert_eq!(
            parse_json(&fresh).unwrap().get("op"),
            Some(&JsValue::Number(2.0))
        );
        let _ = ready_tx.send(());
        let closed = socket.next().await.unwrap().unwrap();
        assert!(matches!(closed, Message::Close(_)));
    });
    let channel = QQChannel::new(
        QQChannelOptions {
            app_id: Some("fixture-app".into()),
            client_secret: Some("fixture-secret".into()),
            rest_url: Some("http://fixture.invalid".into()),
            token_url: Some("http://fixture.invalid/getAppAccessToken".into()),
            transport: Some(Arc::new(GatewayTransport {
                bridge: Bridge::default(),
                gateway_url: format!("ws://{address}"),
            })),
            ..Default::default()
        },
        js_helper_interaction(Arc::new(Responder::default())),
    );
    let handle = channel.start(ChannelStartDeps::default()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(12), ready_rx)
        .await
        .unwrap()
        .unwrap();
    handle.stop("fixture").await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn send_failure_stops_chunk_delivery_and_token_is_cached() {
    let (channel, bridge, _) = fixture();
    assert!(
        !channel
            .0
            .send_reply_owned(
                &"groups/g".into(),
                &"reject".into(),
                Some(JsValue::from("m1")),
                true
            )
            .await
    );
    assert!(
        channel
            .0
            .send_reply_owned(
                &"groups/g".into(),
                &"ok".into(),
                Some(JsValue::from("m2")),
                true
            )
            .await
    );
    assert_eq!(
        bridge
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p.contains("getAppAccessToken"))
            .count(),
        1
    );
}

#[test]
fn saved_mapper_roundtrip_preserves_utf16_keys_and_property_order() {
    let value = parse_json(
        r#"{"activeByChatKey":{"g:z":"qq:last","g:a":"qq:first","\ud800:u":"qq:surrogate"}}"#,
    )
    .unwrap();
    let state = QQSessionMapperState::from_js(&value);
    assert_eq!(
        stringify_json(&state.to_js()).unwrap(),
        stringify_json(&value).unwrap()
    );
    let mut mapper = QQSessionMapper::new(state, Box::new(|| "fixed".into()));
    assert_eq!(
        mapper
            .resolve(&"g".into(), &"a".into(), &"hello".into())
            .session_key,
        JsString::from("qq:first")
    );
    assert!(
        mapper
            .resolve(&"g".into(), &"z".into(), &"/new".into())
            .command_new
    );
    let keys = mapper
        .snapshot()
        .active_by_chat_key
        .entries()
        .iter()
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    assert_eq!(keys[0], JsString::from("g:z"));
    assert_eq!(keys[1], JsString::from("g:a"));
}

#[derive(Clone, Default)]
struct WireProbe {
    bridge: Bridge,
    headers: Arc<Mutex<Vec<(String, HeaderMap)>>>,
}
impl ModelHttpTransport for WireProbe {
    fn send(&self, request: Request<Full<Bytes>>) -> TransportFuture {
        self.headers
            .lock()
            .unwrap()
            .push((request.uri().path().to_owned(), request.headers().clone()));
        if request.uri().path().ends_with("/messages") {
            // A successful QQ send ignores the response body entirely. A
            // never-ending body catches accidental eager JSON/text reading.
            use crate::runtime::model::http::BodyFailure;
            use hyper::body::Frame;
            let stream = futures_util::stream::pending::<Result<Frame<Bytes>, BodyFailure>>();
            return TransportFuture::ready(Ok(Response::new(BodyExt::boxed(
                http_body_util::StreamBody::new(stream),
            ))));
        }
        self.bridge.send(request)
    }
}

#[tokio::test]
async fn gateway_get_has_no_content_type_and_successful_send_drops_unread_body() {
    let probe = WireProbe::default();
    let channel = QQChannel::new(
        QQChannelOptions {
            app_id: Some("fixture-app".into()),
            client_secret: Some("fixture-secret".into()),
            rest_url: Some("http://fixture.invalid".into()),
            token_url: Some("http://fixture.invalid/getAppAccessToken".into()),
            transport: Some(Arc::new(probe.clone())),
            ..Default::default()
        },
        js_helper_interaction(Arc::new(Responder::default())),
    );
    channel.0.running.store(true, Ordering::Release);
    channel
        .0
        .json_request(Method::GET, "http://fixture.invalid/gateway", None, true)
        .await
        .unwrap();
    let delivered = tokio::time::timeout(
        Duration::from_secs(1),
        channel
            .0
            .send_reply_owned(&"groups/g".into(), &"reply".into(), None, true),
    )
    .await
    .unwrap();
    assert!(delivered);
    let requests = probe.headers.lock().unwrap();
    let gateway = &requests
        .iter()
        .find(|(path, _)| path == "/gateway")
        .unwrap()
        .1;
    assert!(!gateway.contains_key("content-type"));
    assert_eq!(gateway.get("authorization").unwrap(), "QQBot fixture-token");
    let send = &requests
        .iter()
        .find(|(path, _)| path.ends_with("/messages"))
        .unwrap()
        .1;
    assert_eq!(send.get("content-type").unwrap(), "application/json");
}

#[test]
fn nonpositive_message_limit_cannot_hang_chunking() {
    assert_eq!(
        configured_message_limit(Some(&JsValue::Number(-1.0))),
        Some(1)
    );
    assert_eq!(
        configured_message_limit(Some(&JsValue::Number(0.0))),
        Some(1)
    );
    assert_eq!(
        configured_message_limit(Some(&JsValue::Number(2000.0))),
        Some(2000)
    );
    assert_eq!(
        wire::chunk_text(&"abc".into(), 0),
        vec![
            JsString::from("a"),
            JsString::from("b"),
            JsString::from("c")
        ]
    );
    let channel = QQChannel::new(
        QQChannelOptions {
            max_message_length: Some(0),
            env: Some(Arc::new(|_| None)),
            ..Default::default()
        },
        js_helper_interaction(Arc::new(Responder::default())),
    );
    assert_eq!(channel.0.max_message_length, 1);
}
