use super::*;
fn oracle() -> JsValue {
    parse_json(include_str!(
        "../../../tests/fixtures/h1-mattermost/values.json"
    ))
    .unwrap()
}
fn array<'a>(value: &'a JsValue, key: &str) -> &'a [JsValue] {
    let JsValue::Array(items) = value.get(key).unwrap() else {
        panic!("array")
    };
    items
}
fn string(value: &JsValue, key: &str) -> JsString {
    value.get(key).unwrap().as_string().unwrap().clone()
}
#[test]
fn baseline_mapper_state_and_utf16() {
    let fixture = oracle();
    let mut mapper = Mapper::default();
    for case in array(&fixture, "mapping") {
        let input = case.get("input").unwrap();
        let expected = case.get("output").unwrap();
        let (session, new, message) =
            mapper.resolve(&string(input, "chatId"), &string(input, "text"), || {
                "fixture-uuid".into()
            });
        assert_eq!(session, string(expected, "sessionKey"));
        assert_eq!(message, string(expected, "message"));
        assert_eq!(new, expected.get("command").is_some());
    }
}
#[test]
fn baseline_renderer() {
    let fixture = oracle();
    for case in array(&fixture, "rendering") {
        let actual = render_event(case.get("event").unwrap())
            .map(JsValue::String)
            .unwrap_or(JsValue::Null);
        assert_eq!(&actual, case.get("output").unwrap());
    }
}
#[test]
fn baseline_utf16_chunks_preserve_surrogates_and_space() {
    let fixture = oracle();
    for case in array(&fixture, "chunking") {
        let chunks = chunk_text(&string(case, "text"), MAX_MESSAGE);
        let expected = array(case, "posts")
            .iter()
            .map(|post| string(post, "message"))
            .collect::<Vec<_>>();
        assert_eq!(chunks, expected, "{:?}", case.get("name"));
        for chunk in chunks {
            let encoded = stringify_json(&JsValue::String(chunk.clone()))
                .unwrap()
                .unwrap();
            assert_eq!(parse_json(&encoded).unwrap(), JsValue::String(chunk));
        }
    }
}
#[test]
fn formal_url_paths_and_headers() {
    assert_eq!(
        websocket_url("http://127.0.0.1:12345/api/v4", "fixture").unwrap(),
        (
            "ws://127.0.0.1:12345/api/v4/api/v4/websocket?token=fixture".into(),
            "http://127.0.0.1:12345".into()
        )
    );
    assert_eq!(
        websocket_url("http://127.0.0.1:12345", "a b+&?!'()*")
            .unwrap()
            .0,
        "ws://127.0.0.1:12345/api/v4/websocket?token=a%20b%2B%26%3F!%27()*"
    );
}
#[test]
fn inbound_filter_uses_string_true_and_thread_chat() {
    let frame = |flag: JsValue| {
        let mut props = JsObject::new();
        props.insert("from_bot".into(), flag);
        let mut post = JsObject::new();
        post.insert("user_id".into(), "user".into());
        post.insert("channel_id".into(), "c".into());
        post.insert("root_id".into(), "r".into());
        post.insert("message".into(), " a\r\nb ".into());
        post.insert("props".into(), JsValue::Object(props));
        let mut data = JsObject::new();
        data.insert(
            "post".into(),
            JsValue::String(
                stringify_json(&JsValue::Object(post))
                    .unwrap()
                    .unwrap()
                    .into(),
            ),
        );
        let mut frame = JsObject::new();
        frame.insert("event".into(), "posted".into());
        frame.insert("data".into(), JsValue::Object(data));
        stringify_json(&JsValue::Object(frame)).unwrap().unwrap()
    };
    assert!(decode_post(&frame("true".into()), &JsValue::from("bot")).is_none());
    let (ctx, text) = decode_post(&frame(JsValue::Bool(true)), &JsValue::from("bot")).unwrap();
    assert_eq!(ctx.chat, JsString::from("c:r"));
    assert_eq!(text, JsString::from("a\nb"));
}

