use super::*;
use crate::channel::{
    im_interaction::InteractionResponder,
    protocol::{TurnEventStream as HostTurnEventStream, TurnGateway, TurnRequest},
};
use pilotdeck_protocol::{parse_json, stringify_json};
use std::sync::Mutex;
use std::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Duration,
};
use tokio::sync::mpsc;
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
impl HostTurnEventStream for Events {
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
    ) -> BoxFuture<Result<Box<dyn HostTurnEventStream>, ChannelError>> {
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
            }) as Box<dyn HostTurnEventStream>)
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

struct Provider {
    url: String,
    requests: Arc<StdMutex<Vec<(String, hyper::HeaderMap, String)>>>,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}
impl Provider {
    async fn new(status: u16) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let captured = requests.clone();
        let (stop, mut rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                let accepted =
                    tokio::select! {biased; _=rx.changed()=>break,result=listener.accept()=>result};
                let (socket, _) = accepted.unwrap();
                let captured = captured.clone();
                connections.spawn(async move {
                    let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                        let captured = captured.clone();
                        async move {
                            let (parts, body) = request.into_parts();
                            let body = body.collect().await.unwrap().to_bytes();
                            captured.lock().unwrap().push((
                                parts.uri.to_string(),
                                parts.headers,
                                String::from_utf8(body.to_vec()).unwrap(),
                            ));
                            Ok::<_, std::convert::Infallible>(
                                Response::builder()
                                    .status(status)
                                    .header("connection", "close")
                                    .body(Full::new(Bytes::from_static(if status == 201 {
                                        br#"{"sid":"SMfixture"}"#
                                    } else {
                                        br#"{"message":"fixture provider rejected","code":21211}"#
                                    })))
                                    .unwrap(),
                            )
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(socket), service)
                        .await;
                });
            }
            while connections.join_next().await.is_some() {}
        });
        Self {
            url,
            requests,
            stop,
            task,
        }
    }
    async fn stop(self) {
        self.stop.send_replace(true);
        self.task.await.unwrap();
    }
}
async fn fixture(
    provider: &Provider,
) -> (
    SmsChannel,
    Arc<dyn ChannelHandle>,
    Arc<Gateway>,
    Arc<Responder>,
    u16,
) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let extra=parse_json(&format!(r#"{{"accountSid":"ACfixture","authToken":"fixture-token","phoneNumber":"+10000000000","webhookPort":{port}}}"#)).unwrap().as_object().unwrap().clone();
    let responder = Arc::new(Responder::default());
    let channel = SmsChannel::new(SmsChannelOptions {
        extra: Some(extra),
        mapper: None,
        gateway: None,
        env: None,
        twilio_factory: Some(sender::native_factory_at(
            Arc::new(crate::runtime::model::http::HyperModelTransport::new()),
            provider.url.clone(),
        )),
        record_unhandled: None,
        request_timeout: None,
    })
    .with_native(responder.clone());
    let gateway = Arc::new(Gateway::default());
    let handle = channel
        .start(ChannelStartDeps {
            turn_gateway: Some(gateway.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    (channel, handle, gateway, responder, port)
}
async fn webhook(port: u16, text: &str) -> serde_json::Value {
    let mut params = JsObject::new();
    params.insert("From".into(), JsValue::from("+10000000001"));
    params.insert("Body".into(), JsValue::String(JsString::from(text)));
    let body = stringify_json(&JsValue::Object(params)).unwrap().unwrap();
    let request = format!(
        "POST /sms/incoming HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    raw_request(port, request.as_bytes(), false).await.unwrap()
}
#[tokio::test]
async fn native_webhook_turn_and_provider_rest_are_connected() {
    let provider = Provider::new(201).await;
    let (_channel, handle, gateway, _responder, port) = fixture(&provider).await;
    let response = webhook(port, "hello").await;
    assert_eq!(response["status"], 200);
    until("native SMS turn", || {
        gateway.requests.lock().unwrap().len() == 1
    })
    .await;
    assert_eq!(
        gateway.requests.lock().unwrap()[0].channel_key,
        ChannelKey::new("sms")
    );
    gateway.event(
        0,
        r#"{"type":"assistant_text_delta","text":"中文 reply + space"}"#,
    );
    gateway.finish(0);
    until("provider request", || {
        provider.requests.lock().unwrap().len() == 1
    })
    .await;
    {
        let requests = provider.requests.lock().unwrap();
        let (path, headers, body) = &requests[0];
        assert_eq!(path, "/2010-04-01/Accounts/ACfixture/Messages.json");
        assert_eq!(
            headers.get("authorization").unwrap(),
            "Basic QUNmaXh0dXJlOmZpeHR1cmUtdG9rZW4="
        );
        assert_eq!(
            headers.get("content-type").unwrap(),
            "application/x-www-form-urlencoded"
        );
        assert_eq!(
            url::form_urlencoded::parse(body.as_bytes()).collect::<Vec<_>>(),
            vec![
                ("Body".into(), "中文 reply + space".into()),
                ("From".into(), "+10000000000".into()),
                ("To".into(), "+10000000001".into())
            ]
        );
    }
    handle.stop("fixture").await.unwrap();
    provider.stop().await;
}
#[tokio::test]
async fn native_stop_owns_cancel_receipt_and_retries_failed_drain() {
    let provider = Provider::new(201).await;
    let (channel, handle, gateway, _, port) = fixture(&provider).await;
    webhook(port, "hold").await;
    until("held native turn", || {
        gateway.requests.lock().unwrap().len() == 1
    })
    .await;
    gateway.fail_cancel.store(true, Ordering::Release);
    assert!(handle.stop("fixture").await.is_err());
    assert_eq!(gateway.cancelled.load(Ordering::Acquire), 1);
    let receipt = handle.stop("retry");
    drop(receipt);
    until("retry drain", || {
        gateway.cancelled.load(Ordering::Acquire) == 2
    })
    .await;
    handle.stop("observe").await.unwrap();
    assert!(!send_reply(&channel.shared, &"+10000000001".into(), "late".into()).await);
    assert!(provider.requests.lock().unwrap().is_empty());
    provider.stop().await;
}
#[tokio::test]
async fn native_sender_surfaces_provider_failure() {
    let provider = Provider::new(400).await;
    let factory = sender::native_factory_at(
        Arc::new(crate::runtime::model::http::HyperModelTransport::new()),
        provider.url.clone(),
    );
    let sender = factory(&"ACfixture".into(), &"fixture-token".into()).unwrap();
    let error = sender
        .send(SmsOutgoingMessage {
            body: "reply".into(),
            from: "+10000000000".into(),
            to: "+10000000001".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.to_js_string(), "Error: fixture provider rejected");
    provider.stop().await;
}

#[tokio::test]
async fn native_permission_reply_uses_host_responder_while_turn_is_open() {
    let provider = Provider::new(201).await;
    let (_channel, handle, gateway, responder, port) = fixture(&provider).await;
    webhook(port, "tool").await;
    until("turn", || gateway.requests.lock().unwrap().len() == 1).await;
    gateway.event(0,r#"{"type":"permission_request","requestId":"p1","toolName":"Bash","payload":{"command":"ls"}}"#);
    until("permission prompt", || {
        provider.requests.lock().unwrap().len() == 1
    })
    .await;
    webhook(port, "1").await;
    until("host decision", || {
        responder.permissions.lock().unwrap().len() == 1
    })
    .await;
    assert_eq!(
        responder.permissions.lock().unwrap()[0].get("decision"),
        Some(&JsValue::from("allow"))
    );
    assert_eq!(gateway.requests.lock().unwrap().len(), 1);
    until("permission confirmation", || {
        provider.requests.lock().unwrap().len() == 2
    })
    .await;
    gateway.finish(0);
    handle.stop("fixture").await.unwrap();
    provider.stop().await;
}
struct PendingSender {
    entered: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
}
struct DropFlag(Arc<AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
impl SmsSender for PendingSender {
    fn send(&self, _message: SmsOutgoingMessage) -> BoxFuture<Result<(), ChannelError>> {
        let entered = self.entered.clone();
        let dropped = self.dropped.clone();
        Box::pin(async move {
            let _guard = DropFlag(dropped);
            entered.store(true, Ordering::Release);
            std::future::pending().await
        })
    }
}
#[tokio::test]
async fn dropped_outbound_waiter_does_not_orphan_exchange() {
    let provider = Provider::new(201).await;
    let (channel, handle, _, _, _) = fixture(&provider).await;
    let entered = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    channel.shared.runtime.lock().await.client = Some(Arc::new(PendingSender {
        entered: entered.clone(),
        dropped: dropped.clone(),
    }));
    let shared = channel.shared.clone();
    let waiter =
        tokio::spawn(
            async move { send_reply(&shared, &"+10000000001".into(), "pending".into()).await },
        );
    until("outbound exchange", || entered.load(Ordering::Acquire)).await;
    waiter.abort();
    let _ = waiter.await;
    handle.stop("fixture").await.unwrap();
    assert!(dropped.load(Ordering::Acquire));
    provider.stop().await;
}
