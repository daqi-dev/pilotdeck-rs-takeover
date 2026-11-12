use super::*;
#[test]
fn discord_baseline_filters_session_and_chunks() {
    let own = JsString::from("bot");
    for raw in [
        r#"{"author":{"id":"a"},"type":6,"channel_id":"c","content":"pin system"}"#,
        r#"{"author":{"id":"a","bot":true},"channel_id":"c","content":"ignore"}"#,
        r#"{"author":{"id":"a"},"type":7,"channel_id":"c","content":"ignore"}"#,
        r#"{"author":{"id":"bot"},"channel_id":"c","content":"ignore"}"#,
    ] {
        assert!(wire::message(&parse_json(raw).unwrap(), Some(&own)).is_none());
    }
    let text = wire::message(
        &parse_json(r#"{"author":{"id":"a"},"channel_id":"c","content":" hi "}"#).unwrap(),
        Some(&own),
    )
    .unwrap();
    assert_eq!(text, JsString::from("hi"));
    let mut mapper = DiscordSessionMapper::new(Default::default(), Box::new(|| "fixed".into()));
    let chat = JsString::from("c");
    assert_eq!(
        mapper.resolve(&chat, &text).session_key,
        JsString::from("discord:chat=c:general")
    );
    assert_eq!(
        mapper
            .resolve(&chat, &JsString::from("/new hi"))
            .session_key,
        JsString::from("discord:chat=c:s_fixed")
    );
    assert_eq!(
        mapper.resolve(&chat, &text).session_key,
        JsString::from("discord:chat=c:s_fixed")
    );
    assert_eq!(wire::chunk_text(&JsString::from("x".repeat(2001))).len(), 2);
}

use crate::channel::im_interaction::InteractionResponder;
use crate::channel::sms::{ElicitationResponse, PermissionDecision, PermissionDecisionOutcome};
struct Responder;
impl InteractionResponder for Responder {
    fn respond_elicitation(&self, _: ElicitationResponse) -> BoxFuture<Result<(), ChannelError>> {
        Box::pin(async { Ok(()) })
    }
    fn permission_decide(
        &self,
        _: PermissionDecision,
    ) -> BoxFuture<Result<PermissionDecisionOutcome, ChannelError>> {
        Box::pin(async { Ok(PermissionDecisionOutcome { delivered: true }) })
    }
}
#[derive(Default)]
struct Rest(Mutex<Vec<(Method, String, HeaderMap, String)>>);
impl ModelHttpTransport for Rest {
    fn send(&self, request: Request<Full<Bytes>>) -> TransportFuture {
        let (parts, body) = request.into_parts();
        let path = parts.uri.path().to_owned();
        let body = String::from_utf8(
            body.collect()
                .now_or_never()
                .expect("Full body is immediately ready")
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        self.0
            .lock()
            .unwrap()
            .push((parts.method, path.clone(), parts.headers, body));
        let body = if path.ends_with("/c") {
            r#"{"id":"c","type":0}"#
        } else {
            "{}"
        };
        TransportFuture::ready(Ok(Response::new(
            Full::new(Bytes::from_static(body.as_bytes()))
                .map_err(|never| match never {})
                .boxed(),
        )))
    }
}
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(4), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Discord mock condition");
}

#[tokio::test]
async fn discord_gateway_identifies_waits_ready_resumes_and_native_rest_sends() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let resume = url.clone();
    let packets = Arc::new(Mutex::new(Vec::new()));
    let captured = packets.clone();
    let peer = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
        ws.send(Message::Text(
            r#"{"op":10,"d":{"heartbeat_interval":45000}}"#.into(),
        ))
        .await
        .unwrap();
        let identify = ws.next().await.unwrap().unwrap().into_text().unwrap();
        captured
            .lock()
            .unwrap()
            .push(parse_json(identify.as_str()).unwrap());
        ws.send(Message::Text(serde_json::json!({"op":0,"s":1,"t":"READY","d":{"session_id":"session","resume_gateway_url":resume,"user":{"id":"bot"}}}).to_string().into())).await.unwrap();
        ws.send(Message::Text(r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"author":{"id":"a"},"channel_id":"c","type":0,"content":"/new"}}"#.into())).await.unwrap();
        ws.send(Message::Text(r#"{"op":7,"d":null}"#.into()))
            .await
            .unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
        ws.send(Message::Text(
            r#"{"op":10,"d":{"heartbeat_interval":45000}}"#.into(),
        ))
        .await
        .unwrap();
        let resumed = ws.next().await.unwrap().unwrap().into_text().unwrap();
        captured
            .lock()
            .unwrap()
            .push(parse_json(resumed.as_str()).unwrap());
        while ws.next().await.is_some() {}
    });
    let rest = Arc::new(Rest::default());
    let channel = DiscordChannel::new(
        DiscordChannelOptions {
            token: Some("fixture".into()),
            api_base: Some("http://mock.invalid/api/v10".into()),
            gateway_url: Some(url),
            transport: Some(rest.clone()),
            env: Some(Arc::new(|_| None)),
            ..Default::default()
        },
        js_helper_interaction(Arc::new(Responder)),
    );
    let handle = channel.start(Default::default()).await.unwrap();
    assert_eq!(channel.0.state.lock().unwrap().user_id, Some("bot".into()));
    until(|| packets.lock().unwrap().len() == 2).await;
    let packets = packets.lock().unwrap().clone();
    assert_eq!(packets[0].get("op").and_then(JsValue::as_number), Some(2.0));
    assert_eq!(
        packets[0]
            .get("d")
            .unwrap()
            .get("intents")
            .and_then(JsValue::as_number),
        Some(37377.0)
    );
    assert_eq!(packets[1].get("op").and_then(JsValue::as_number), Some(6.0));
    assert_eq!(
        packets[1].get("d").unwrap().get("session_id"),
        Some(&JsValue::from("session"))
    );
    assert_eq!(
        packets[1]
            .get("d")
            .unwrap()
            .get("seq")
            .and_then(JsValue::as_number),
        Some(2.0)
    );
    until(|| {
        rest.0
            .lock()
            .unwrap()
            .iter()
            .any(|(method, path, _, _)| *method == Method::POST && path.ends_with("/messages"))
    })
    .await;
    let requests = rest.0.lock().unwrap().clone();
    assert!(
        requests
            .iter()
            .all(|(_, _, headers, _)| headers.get("authorization").unwrap() == "Bot fixture")
    );
    let (_, _, _, body) = requests
        .iter()
        .find(|(method, path, _, _)| *method == Method::POST && path.ends_with("/messages"))
        .unwrap();
    assert_eq!(
        parse_json(body).unwrap().get("content"),
        Some(&JsValue::from("已创建新会话。"))
    );
    handle.stop("fixture complete").await.unwrap();
    assert!(!channel.0.send_reply(&"c".into(), &"late".into()).await);
    peer.await.unwrap();
}

