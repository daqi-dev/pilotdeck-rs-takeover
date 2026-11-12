use super::*;
use crate::channel::im_interaction::InteractionResponder;
use crate::channel::sms::{ElicitationResponse, PermissionDecision, PermissionDecisionOutcome};
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

async fn fixture() -> (
    TelegramChannel,
    Arc<dyn ChannelHandle>,
    Bridge,
    Arc<Gateway>,
) {
    let bridge = Bridge::default();
    let channel = TelegramChannel::new(
        TelegramChannelOptions {
            token: Some("fixture".into()),
            webhook_url: Some("https://fixture.invalid/webhook".into()),
            rest_url: Some("http://fixture.invalid".into()),
            transport: Some(Arc::new(bridge.clone())),
            mapper: Some(TelegramSessionMapper::new(
                TelegramSessionMapperState::default(),
                Box::new(|| "fixed".into()),
            )),
            ..Default::default()
        },
        js_helper_interaction(Arc::new(Responder::default())),
    );
    let gateway = Arc::new(Gateway::default());
    let handle = channel
        .start(ChannelStartDeps {
            turn_gateway: Some(gateway.clone()),
            log: None,
            ..Default::default()
        })
        .await
        .unwrap();
    (channel, handle, bridge, gateway)
}
fn incoming(text: &str) -> JsValue {
    object(&[(
        "message",
        object(&[
            ("chat", object(&[("id", JsValue::Number(42.0))])),
            ("text", JsValue::from(text)),
        ]),
    )])
}

#[tokio::test]
async fn new_session_final_reply_and_stop_match_characterized_class() {
    let (channel, handle, bridge, gateway) = fixture().await;
    channel.0.clone().parse_update(&incoming("/new")).await;
    let owner = channel.0.clone();
    let task = tokio::spawn(async move {
        owner.parse_update(&incoming("hello")).await;
    });
    until("turn", || gateway.requests.lock().unwrap().len() == 1).await;
    assert_eq!(
        gateway.requests.lock().unwrap()[0].session_key,
        JsString::from("telegram:chat=42:s_fixed")
    );
    gateway.event(0, r#"{"type":"assistant_text_delta","text":"  reply  "}"#);
    gateway.finish(0);
    task.await.unwrap();
    let sent: Vec<_> = bridge
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|(p, _)| p.ends_with("sendMessage"))
        .map(|(_, v)| v.get("text").unwrap().clone())
        .collect();
    assert_eq!(
        sent,
        vec![JsValue::from("已创建新会话。"), JsValue::from("reply")]
    );
    handle.stop("fixture").await.unwrap();
    assert!(!channel.0.send_reply(&"42".into(), &"after".into()).await);
}

#[tokio::test]
async fn stop_retains_and_retries_failed_owned_turn_cleanup() {
    let (channel, handle, _, gateway) = fixture().await;
    let owner = channel.0.clone();
    let task = tokio::spawn(async move {
        owner.parse_update(&incoming("hello")).await;
    });
    channel.0.handlers.lock().unwrap().push(task);
    until("turn", || gateway.requests.lock().unwrap().len() == 1).await;
    gateway.fail_cancel.store(true, Ordering::Release);
    assert!(handle.stop("fixture").await.is_err());
    assert_eq!(gateway.cancelled.load(Ordering::Acquire), 1);
    handle.stop("retry").await.unwrap();
    assert_eq!(gateway.cancelled.load(Ordering::Acquire), 2);
}

#[test]
fn chunks_use_utf16_and_consume_all_boundary_newlines() {
    let chunks = wire::chunk_text(&JsString::from(format!("{}\n\nrest", "x".repeat(4095))));
    assert_eq!(
        chunks,
        vec![
            JsString::from(format!("{}\n", "x".repeat(4095))),
            JsString::from("rest")
        ]
    );
    let chunks = wire::chunk_text(&JsString::from(format!("{}😀", "x".repeat(4095))));
    assert_eq!(chunks[0].units().len(), 4096);
    assert_eq!(chunks[1].units(), &[0xde00]);
}

#[tokio::test]
async fn send_api_failure_is_reported_as_not_delivered() {
    let (channel, handle, _, _) = fixture().await;
    assert!(!channel.0.send_reply(&"42".into(), &"reject".into()).await);
    handle.stop("fixture").await.unwrap();
}