use super::super::{
    im_interaction::InteractionResponder,
    protocol::{TurnEventStream, TurnGateway},
    sms::{ElicitationResponse, PermissionDecision, PermissionDecisionOutcome},
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
};

#[derive(Default)]
struct Responder {
    answers: Mutex<Vec<JsValue>>,
}
impl InteractionResponder for Responder {
    fn respond_elicitation(
        &self,
        response: ElicitationResponse,
    ) -> BoxFuture<Result<(), ChannelError>> {
        self.answers
            .lock()
            .unwrap()
            .push(JsValue::Object(response.to_js()));
        Box::pin(async { Ok(()) })
    }
    fn permission_decide(
        &self,
        decision: PermissionDecision,
    ) -> BoxFuture<Result<PermissionDecisionOutcome, ChannelError>> {
        self.answers
            .lock()
            .unwrap()
            .push(JsValue::Object(decision.to_js()));
        Box::pin(async { Ok(PermissionDecisionOutcome { delivered: true }) })
    }
}
struct TestStream {
    rx: mpsc::UnboundedReceiver<JsValue>,
    cancelled: Arc<AtomicUsize>,
    cancel_failures: Arc<AtomicUsize>,
    cancel_blocked: Arc<AtomicBool>,
}
impl TurnEventStream for TestStream {
    fn next(
        &mut self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<JsValue>, ChannelError>> + Send + '_>,
    > {
        Box::pin(async move { Ok(self.rx.recv().await) })
    }
    fn cancel_and_drain(
        &mut self,
        _: JsString,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ChannelError>> + Send + '_>>
    {
        Box::pin(async move {
            self.cancelled.fetch_add(1, Ordering::SeqCst);
            while self.cancel_blocked.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            if self
                .cancel_failures
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                return Err(ChannelError::error("fixture cancel failed"));
            }
            self.rx.close();
            while self.rx.recv().await.is_some() {}
            Ok(())
        })
    }
}
#[derive(Default)]
struct Turns {
    requests: Mutex<Vec<TurnRequest>>,
    senders: Mutex<Vec<mpsc::UnboundedSender<JsValue>>>,
    cancelled: Arc<AtomicUsize>,
    cancel_failures: Arc<AtomicUsize>,
    cancel_blocked: Arc<AtomicBool>,
}
impl TurnGateway for Turns {
    fn submit(
        &self,
        request: TurnRequest,
    ) -> BoxFuture<Result<Box<dyn TurnEventStream>, ChannelError>> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.requests.lock().unwrap().push(request);
        self.senders.lock().unwrap().push(tx);
        let cancelled = self.cancelled.clone();
        let cancel_failures = self.cancel_failures.clone();
        let cancel_blocked = self.cancel_blocked.clone();
        Box::pin(async move {
            Ok(Box::new(TestStream {
                rx,
                cancelled,
                cancel_failures,
                cancel_blocked,
            }) as Box<dyn TurnEventStream>)
        })
    }
}
async fn until(predicate: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(10)).await
        }
    })
    .await
    .expect("observable loopback result");
}
struct Peer {
    url: String,
    posts: Arc<Mutex<Vec<JsValue>>>,
    ws: mpsc::UnboundedReceiver<tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>>,
    owner: JoinHandle<()>,
    headers: Arc<Mutex<Vec<String>>>,
    fail_next_post: Arc<AtomicBool>,
    response_mode: Arc<AtomicUsize>,
}
impl Peer {
    async fn start() -> Self {
        use base64::Engine;
        use sha1::{Digest, Sha1};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let posts = Arc::new(Mutex::new(Vec::new()));
        let saved = posts.clone();
        let headers = Arc::new(Mutex::new(Vec::new()));
        let captured = headers.clone();
        let response_mode = Arc::new(AtomicUsize::new(0));
        let modes = response_mode.clone();
        let fail_next_post = Arc::new(AtomicBool::new(false));
        let failures = fail_next_post.clone();
        let (tx, ws) = mpsc::unbounded_channel();
        let owner = tokio::spawn(async move {
            let mut children = JoinSet::new();
            loop {
                tokio::select! {
                    Some(_)=children.join_next(),if !children.is_empty()=>{},
                    accepted=listener.accept()=>{let (mut stream,_)=accepted.unwrap();let posts=saved.clone();let tx=tx.clone();let headers=captured.clone();let failures=failures.clone();let modes=modes.clone();children.spawn(async move {
                        let mut head=Vec::new();while !head.ends_with(b"\r\n\r\n") {let mut byte=[0];if stream.read_exact(&mut byte).await.is_err(){return}head.push(byte[0]);assert!(head.len()<65536);}
                        let head=String::from_utf8(head).unwrap();headers.lock().unwrap().push(head.clone());
                        if head.contains("/api/v4/websocket?") {let key=head.lines().find_map(|line|line.split_once(':').filter(|(name,_)|name.eq_ignore_ascii_case("sec-websocket-key")).map(|(_,value)|value.trim())).unwrap();let accept=base64::engine::general_purpose::STANDARD.encode(Sha1::digest(format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11")));let extension=if modes.load(Ordering::SeqCst)==5 {"Sec-WebSocket-Extensions: permessage-deflate\r\n"} else {""};stream.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n{extension}\r\n").as_bytes()).await.unwrap();let socket=tokio_tungstenite::WebSocketStream::from_raw_socket(stream,tokio_tungstenite::tungstenite::protocol::Role::Server,None).await;let _=tx.send(socket);return}
                        let length=head.lines().find_map(|line|line.split_once(':').filter(|(name,_)|name.eq_ignore_ascii_case("content-length")).map(|(_,length)|length.trim().parse::<usize>().unwrap())).unwrap_or(0);let mut body=vec![0;length];stream.read_exact(&mut body).await.unwrap();
                        if head.starts_with("POST ") {posts.lock().unwrap().push(parse_json(std::str::from_utf8(&body).unwrap()).unwrap());}
                        let redirect=match modes.load(Ordering::SeqCst) {1 if head.starts_with("GET /users/me ")=>Some(("302 Found","/users/me-final")),2 if head.starts_with("POST /posts ")=>Some(("307 Temporary Redirect","/posts-final")),_=>None};
                        if let Some((status,location))=redirect {stream.write_all(format!("HTTP/1.1 {status}\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();return;}
                        let status=if head.starts_with("POST ")&&failures.swap(false,Ordering::SeqCst) {"500 Internal Server Error"} else {"200 OK"};
                        let body: &[u8]=if modes.load(Ordering::SeqCst)==4 {b"null"} else if modes.load(Ordering::SeqCst)==3 {b"\xef\xbb\xbf{\"id\":\"fixture-bot\"}"} else {b"{\"id\":\"fixture-bot\"}"};stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await.unwrap();stream.write_all(body).await.unwrap();
                    });}
                }
            }
        });
        Self {
            url,
            posts,
            ws,
            owner,
            headers,
            fail_next_post,
            response_mode,
        }
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.owner.abort();
    }
}
async fn post(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    message: &str,
    root: Option<&str>,
) {
    let post = serde_json::json!({"user_id":"fixture-user","channel_id":"fixture-channel","message":message,"root_id":root});
    let frame = serde_json::json!({"event":"posted","data":{"post":post.to_string()}});
    socket
        .send(Message::Text(frame.to_string().into()))
        .await
        .unwrap();
}
#[tokio::test]
async fn loopback_turn_busy_thread_and_owned_stop() {
    let mut peer = Peer::start().await;
    let turns = Arc::new(Turns::default());
    let responder = Arc::new(Responder::default());
    let helper = js_helper_interaction(responder.clone());
    let channel = MattermostChannel::new(
        MattermostOptions {
            token: Some("fixture".into()),
            server_url: Some(JsValue::String(peer.url.clone().into())),
            team_id: None,
        },
        helper,
    );
    let handle = channel
        .start(ChannelStartDeps {
            turn_gateway: Some(turns.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut socket = peer.ws.recv().await.unwrap();
    post(&mut socket, "/new", None).await;
    until(|| peer.posts.lock().unwrap().len() == 1).await;
    post(&mut socket, "hello", None).await;
    until(|| turns.requests.lock().unwrap().len() == 1).await;
    post(&mut socket, "/new", None).await;
    post(&mut socket, "other", Some("thread")).await;
    until(|| turns.requests.lock().unwrap().len() == 2).await;
    let requests = turns.requests.lock().unwrap().clone();
    assert!(
        requests[0]
            .session_key
            .to_utf8()
            .unwrap()
            .starts_with("mattermost:chat=fixture-channel:s_")
    );
    assert_eq!(
        requests[1].session_key,
        JsString::from("mattermost:chat=fixture-channel:thread:general")
    );
    assert_eq!(requests[0].channel_key, ChannelKey::new("mattermost"));
    assert_eq!(peer.posts.lock().unwrap().len(), 1);
    turns.senders.lock().unwrap()[0].send(parse_json(r#"{"type":"elicitation_request","requestId":"q1","questions":[{"id":"q","question":"Choose","options":[{"label":"A"},{"label":"B"}]}]}"#).unwrap()).unwrap();
    until(|| peer.posts.lock().unwrap().len() == 2).await;
    post(&mut socket, "1", None).await;
    until(|| responder.answers.lock().unwrap().len() == 1).await;
    assert_eq!(turns.requests.lock().unwrap().len(), 2);
    tokio::time::timeout(Duration::from_secs(2), handle.stop("test-stop"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(turns.cancelled.load(Ordering::SeqCst), 2);
    assert!(matches!(
        socket.next().await,
        Some(Ok(Message::Close(_))) | None
    ));
    assert!(
        peer.headers
            .lock()
            .unwrap()
            .iter()
            .any(|head| head.contains("authorization: Bearer fixture"))
    );
    assert!(
        !channel
            .deliver_cron_result(&super::super::im_cron::CronResultDelivery {
                task_id: "fixture-task".into(),
                run_id: "fixture-run".into(),
                session_key: "mattermost:chat=c:general".into(),
                channel_key: ChannelKey::new("mattermost"),
                origin_session_key: None,
                text: JsString::from("cron"),
                origin_channel_key: None
            })
            .await
    );
}

#[tokio::test]
async fn baseline_helper_interactions_through_http_and_websocket() {
    let fixture = parse_json(include_str!(
        "../../../tests/fixtures/h1-mattermost/interactions.json"
    ))
    .unwrap();
    for expected in array(&fixture, "records") {
        let mode = string(expected, "mode");
        let elicitation = mode.equals_str("elicitation");
        let failed = mode.equals_str("permission-initial-failure");
        let mut peer = Peer::start().await;
        let turns = Arc::new(Turns::default());
        let responder = Arc::new(Responder::default());
        let helper = Arc::new(ObservedHelper {
            inner: js_helper_interaction(responder.clone()),
            confirmed: AtomicUsize::new(0),
        });
        let channel = MattermostChannel::new(
            MattermostOptions {
                token: Some("fixture".into()),
                server_url: Some(JsValue::String(peer.url.clone().into())),
                team_id: None,
            },
            helper.clone(),
        );
        let handle = channel
            .start(ChannelStartDeps {
                turn_gateway: Some(turns.clone()),
                ..Default::default()
            })
            .await
            .unwrap();
        let mut socket = peer.ws.recv().await.unwrap();
        peer.fail_next_post.store(failed, Ordering::SeqCst);
        post(&mut socket, "start", None).await;
        until(|| !turns.senders.lock().unwrap().is_empty()).await;
        let send = |raw: &str| {
            turns.senders.lock().unwrap()[0]
                .send(parse_json(raw).unwrap())
                .unwrap()
        };
        if elicitation {
            send(
                r#"{"type":"elicitation_request","requestId":"e1","questions":[{"id":"q","header":"h","question":"Choose","options":[{"label":"A"},{"label":"B"}]}]}"#,
            )
        } else {
            send(
                r#"{"type":"permission_request","requestId":"p1","toolName":"write","payload":{"path":"fixture.txt"}}"#,
            );
            send(
                r#"{"type":"permission_request","requestId":"p2","toolName":"read","payload":{"path":"fixture2.txt"}}"#,
            );
        }
        until(|| peer.posts.lock().unwrap().len() == 1).await;
        if !elicitation {
            until(|| helper.confirmed.load(Ordering::SeqCst) >= 1).await;
        }
        if failed {
            post(&mut socket, "1", None).await;
            until(|| peer.posts.lock().unwrap().len() == 3).await;
            until(|| helper.confirmed.load(Ordering::SeqCst) >= 2).await;
            assert!(responder.answers.lock().unwrap().is_empty());
        }
        post(&mut socket, "1", None).await;
        until(|| responder.answers.lock().unwrap().len() == 1).await;
        if !elicitation {
            until(|| peer.posts.lock().unwrap().len() == if failed { 5 } else { 3 }).await;
            until(|| helper.confirmed.load(Ordering::SeqCst) >= if failed { 3 } else { 2 }).await;
            post(&mut socket, "2", None).await;
            until(|| responder.answers.lock().unwrap().len() == 2).await;
        }
        // Finish only after the answering handler delivered its acknowledgement.
        if !elicitation {
            until(|| peer.posts.lock().unwrap().len() == if failed { 6 } else { 4 }).await;
        }
        send(r#"{"type":"assistant_text_delta","text":"finished"}"#);
        turns.senders.lock().unwrap().clear();
        let expected_posts = array(expected, "posts");
        until(|| peer.posts.lock().unwrap().len() == expected_posts.len()).await;
        let actual = peer
            .posts
            .lock()
            .unwrap()
            .iter()
            .map(|post| string(post, "message"))
            .collect::<Vec<_>>();
        let expected_text = expected_posts
            .iter()
            .map(|post| string(post.get("body").unwrap(), "message"))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected_text, "{:?}", mode);
        assert_eq!(turns.requests.lock().unwrap().len(), 1);
        let actual_answers = responder.answers.lock().unwrap().clone();
        for (actual, expected) in actual_answers.iter().zip(array(expected, "answers")) {
            assert_eq!(actual.get("requestId"), expected.get("requestId"));
            assert_eq!(actual.get("decision"), expected.get("decision"));
            assert_eq!(actual.get("answer"), expected.get("answer"));
        }
        handle.stop("test").await.unwrap();
    }
}

#[test]
fn null_frames_are_contained_and_literal_surrogates_survive_inner_json() {
    assert!(decode_post("null", &JsValue::Null).is_none());
    assert!(
        decode_post(
            r#"{"event":"posted","data":{"post":"null"}}"#,
            &JsValue::Null
        )
        .is_none()
    );
    let inner = JsString::from_utf16(
        [
            "{\"channel_id\":\"c\",\"message\":\""
                .encode_utf16()
                .collect::<Vec<_>>(),
            vec![0xd800],
            "\"}".encode_utf16().collect(),
        ]
        .concat(),
    );
    let mut data = JsObject::new();
    data.insert("post".into(), JsValue::String(inner));
    let mut event = JsObject::new();
    event.insert("event".into(), "posted".into());
    event.insert("data".into(), JsValue::Object(data));
    let frame = stringify_json(&JsValue::Object(event)).unwrap().unwrap();
    let (_, text) = decode_post(&frame, &JsValue::Null).unwrap();
    assert_eq!(text.units(), &[0xd800]);
}

// Observe completion of the real helper's HTTP delivery callback, so the trace
// never depends on sleeping long enough for a background handler to run.
struct ObservedHelper {
    inner: Arc<dyn JsInteractionPort>,
    confirmed: AtomicUsize,
}
impl JsInteractionPort for ObservedHelper {
    fn clear_chat_for_shutdown(&self, c: &JsString) {
        self.inner.clear_chat_for_shutdown(c)
    }
    fn elicitation_pending(&self, c: &JsString) -> bool {
        self.inner.elicitation_pending(c)
    }
    fn elicitation_capture(&self, c: &JsString, s: &JsString, e: &JsValue) -> JsString {
        self.inner.elicitation_capture(c, s, e)
    }
    fn elicitation_answer<'a>(
        &'a self,
        c: &'a JsString,
        t: &'a JsString,
    ) -> super::super::im_interaction::InteractionFuture<'a, Result<Option<JsString>, ChannelError>>
    {
        self.inner.elicitation_answer(c, t)
    }
    fn elicitation_clear(&self, c: &JsString) {
        self.inner.elicitation_clear(c)
    }
    fn permission_pending(&self, c: &JsString) -> bool {
        self.inner.permission_pending(c)
    }
    fn permission_capture(&self, c: &JsString, s: &JsString, e: &JsValue) -> Option<JsString> {
        self.inner.permission_capture(c, s, e)
    }
    fn permission_confirm_initial(&self, c: &JsString, d: bool, r: Option<&JsString>) {
        self.inner.permission_confirm_initial(c, d, r);
        self.confirmed.fetch_add(1, Ordering::SeqCst);
    }
    fn permission_answer<'a>(
        &'a self,
        c: &'a JsString,
        t: &'a JsString,
    ) -> super::super::im_interaction::InteractionFuture<
        'a,
        Result<Option<super::super::im_interaction::JsPermissionAnswer>, ChannelError>,
    > {
        self.inner.permission_answer(c, t)
    }
    fn permission_release_answer(&self, c: &JsString, t: Option<u64>) {
        self.inner.permission_release_answer(c, t)
    }
    fn permission_take_next_prompt(&self, c: &JsString, t: Option<u64>) -> Option<JsString> {
        self.inner.permission_take_next_prompt(c, t)
    }
    fn permission_prompt_request_id(&self, c: &JsString, t: Option<u64>) -> Option<JsString> {
        self.inner.permission_prompt_request_id(c, t)
    }
    fn permission_confirm_next(&self, c: &JsString, d: bool, r: Option<&JsString>, t: Option<u64>) {
        self.inner.permission_confirm_next(c, d, r, t);
        self.confirmed.fetch_add(1, Ordering::SeqCst);
    }
    fn permission_clear_after_turn(&self, c: &JsString) {
        self.inner.permission_clear_after_turn(c)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn burst_preserves_new_then_first_message_and_cancel_failure_can_retry() {
    let mut peer = Peer::start().await;
    let turns = Arc::new(Turns::default());
    turns.cancel_failures.store(1, Ordering::SeqCst);
    let channel = MattermostChannel::new(
        MattermostOptions {
            token: Some("fixture".into()),
            server_url: Some(JsValue::String(peer.url.clone().into())),
            team_id: None,
        },
        js_helper_interaction(Arc::new(Responder::default())),
    );
    let handle = channel
        .start(ChannelStartDeps {
            turn_gateway: Some(turns.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut socket = peer.ws.recv().await.unwrap();
    post(&mut socket, "/new", None).await;
    post(&mut socket, "first", None).await;
    post(&mut socket, "second", None).await;
    until(|| !turns.requests.lock().unwrap().is_empty()).await;
    // The following frame is a reader-order barrier: its independent thread
    // must be admitted after both same-chat frames have already been examined.
    post(&mut socket, "barrier", Some("barrier")).await;
    until(|| turns.requests.lock().unwrap().len() == 2).await;
    let requests = turns.requests.lock().unwrap().clone();
    assert_eq!(requests[0].message, JsString::from("first"));
    assert!(
        requests[0]
            .session_key
            .to_utf8()
            .unwrap()
            .starts_with("mattermost:chat=fixture-channel:s_")
    );
    assert!(handle.stop("first attempt").await.is_err());
    assert_eq!(turns.cancelled.load(Ordering::SeqCst), 2);
    let still_owned = turns
        .senders
        .lock()
        .unwrap()
        .iter()
        .filter(|sender| !sender.is_closed())
        .count();
    assert_eq!(
        still_owned, 1,
        "failed cancellation retains its exact stream for retry"
    );
    handle.stop("retry").await.unwrap();
    assert_eq!(turns.cancelled.load(Ordering::SeqCst), 3);
    assert!(
        turns
            .senders
            .lock()
            .unwrap()
            .iter()
            .all(mpsc::UnboundedSender::is_closed)
    );
}

#[tokio::test]
async fn dropping_a_stop_waiter_keeps_the_worker_owned_until_retry() {
    let mut peer = Peer::start().await;
    let turns = Arc::new(Turns::default());
    turns.cancel_blocked.store(true, Ordering::SeqCst);
    let channel = MattermostChannel::new(
        MattermostOptions {
            token: Some("fixture".into()),
            server_url: Some(JsValue::String(peer.url.clone().into())),
            team_id: None,
        },
        js_helper_interaction(Arc::new(Responder::default())),
    );
    let handle = channel
        .start(ChannelStartDeps {
            turn_gateway: Some(turns.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut socket = peer.ws.recv().await.unwrap();
    post(&mut socket, "owned", None).await;
    until(|| !turns.requests.lock().unwrap().is_empty()).await;
    let mut waiting = handle.stop("cancel waiter");
    assert!(waiting.as_mut().now_or_never().is_none());
    until(|| turns.cancelled.load(Ordering::SeqCst) == 1).await;
    drop(waiting);
    assert!(!turns.senders.lock().unwrap()[0].is_closed());
    let mut retry = handle.stop("retry waiter");
    assert!(retry.as_mut().now_or_never().is_none());
    assert!(!turns.senders.lock().unwrap()[0].is_closed());
    turns.cancel_blocked.store(false, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(2), retry)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(turns.cancelled.load(Ordering::SeqCst), 1);
    assert!(turns.senders.lock().unwrap()[0].is_closed());
}

#[tokio::test]
async fn fetch_follows_redirects_and_bom_identity_still_filters_self_posts() {
    for mode in 1..=4 {
        let mut peer = Peer::start().await;
        peer.response_mode.store(mode, Ordering::SeqCst);
        let channel = MattermostChannel::new(
            MattermostOptions {
                token: Some("fixture".into()),
                server_url: Some(JsValue::String(peer.url.clone().into())),
                team_id: None,
            },
            js_helper_interaction(Arc::new(Responder::default())),
        );
        let logs = Arc::new(Mutex::new(Vec::new()));
        let captured = logs.clone();
        let handle = channel
            .start(ChannelStartDeps {
                log: Some(Arc::new(move |level, text| {
                    captured
                        .lock()
                        .unwrap()
                        .push((level.to_owned(), text.to_owned()))
                })),
                ..Default::default()
            })
            .await
            .unwrap();
        if mode == 4 {
            assert_eq!(*logs.lock().unwrap(),vec![("error".into(),"mattermost: token check failed: TypeError: Cannot read properties of null (reading 'id')".into())]);
            assert_eq!(peer.headers.lock().unwrap().len(), 1);
            handle.stop("test").await.unwrap();
            continue;
        }
        let mut socket = peer.ws.recv().await.unwrap();
        if mode == 3 {
            socket.send(Message::Text(r#"{"event":"posted","data":{"post":"{\"user_id\":\"fixture-bot\",\"channel_id\":\"c\",\"message\":\"/new\"}"}}"#.into())).await.unwrap();
        }
        post(&mut socket, "/new", None).await;
        until(|| peer.posts.lock().unwrap().len() == if mode == 2 { 2 } else { 1 }).await;
        let posts = peer.posts.lock().unwrap().clone();
        assert!(
            posts
                .iter()
                .all(|post| string(post, "channel_id").equals_str("fixture-channel"))
        );
        assert!(
            posts
                .iter()
                .all(|post| string(post, "message").equals_str("已创建新会话。"))
        );
        let headers = peer.headers.lock().unwrap().clone();
        let requests = headers
            .iter()
            .filter(|head| !head.contains("/api/v4/websocket?"))
            .collect::<Vec<_>>();
        assert!(requests.iter().all(|head| {
            head.to_ascii_lowercase()
                .contains("authorization: bearer fixture")
        }));
        if mode == 1 {
            assert!(
                requests
                    .iter()
                    .any(|head| head.starts_with("GET /users/me-final "))
            );
        }
        if mode == 2 {
            assert!(
                requests
                    .iter()
                    .any(|head| head.starts_with("POST /posts-final "))
            );
            assert_eq!(posts[0], posts[1]);
        }
        handle.stop("test").await.unwrap();
    }
}

#[tokio::test]
async fn false_and_zero_server_urls_keep_required_configuration_branch() {
    for value in [JsValue::Bool(false), JsValue::Number(0.0)] {
        let logs = Arc::new(Mutex::new(Vec::new()));
        let captured = logs.clone();
        let channel = MattermostChannel::new(
            MattermostOptions {
                token: Some("fixture".into()),
                server_url: Some(value),
                team_id: Some(JsValue::Bool(false)),
            },
            js_helper_interaction(Arc::new(Responder::default())),
        );
        let handle = channel
            .start(ChannelStartDeps {
                log: Some(Arc::new(move |level, text| {
                    captured
                        .lock()
                        .unwrap()
                        .push((level.to_owned(), text.to_owned()))
                })),
                ..Default::default()
            })
            .await
            .unwrap();
        handle.stop("test").await.unwrap();
        assert_eq!(
            *logs.lock().unwrap(),
            vec![(
                "error".into(),
                "mattermost: serverUrl and token are required (MATTERMOST_URL, MATTERMOST_TOKEN)"
                    .into()
            )]
        );
    }
}

#[tokio::test]
async fn compressed_binary_is_ignored_after_inflate_and_following_text_reuses_dictionary() {
    use base64::Engine;
    use tokio_tungstenite::tungstenite::protocol::frame::{
        Frame, FrameHeader,
        coding::{Data, OpCode},
    };
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/h1-ws-wire/mattermost-deflate.json"
    ))
    .unwrap();
    let mut peer = Peer::start().await;
    peer.response_mode.store(5, Ordering::SeqCst);
    let turns = Arc::new(Turns::default());
    let channel = MattermostChannel::new(
        MattermostOptions {
            token: Some("fixture".into()),
            server_url: Some(JsValue::String(peer.url.clone().into())),
            team_id: None,
        },
        js_helper_interaction(Arc::new(Responder::default())),
    );
    let handle = channel
        .start(ChannelStartDeps {
            turn_gateway: Some(turns.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut socket = peer.ws.recv().await.unwrap();
    assert!(peer.headers.lock().unwrap().iter().any(|head| {
        head.to_ascii_lowercase()
            .contains("sec-websocket-extensions: permessage-deflate; client_max_window_bits")
    }));
    for frame in fixture["frames"].as_array().unwrap() {
        let payload = base64::engine::general_purpose::STANDARD
            .decode(frame["payloadBase64"].as_str().unwrap())
            .unwrap();
        let opcode = if frame["opcode"] == 2 {
            Data::Binary
        } else {
            Data::Text
        };
        let header = FrameHeader {
            is_final: true,
            rsv1: true,
            opcode: OpCode::Data(opcode),
            ..Default::default()
        };
        socket
            .send(Message::Frame(Frame::from_payload(header, payload.into())))
            .await
            .unwrap();
    }
    until(|| !turns.requests.lock().unwrap().is_empty()).await;
    let requests = turns.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].message,
        JsString::from(fixture["expectedMessage"].as_str().unwrap())
    );
    assert_eq!(
        requests[0].session_key,
        JsString::from("mattermost:chat=fixture-channel:general")
    );
    assert!(peer.posts.lock().unwrap().is_empty());
    handle.stop("test").await.unwrap();
}
