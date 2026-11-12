use super::*;

#[test]
fn mapper_new_and_unicode_use_utf16_keys() {
    let mut mapper = DingTalkSessionMapper::new(Default::default(), Box::new(|| "fixed".into()));
    let chat = JsString::from_utf16(vec![0xd800]);
    let first = mapper.resolve(&chat, &" hello ".into());
    assert_eq!(first.message, JsString::from("hello"));
    assert!(first.session_key.units().contains(&0xd800));
    let new = mapper.resolve(&chat, &"/new".into());
    assert!(new.command_new);
    assert!(new.message.is_empty());
    assert_eq!(
        mapper.resolve(&chat, &"follow".into()).session_key,
        new.session_key
    );
    assert!(!mapper.resolve(&chat, &"/new\tmessage".into()).command_new);
}
#[test]
fn inbound_extraction_and_webhook_origin_preserve_baseline() {
    for (raw, expected) in [
        (r#"{"text":{"content":" hello "}}"#, "hello"),
        (r#"{"text":" text "}"#, "text"),
        (
            r#"{"richText":[{"text":"rich"},{"text":"text"},null,3]}"#,
            "rich text",
        ),
        (
            r#"{"text":{},"rich_text":[{"text":false},{"text":0}]}"#,
            "false 0",
        ),
    ] {
        assert_eq!(
            wire::text(&parse_json(raw).unwrap()),
            JsString::from(expected)
        );
    }
    assert!(wire::trusted_webhook(
        &"https://api.dingtalk.com/robot/send".into()
    ));
    for rejected in [
        "http://api.dingtalk.com/robot/send",
        "https://api.dingtalk.com.evil/",
        "https://oapi.dingtalk.com/robot/send",
        "https://api.dingtalk.com@evil/",
    ] {
        assert!(!wire::trusted_webhook(&rejected.into()));
    }
}
#[test]
fn renderer_keeps_failure_and_text_fragments() {
    assert_eq!(
        wire::render(&parse_json(r#"{"type":"assistant_text_delta","text":"你好"}"#).unwrap()),
        Some("你好".into())
    );
    assert_eq!(
        wire::render(
            &parse_json(r#"{"type":"tool_call_finished","ok":false,"toolName":"mock"}"#).unwrap()
        ),
        Some("\n⚠️ mock 执行失败\n".into())
    );
    assert_eq!(
        wire::render(&parse_json(r#"{"type":"error","message":"bad"}"#).unwrap()),
        Some("\n❌ bad\n".into())
    );
}

struct Responder;
impl crate::channel::im_interaction::InteractionResponder for Responder {
    fn respond_elicitation(
        &self,
        _: crate::channel::sms::ElicitationResponse,
    ) -> BoxFuture<Result<(), ChannelError>> {
        Box::pin(async { Ok(()) })
    }
    fn permission_decide(
        &self,
        _: crate::channel::sms::PermissionDecision,
    ) -> BoxFuture<Result<crate::channel::sms::PermissionDecisionOutcome, ChannelError>> {
        Box::pin(async { Ok(crate::channel::sms::PermissionDecisionOutcome { delivered: true }) })
    }
}
struct MockHttp {
    endpoint: String,
    requests: Arc<Mutex<Vec<(String, JsValue)>>>,
}
impl ModelHttpTransport for MockHttp {
    fn send(
        &self,
        request: hyper::Request<http_body_util::Full<Bytes>>,
    ) -> crate::runtime::model::http::TransportFuture {
        let endpoint = self.endpoint.clone();
        let requests = self.requests.clone();
        crate::runtime::model::http::TransportFuture::io(async move {
            let (parts, body) = request.into_parts();
            let body = body.collect().await.unwrap().to_bytes();
            let body = parse_json(&String::from_utf8_lossy(&body)).unwrap();
            requests.lock().unwrap().push((parts.uri.to_string(), body));
            let reply = if parts.uri.path().ends_with("/open") {
                serde_json::json!({"endpoint":endpoint,"ticket":"fixture"}).to_string()
            } else {
                "{\"errcode\":123}".into()
            };
            let body = http_body_util::Full::new(Bytes::from(reply))
                .map_err(|never| match never {})
                .boxed();
            Ok(hyper::Response::new(body))
        })
    }
}
struct Gateway(Arc<Mutex<Vec<TurnRequest>>>);
struct Events(VecDeque<JsValue>);
impl TurnEventStream for Events {
    fn next(
        &mut self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<JsValue>, ChannelError>> + Send + '_>,
    > {
        Box::pin(async move { Ok(self.0.pop_front()) })
    }
}
impl TurnGateway for Gateway {
    fn submit(
        &self,
        request: TurnRequest,
    ) -> BoxFuture<Result<Box<dyn TurnEventStream>, ChannelError>> {
        self.0.lock().unwrap().push(request);
        Box::pin(async {
            Ok(Box::new(Events(VecDeque::from([
                parse_json(r#"{"type":"assistant_text_delta","text":"你好"}"#).unwrap(),
                parse_json(r#"{"type":"tool_call_finished","ok":false,"toolName":"mock"}"#)
                    .unwrap(),
            ]))) as Box<dyn TurnEventStream>)
        })
    }
}
#[tokio::test]
async fn stream_real_peer_ack_gateway_and_https_webhook_match_injected_oracle() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let endpoint = format!("ws://{}/connect", listener.local_addr().unwrap());
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_hdr_async(
            stream,
            |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
             response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                assert_eq!(request.uri().to_string(), "/connect?ticket=fixture");
                Ok(response)
            },
        )
        .await
        .unwrap();
        let data=serde_json::json!({"msgId":"1","conversationId":"room","text":{"content":" hello "},"sessionWebhook":"https://api.dingtalk.com/robot/send?access_token=fixture"}).to_string();
        let frame=serde_json::json!({"type":"CALLBACK","headers":{"topic":wire::ROBOT_TOPIC,"messageId":"wire-1"},"data":data}).to_string();
        socket
            .send(Message::Text(frame.clone().into()))
            .await
            .unwrap();
        let ack = socket.next().await.unwrap().unwrap();
        let ack: serde_json::Value = serde_json::from_str(ack.to_text().unwrap()).unwrap();
        assert_eq!(ack["headers"]["messageId"], "wire-1");
        assert_eq!(ack["data"], r#"{"response":null}"#);
        socket.send(Message::Text(frame.into())).await.unwrap();
        let _ = socket.next().await.unwrap().unwrap();
        ready_tx.send(()).unwrap();
        let _ = release_rx.await;
        let close = socket.next().await.unwrap().unwrap();
        assert!(matches!(close, Message::Close(_)));
    });
    let requests = Arc::new(Mutex::new(Vec::new()));
    let turns = Arc::new(Mutex::new(Vec::new()));
    let channel = DingTalkChannel::new(
        DingTalkChannelOptions {
            client_id: Some("fixture".into()),
            client_secret: Some("secret".into()),
            transport: Some(Arc::new(MockHttp {
                endpoint,
                requests: requests.clone(),
            })),
            env: Some(Arc::new(|_| None)),
            ..Default::default()
        },
        js_helper_interaction(Arc::new(Responder)),
    );
    let handle = channel
        .start(ChannelStartDeps {
            turn_gateway: Some(Arc::new(Gateway(turns.clone()))),
            ..Default::default()
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        ready_rx.await.unwrap();
        loop {
            if requests.lock().unwrap().len() == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(turns.lock().unwrap().len(), 1);
    let turns = turns.lock().unwrap();
    assert_eq!(turns[0].message, JsString::from("hello"));
    assert_eq!(
        turns[0].session_key,
        JsString::from("dingtalk:chat=room:general")
    );
    drop(turns);
    let requests = requests.lock().unwrap();
    assert_eq!(
        requests[0].1.get("subscriptions"),
        Some(&JsValue::Array(vec![object([
            ("type", JsValue::from("CALLBACK")),
            ("topic", JsValue::from(wire::ROBOT_TOPIC))
        ])]))
    );
    let oracle = parse_json(include_str!(
        "../../../tests/fixtures/h1-dingtalk/expected.json"
    ))
    .unwrap();
    let expected = match oracle.get("replies").unwrap() {
        JsValue::Array(items) => items[0].get("body").unwrap(),
        _ => panic!(),
    };
    assert_eq!(&requests[1].1, expected);
    drop(requests);
    release_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), handle.stop("fixture"))
        .await
        .unwrap()
        .unwrap();
    peer.await.unwrap();
}

struct PendingGateway {
    submitted: Arc<std::sync::atomic::AtomicUsize>,
    cancel_started: Arc<tokio::sync::Notify>,
    release_cancel: Arc<tokio::sync::Notify>,
    finished: Arc<std::sync::atomic::AtomicBool>,
}
struct PendingEvents {
    cancel_started: Arc<tokio::sync::Notify>,
    release_cancel: Arc<tokio::sync::Notify>,
    finished: Arc<std::sync::atomic::AtomicBool>,
}
impl TurnEventStream for PendingEvents {
    fn next(
        &mut self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<JsValue>, ChannelError>> + Send + '_>,
    > {
        Box::pin(std::future::pending())
    }
    fn cancel_and_drain(
        &mut self,
        _: JsString,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ChannelError>> + Send + '_>>
    {
        Box::pin(async move {
            self.cancel_started.notify_one();
            self.release_cancel.notified().await;
            self.finished
                .store(true, std::sync::atomic::Ordering::Release);
            Ok(())
        })
    }
}
impl TurnGateway for PendingGateway {
    fn submit(&self, _: TurnRequest) -> BoxFuture<Result<Box<dyn TurnEventStream>, ChannelError>> {
        self.submitted
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let stream = PendingEvents {
            cancel_started: self.cancel_started.clone(),
            release_cancel: self.release_cancel.clone(),
            finished: self.finished.clone(),
        };
        Box::pin(async { Ok(Box::new(stream) as Box<dyn TurnEventStream>) })
    }
}
#[tokio::test]
async fn cancelling_a_stop_waiter_keeps_the_owned_turn_drain_alive() {
    let submitted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let channel = DingTalkChannel::new(
        DingTalkChannelOptions {
            env: Some(Arc::new(|_| None)),
            ..Default::default()
        },
        js_helper_interaction(Arc::new(Responder)),
    );
    channel.0.state.lock().unwrap().gateway = Some(Arc::new(PendingGateway {
        submitted: submitted.clone(),
        cancel_started: started.clone(),
        release_cancel: release.clone(),
        finished: finished.clone(),
    }));
    let shared = channel.0.clone();
    channel.0.spawn_handler(async move{shared.on_downstream(parse_json(r#"{"headers":{"messageId":"owned"},"data":"{\"msgId\":\"owned\",\"conversationId\":\"room\",\"text\":\"hello\"}"}"#).unwrap()).await;});
    let handle = Handle(channel.0.clone());
    let waiter = tokio::spawn(handle.stop("first"));
    tokio::time::timeout(Duration::from_secs(3), started.notified())
        .await
        .unwrap();
    waiter.abort();
    let _ = waiter.await;
    let second = tokio::spawn(handle.stop("second"));
    tokio::task::yield_now().await;
    assert!(!second.is_finished());
    assert!(!finished.load(std::sync::atomic::Ordering::Acquire));
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(3), second)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(finished.load(std::sync::atomic::Ordering::Acquire));
    assert_eq!(submitted.load(std::sync::atomic::Ordering::Relaxed), 1);
}
#[tokio::test]
async fn finished_handlers_are_harvested_and_panics_stay_failed() {
    let channel = DingTalkChannel::new(
        DingTalkChannelOptions {
            env: Some(Arc::new(|_| None)),
            ..Default::default()
        },
        js_helper_interaction(Arc::new(Responder)),
    );
    channel
        .0
        .handlers
        .lock()
        .unwrap()
        .push(tokio::spawn(async {}));
    tokio::task::yield_now().await;
    channel.0.spawn_handler(std::future::pending());
    assert_eq!(channel.0.handlers.lock().unwrap().len(), 1);
    channel.0.handlers.lock().unwrap().pop().unwrap().abort();
    channel.0.handlers.lock().unwrap().push(tokio::spawn(async {
        panic!("owned handler fixture panic")
    }));
    tokio::task::yield_now().await;
    let handle = Handle(channel.0.clone());
    assert!(handle.stop("panic").await.is_err());
    assert!(handle.stop("retry").await.is_err());
}
#[tokio::test]
async fn disconnect_reopens_the_control_endpoint_and_socket() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let endpoint = format!("ws://{}/connect", listener.local_addr().unwrap());
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut first = tokio_tungstenite::accept_async(stream).await.unwrap();
        first.close(None).await.unwrap();
        drop(first);
        let (stream, _) = listener.accept().await.unwrap();
        let mut second = tokio_tungstenite::accept_async(stream).await.unwrap();
        ready_tx.send(()).unwrap();
        assert!(matches!(
            second.next().await.unwrap().unwrap(),
            Message::Close(_)
        ));
    });
    let requests = Arc::new(Mutex::new(Vec::new()));
    let channel = DingTalkChannel::new(
        DingTalkChannelOptions {
            client_id: Some("fixture".into()),
            client_secret: Some("secret".into()),
            transport: Some(Arc::new(MockHttp {
                endpoint,
                requests: requests.clone(),
            })),
            env: Some(Arc::new(|_| None)),
            ..Default::default()
        },
        js_helper_interaction(Arc::new(Responder)),
    );
    let handle = channel.start(Default::default()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(4), ready_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.lock().unwrap().len(), 2);
    handle.stop("reconnected").await.unwrap();
    peer.await.unwrap();
}