#[tokio::test]
async fn discord_fatal_auth_fails_start_and_releases_receiver() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let peer = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
        ws.send(Message::Text(
            r#"{"op":10,"d":{"heartbeat_interval":45000}}"#.into(),
        ))
        .await
        .unwrap();
        ws.next().await.unwrap().unwrap();
        ws.close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
            code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Library(4004),
            reason: "fixture auth failure".into(),
        }))
        .await
        .unwrap();
    });
    let channel = DiscordChannel::new(
        DiscordChannelOptions {
            token: Some("fixture".into()),
            gateway_url: Some(url),
            env: Some(Arc::new(|_| None)),
            ..Default::default()
        },
        js_helper_interaction(Arc::new(Responder)),
    );
    let handle = tokio::time::timeout(Duration::from_secs(3), channel.start(Default::default()))
        .await
        .unwrap()
        .unwrap();
    assert!(!channel.0.running.load(Ordering::Acquire));
    assert!(channel.0.receiver.lock().unwrap().is_none());
    handle.stop("noop").await.unwrap();
    peer.await.unwrap();
}

#[test]
fn discord_gateway_query_is_an_origin_form_request_target() {
    for base in ["ws://127.0.0.1:12345", "ws://127.0.0.1:12345/"] {
        let url = wire::gateway_url(base).unwrap();
        assert_eq!(url, "ws://127.0.0.1:12345/?v=10&encoding=json");
        let request =
            tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
                url.as_str(),
            )
            .unwrap();
        assert_eq!(
            request.uri().path_and_query().unwrap().as_str(),
            "/?v=10&encoding=json"
        );
    }
    assert_eq!(
        wire::gateway_url("wss://gateway.discord.gg/?encoding=etf&v=9&fixture=yes").unwrap(),
        "wss://gateway.discord.gg/?fixture=yes&v=10&encoding=json"
    );
}
