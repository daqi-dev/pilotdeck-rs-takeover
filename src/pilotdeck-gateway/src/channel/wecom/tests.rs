use super::*;
#[test]
fn scope_new_and_projects_preserve_group_user_isolation() {
    let mut mapper = WeComSessionMapper::new(Default::default(), Arc::new(|| "fixed".into()));
    let chat = " room ".into();
    let user = " user ".into();
    let scope = Scope {
        chat_id: &chat,
        user_id: Some(&user),
        group: true,
        group_sessions_per_user: true,
    };
    mapper.bind_project(&scope, "project".into());
    let mapped = mapper.resolve(&scope, &" /new hi ".into());
    assert_eq!(
        mapped.session_key,
        JsString::from("wecom:group=room:user=user:s_fixed")
    );
    assert_eq!(mapped.message, JsString::from("hi"));
    assert_eq!(mapped.project_key, Some("project".into()));
    let other = "other".into();
    let scope = Scope {
        chat_id: &chat,
        user_id: Some(&other),
        group: true,
        group_sessions_per_user: true,
    };
    assert_eq!(
        mapper.resolve(&scope, &"hello".into()).session_key,
        JsString::from("wecom:group=room:user=other:general")
    );
    assert!(mapper.get_project(&scope).is_none());
}
#[test]
fn text_mixed_voice_quote_and_mention_match_baseline() {
    for (raw, expected) in [
        (
            r#"{"msgtype":"mixed","mixed":{"msg_item":[{"msgtype":"text","text":{"content":" a "}},{"msgtype":"image","image":{}},{"msgtype":"text","text":{"content":"b"}}]}}"#,
            "a\nb",
        ),
        (
            r#"{"msgtype":"voice","text":{"content":"one"},"voice":{"content":"two"}}"#,
            "one\ntwo",
        ),
        (
            r#"{"msgtype":"text","quote":{"msgtype":"voice","voice":{"content":" quoted "}}}"#,
            "quoted",
        ),
    ] {
        assert_eq!(
            wire::message_text(&parse_json(raw).unwrap()),
            JsString::from(expected)
        );
    }
    assert_eq!(
        wire::strip_mention(&"@bot hello".into()),
        JsString::from("hello")
    );
    assert_eq!(
        wire::strip_mention(&" @bot hello".into()),
        JsString::from("@bot hello")
    );
}
#[test]
fn access_policy_is_case_insensitive_with_prefixed_entries() {
    assert!(wire::entry_matches(
        &["wecom:group:ROOM".into()],
        &"room".into()
    ));
    assert!(!wire::entry_matches(&[], &"user".into()));
    assert!(wire::entry_matches(&["*".into()], &"any".into()));
}
#[test]
fn response_errors_and_utf16_truncation_are_strict() {
    assert!(wire::response_error(&parse_json(r#"{"body":{"errcode":0}}"#).unwrap()).is_none());
    assert_eq!(
        wire::response_error(&parse_json(r#"{"body":{"errcode":4,"errmsg":"bad"}}"#).unwrap()),
        Some("WeCom errcode 4: bad".into())
    );
    assert!(wire::response_error(&parse_json(r#"{"errcode":"0"}"#).unwrap()).is_some());
    let mut units = vec![97; 3999];
    units.extend([0xd83d, 0xde00]);
    let body = wire::markdown(&JsString::from_utf16(units));
    let content = body
        .get("markdown")
        .unwrap()
        .get("content")
        .unwrap()
        .as_string()
        .unwrap();
    assert_eq!(content.units().len(), 4000);
    assert_eq!(content.units()[3999], 0xd83d);
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
                parse_json(r#"{"type":"assistant_text_delta","text":"答复"}"#).unwrap(),
                parse_json(r#"{"type":"tool_call_finished","toolName":"mock","ok":false}"#)
                    .unwrap(),
            ]))) as Box<dyn TurnEventStream>)
        })
    }
}
fn options(url: String) -> WeComChannelOptions {
    let mut extra = JsObject::new();
    extra.insert("secret".into(), JsValue::from("secret"));
    extra.insert("websocket_url".into(), JsValue::from(url.as_str()));
    extra.insert("dm_policy".into(), JsValue::from("open"));
    extra.insert("text_batch_delay_ms".into(), JsValue::Number(0.0));
    WeComChannelOptions {
        bot_key: Some("fixture".into()),
        extra: Some(extra),
        env: Some(Arc::new(|_| None)),
        ..Default::default()
    }
}
#[tokio::test]
async fn real_websocket_subscribe_reply_correlation_and_turn_payload_match_ts() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let (ready_tx, ready_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let subscribe = ws.next().await.unwrap().unwrap();
        let subscribe = parse_json(subscribe.to_text().unwrap()).unwrap();
        assert_eq!(string(&subscribe, "cmd"), JsString::from("aibot_subscribe"));
        assert_eq!(
            string(subscribe.get("body").unwrap(), "bot_id"),
            JsString::from("fixture")
        );
        ws.send(Message::Text(
            encode(&object([
                ("headers", subscribe.get("headers").unwrap().clone()),
                ("body", object([("errcode", JsValue::Number(0.0))])),
            ]))
            .into(),
        ))
        .await
        .unwrap();
        ws.send(Message::Text(r#"{"cmd":"aibot_msg_callback","headers":{"req_id":"r-1"},"body":{"msgid":"1","from":{"userid":"user"},"msgtype":"text","text":{"content":" hello "}}}"#.into())).await.unwrap();
        let response = ws.next().await.unwrap().unwrap();
        let response = parse_json(response.to_text().unwrap()).unwrap();
        assert_eq!(
            string(&response, "cmd"),
            JsString::from("aibot_respond_msg")
        );
        assert_eq!(payload_id(&response), JsString::from("r-1"));
        let oracle = parse_json(include_str!(
            "../../../tests/fixtures/h1-wecom/expected.json"
        ))
        .unwrap();
        let expected = match oracle.get("sent").unwrap() {
            JsValue::Array(items) => items[1].get("body").unwrap(),
            _ => panic!(),
        };
        assert_eq!(response.get("body").unwrap(), expected);
        ws.send(Message::Text(
            r#"{"headers":{"req_id":"r-1"},"body":{"errcode":0}}"#.into(),
        ))
        .await
        .unwrap();
        ready_tx.send(()).unwrap();
        let _ = release_rx.await;
        assert!(matches!(
            ws.next().await.unwrap().unwrap(),
            Message::Close(_)
        ));
    });
    let turns = Arc::new(Mutex::new(Vec::new()));
    let channel = WeComChannel::new(options(url), js_helper_interaction(Arc::new(Responder)));
    let handle = channel
        .start(ChannelStartDeps {
            turn_gateway: Some(Arc::new(Gateway(turns.clone()))),
            ..Default::default()
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), ready_rx)
        .await
        .unwrap()
        .unwrap();
    {
        let turns = turns.lock().unwrap();
        assert_eq!(turns.len(), 1);
        let request = &turns[0];
        assert_eq!(request.message, JsString::from("hello"));
        assert_eq!(request.session_key, JsString::from("wecom:dm=user:general"));
        assert!(request.attachments.is_none());
        assert_eq!(request.allow_plan_mode_tools, Some(false));
        assert!(request.synthetic_messages.is_some());
    }
    release_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), handle.stop("fixture"))
        .await
        .unwrap()
        .unwrap();
    peer.await.unwrap();
}
#[test]
fn native_aes_and_media_rules_do_not_drop_bytes() {
    use base64::Engine as _;
    use cbc::cipher::{BlockEncryptMut, KeyIvInit, block_padding::NoPadding};
    let key = [7u8; 32];
    let mut bytes = b"hello".to_vec();
    bytes.extend([11u8; 11]);
    let mut encrypted = bytes.clone();
    let cipher = cbc::Encryptor::<aes::Aes256>::new_from_slices(&key, &key[..16])
        .unwrap()
        .encrypt_padded_mut::<NoPadding>(&mut encrypted, bytes.len())
        .unwrap();
    let encoded = base64::engine::general_purpose::STANDARD.encode(key);
    assert_eq!(media::decrypt(cipher, &encoded).unwrap(), b"hello");
    assert!(media::decrypt(&[0; 16], "bad").is_err());
    assert_eq!(media::base64_bytes("aG VsbG8=ignored"), b"hello");
    assert_eq!(media::safe_name("../../a b.pdf"), "a_b.pdf");
}
#[tokio::test]
async fn deliverable_protected_spans_and_sensitive_paths_are_not_read() {
    let text = "`/no-such-file.pdf`\n> MEDIA:/no-such-file.pdf\n```\n/no-such-file.pdf\n```".into();
    let (files, warnings) = deliverables::extract(&text).await;
    assert!(files.is_empty());
    assert!(warnings.is_empty());
    let (files, warnings) = deliverables::extract(&"MEDIA:/tmp/.ssh/private.pdf".into()).await;
    assert!(files.is_empty());
    assert_eq!(
        warnings,
        vec![JsString::from(
            "附件未发送：该路径位于敏感目录或敏感配置文件中。"
        )]
    );
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
    let channel = WeComChannel::new(
        options("ws://unused.invalid".into()),
        js_helper_interaction(Arc::new(Responder)),
    );
    channel.0.state.lock().unwrap().gateway = Some(Arc::new(PendingGateway {
        submitted: submitted.clone(),
        cancel_started: started.clone(),
        release_cancel: release.clone(),
        finished: finished.clone(),
    }));
    let shared = channel.0.clone();
    channel.0.spawn_handler(async move {
        shared
            .dispatch(Inbound {
                chat: "room".into(),
                user: "user".into(),
                group: false,
                interaction: "room".into(),
                text: "hello".into(),
                message_id: "owned".into(),
                attachments: None,
            })
            .await;
    });
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
    let channel = WeComChannel::new(
        options("ws://unused.invalid".into()),
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
async fn reconnect_resubscribes_with_same_device_and_new_request_id() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let (ready_tx, ready_rx) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let mut first_subscribe = None;
        for attempt in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let subscribe = socket.next().await.unwrap().unwrap();
            let subscribe = parse_json(subscribe.to_text().unwrap()).unwrap();
            if let Some(first) = &first_subscribe {
                assert_ne!(payload_id(&subscribe), payload_id(first));
                assert_eq!(subscribe.get("body"), first.get("body"));
            } else {
                first_subscribe = Some(subscribe.clone());
            }
            socket
                .send(Message::Text(
                    encode(&object([
                        ("headers", subscribe.get("headers").unwrap().clone()),
                        ("body", object([("errcode", JsValue::Number(0.0))])),
                    ]))
                    .into(),
                ))
                .await
                .unwrap();
            if attempt == 0 {
                socket.close(None).await.unwrap();
            } else {
                // Auth ACK arrival at the server is earlier than completion
                // of the adapter's connect promise. Confirm a real turn/reply
                // on the reconnected socket before asserting graceful stop.
                socket.send(Message::Text(r#"{"cmd":"aibot_msg_callback","headers":{"req_id":"reconnected-turn"},"body":{"msgid":"reconnected-turn","from":{"userid":"user"},"msgtype":"text","text":{"content":"hello"}}}"#.into())).await.unwrap();
                let reply = socket.next().await.unwrap().unwrap();
                let reply = parse_json(reply.to_text().unwrap()).unwrap();
                assert_eq!(string(&reply, "cmd"), JsString::from("aibot_respond_msg"));
                assert_eq!(payload_id(&reply), JsString::from("reconnected-turn"));
                socket
                    .send(Message::Text(
                        r#"{"headers":{"req_id":"reconnected-turn"},"body":{"errcode":0}}"#.into(),
                    ))
                    .await
                    .unwrap();
                ready_tx.send(()).unwrap();
                assert!(matches!(
                    socket.next().await.unwrap().unwrap(),
                    Message::Close(_)
                ));
                break;
            }
        }
    });
    let channel = WeComChannel::new(options(url), js_helper_interaction(Arc::new(Responder)));
    let handle = channel
        .start(ChannelStartDeps {
            turn_gateway: Some(Arc::new(Gateway(Arc::default()))),
            ..Default::default()
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), ready_rx)
        .await
        .unwrap()
        .unwrap();
    handle.stop("reconnected").await.unwrap();
    peer.await.unwrap();
}
#[test]
fn mapper_state_roundtrip_keeps_property_order_unknown_values_and_utf16() {
    let state=parse_json(r#"{"activeByChatId":{"z":"last","a":"first","bad":7,"\ud800":"lone"},"projectByScopeKey":{"b":"two","a":"one"}}"#).unwrap();
    let mapper = WeComSessionMapper::new(
        WeComSessionMapperState::from_js(&state),
        Arc::new(|| "fixed".into()),
    );
    assert_eq!(mapper.snapshot().to_js(), state);
    assert_eq!(
        stringify_json(&mapper.snapshot().to_js()).unwrap(),
        stringify_json(&state).unwrap()
    );
}
struct MediaGateway(Arc<Mutex<Vec<TurnRequest>>>);
impl TurnGateway for MediaGateway {
    fn submit(
        &self,
        request: TurnRequest,
    ) -> BoxFuture<Result<Box<dyn TurnEventStream>, ChannelError>> {
        self.0.lock().unwrap().push(request);
        Box::pin(async {
            Ok(Box::new(Events(VecDeque::from([parse_json(r#"{"type":"tool_call_finished","ok":true,"toolCallId":"tool","images":[{"data":"eA==","mimeType":"image/png"}]}"#).unwrap()])))as Box<dyn TurnEventStream>)
        })
    }
}
struct Scratch(std::path::PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[tokio::test]
async fn inbound_aes_and_tool_image_upload_wire_match_fixed_ts_oracle() {
    use base64::Engine as _;
    use cbc::cipher::{BlockEncryptMut, KeyIvInit, block_padding::NoPadding};
    let scratch =
        Scratch(std::env::temp_dir().join(format!("pilotdeck-h1-wecom-{}", uuid::Uuid::new_v4())));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let (ready_tx, ready_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let key = [7u8; 32];
    let mut bytes = b"hello".to_vec();
    bytes.extend([27u8; 27]);
    let len = bytes.len();
    let encrypted = cbc::Encryptor::<aes::Aes256>::new_from_slices(&key, &key[..16])
        .unwrap()
        .encrypt_padded_mut::<NoPadding>(&mut bytes, len)
        .unwrap();
    let incoming=serde_json::json!({"cmd":"aibot_msg_callback","headers":{"req_id":"incoming-media"},"body":{"msgid":"media-in","from":{"userid":"user"},"msgtype":"image","image":{"base64":base64::engine::general_purpose::STANDARD.encode(encrypted),"aeskey":base64::engine::general_purpose::STANDARD_NO_PAD.encode(key),"filename":"a b.png","content_type":"image/png"}}}).to_string();
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let subscribe = socket.next().await.unwrap().unwrap();
        let subscribe = parse_json(subscribe.to_text().unwrap()).unwrap();
        socket
            .send(Message::Text(
                encode(&object([
                    ("headers", subscribe.get("headers").unwrap().clone()),
                    ("body", object([("errcode", JsValue::Number(0.0))])),
                ]))
                .into(),
            ))
            .await
            .unwrap();
        socket.send(Message::Text(incoming.into())).await.unwrap();
        let oracle = parse_json(include_str!(
            "../../../tests/fixtures/h1-wecom/media-expected.json"
        ))
        .unwrap();
        let expected = if let Some(JsValue::Array(items)) = oracle.get("sent") {
            items
        } else {
            panic!()
        };
        for (index, expected) in expected.iter().enumerate().skip(1) {
            let message = socket.next().await.unwrap().unwrap();
            let message = parse_json(message.to_text().unwrap()).unwrap();
            assert_eq!(string(&message, "cmd"), string(expected, "cmd"));
            assert_eq!(message.get("body"), expected.get("body"));
            if index == 4 {
                assert_eq!(payload_id(&message), JsString::from("incoming-media"));
            }
            let body = match index {
                1 => object([
                    ("errcode", JsValue::Number(0.0)),
                    ("upload_id", JsValue::from("upload")),
                ]),
                3 => object([
                    ("errcode", JsValue::Number(0.0)),
                    ("media_id", JsValue::from("media")),
                ]),
                _ => object([("errcode", JsValue::Number(0.0))]),
            };
            socket
                .send(Message::Text(
                    encode(&object([
                        ("headers", message.get("headers").unwrap().clone()),
                        ("body", body),
                    ]))
                    .into(),
                ))
                .await
                .unwrap();
        }
        ready_tx.send(()).unwrap();
        let _ = release_rx.await;
        assert!(matches!(
            socket.next().await.unwrap().unwrap(),
            Message::Close(_)
        ));
    });
    let turns = Arc::new(Mutex::new(Vec::new()));
    let mut config = options(url);
    config.media_cache_dir = Some(scratch.0.clone());
    let channel = WeComChannel::new(config, js_helper_interaction(Arc::new(Responder)));
    let handle = channel
        .start(ChannelStartDeps {
            turn_gateway: Some(Arc::new(MediaGateway(turns.clone()))),
            ..Default::default()
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(4), ready_rx)
        .await
        .unwrap()
        .unwrap();
    let mut attachment = {
        let turns = turns.lock().unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].message, JsString::from("用户发送了企业微信附件。"));
        turns[0].attachments.as_ref().unwrap()[0].clone()
    };
    let path = String::from_utf16_lossy(string(&attachment, "path").units());
    assert_eq!(tokio::fs::read(&path).await.unwrap(), b"hello");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    if let JsValue::Object(object) = &mut attachment {
        object.insert("path".into(), JsValue::from("<TMPDIR>/wecom-media"));
    }
    let oracle = parse_json(include_str!(
        "../../../tests/fixtures/h1-wecom/media-expected.json"
    ))
    .unwrap();
    let expected = if let Some(JsValue::Array(turns)) = oracle.get("turns") {
        if let Some(JsValue::Array(items)) = turns[0].get("attachments") {
            &items[0]
        } else {
            panic!()
        }
    } else {
        panic!()
    };
    assert_eq!(&attachment, expected);
    release_tx.send(()).unwrap();
    handle.stop("media").await.unwrap();
    peer.await.unwrap();
}
#[tokio::test]
async fn public_document_send_keeps_work_owned_when_the_caller_drops_its_receipt() {
    let scratch = Scratch(
        std::env::temp_dir().join(format!("pilotdeck-h1-wecom-api-{}", uuid::Uuid::new_v4())),
    );
    tokio::fs::create_dir_all(&scratch.0).await.unwrap();
    let path = scratch.0.join("source.png");
    tokio::fs::write(&path, b"x").await.unwrap();
    let channel = WeComChannel::new(
        options("ws://unused.invalid".into()),
        js_helper_interaction(Arc::new(Responder)),
    );
    let (sender, mut frames) = mpsc::unbounded_channel();
    *channel.0.writer.lock().unwrap() = Some(sender);
    let shared = channel.0.clone();
    let (completed_tx, completed_rx) = oneshot::channel();
    let peer = tokio::spawn(async move {
        for index in 0..5 {
            let frame = frames.recv().await.unwrap();
            let cmd = string(&frame, "cmd");
            let body = frame.get("body").unwrap();
            let response = match index {
                0 => {
                    assert!(cmd.equals_str("aibot_upload_media_init"));
                    assert_eq!(string(body, "type"), JsString::from("file"));
                    assert_eq!(string(body, "filename"), JsString::from("x_.pdf"));
                    object([
                        ("errcode", JsValue::Number(0.0)),
                        ("upload_id", JsValue::from("upload")),
                    ])
                }
                1 => {
                    assert!(cmd.equals_str("aibot_upload_media_chunk"));
                    assert_eq!(string(body, "base64_data"), JsString::from("eA=="));
                    object([("errcode", JsValue::Number(0.0))])
                }
                2 => {
                    assert!(cmd.equals_str("aibot_upload_media_finish"));
                    object([
                        ("errcode", JsValue::Number(0.0)),
                        ("media_id", JsValue::from("media")),
                    ])
                }
                3 => {
                    assert!(cmd.equals_str("aibot_send_msg"));
                    assert_eq!(string(body, "chatid"), JsString::from("room"));
                    assert_eq!(string(body, "msgtype"), JsString::from("file"));
                    object([("errcode", JsValue::Number(0.0))])
                }
                _ => {
                    assert!(cmd.equals_str("aibot_send_msg"));
                    assert_eq!(
                        body.get("markdown").map(|v| string(v, "content")),
                        Some("caption".into())
                    );
                    object([("errcode", JsValue::Number(0.0))])
                }
            };
            shared
                .pending
                .lock()
                .unwrap()
                .remove(&payload_id(&frame))
                .unwrap()
                .send(Ok(object([("body", response)])))
                .unwrap();
        }
        completed_tx.send(()).unwrap();
    });
    let receipt = channel.send_document(
        "room".into(),
        path.to_string_lossy().as_ref().into(),
        Some("caption".into()),
        Some("../x ??.pdf".into()),
        None,
    );
    drop(receipt);
    tokio::time::timeout(Duration::from_secs(3), completed_rx)
        .await
        .unwrap()
        .unwrap();
    Handle(channel.0.clone()).stop("api").await.unwrap();
    peer.await.unwrap();
}
#[tokio::test]
async fn upload_rejection_returns_false_and_does_not_send_later_chunks() {
    let scratch = Scratch(std::env::temp_dir().join(format!(
        "pilotdeck-h1-wecom-reject-{}",
        uuid::Uuid::new_v4()
    )));
    tokio::fs::create_dir_all(&scratch.0).await.unwrap();
    let path = scratch.0.join("source.amr");
    tokio::fs::write(&path, b"x").await.unwrap();
    let channel = WeComChannel::new(
        options("ws://unused.invalid".into()),
        js_helper_interaction(Arc::new(Responder)),
    );
    let (sender, mut frames) = mpsc::unbounded_channel();
    *channel.0.writer.lock().unwrap() = Some(sender);
    let shared = channel.0.clone();
    let peer = tokio::spawn(async move {
        let frame = frames.recv().await.unwrap();
        assert!(string(&frame, "cmd").equals_str("aibot_upload_media_init"));
        shared
            .pending
            .lock()
            .unwrap()
            .remove(&payload_id(&frame))
            .unwrap()
            .send(Ok(parse_json(
                r#"{"body":{"errcode":400,"errmsg":"rejected"}}"#,
            )
            .unwrap()))
            .unwrap();
        frames
    });
    assert!(
        !channel
            .send_voice(
                "room".into(),
                path.to_string_lossy().as_ref().into(),
                None,
                None
            )
            .await
    );
    let mut frames = peer.await.unwrap();
    assert!(frames.try_recv().is_err());
    Handle(channel.0.clone()).stop("rejected").await.unwrap();
}

#[test]
fn safe_upload_names_match_basename_and_collapsed_invalid_runs() {
    assert_eq!(media::safe_name("../x ??.pdf"), "x_.pdf");
    assert_eq!(media::safe_name("a???   b.png"), "a_b.png");
    assert_eq!(media::safe_name(""), "wecom_media");
    assert_eq!(media::safe_name("/tmp/"), "tmp");
    assert_eq!(media::safe_name(".."), "..");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_cannot_finish_between_a_handlers_first_effect_and_registration() {
    let channel = WeComChannel::new(
        options("ws://unused.invalid".into()),
        js_helper_interaction(Arc::new(Responder)),
    );
    let (entered, entered_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shared = channel.0.clone();
    let owner = shared.clone();
    let done = completed.clone();
    let producer = tokio::task::spawn_blocking(move || {
        shared.spawn_handler(async move {
            entered.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            owner.stopped().await;
            done.store(true, std::sync::atomic::Ordering::Release);
        })
    });
    entered_rx.await.unwrap();
    let registered = channel.0.handlers.lock().unwrap().len();
    let handle = Handle(channel.0.clone());
    let mut stop = tokio::spawn(handle.stop("admission barrier"));
    let premature = tokio::time::timeout(Duration::from_millis(25), &mut stop)
        .await
        .is_ok();
    release.send(()).unwrap();
    producer.await.unwrap();
    if !premature {
        stop.await.unwrap().unwrap();
    }
    assert_eq!(
        registered, 1,
        "the owner must be reserved before running handler effects"
    );
    assert!(!premature, "stop must wait for the reserved owner");
    assert!(completed.load(std::sync::atomic::Ordering::Acquire));
}
#[test]
fn malformed_exact_group_does_not_bypass_wildcard_sender_policy() {
    let mut config = options("ws://unused.invalid".into());
    let extra = config.extra.as_mut().unwrap();
    extra.insert("group_policy".into(), JsValue::from("open"));
    extra.insert(
        "groups".into(),
        parse_json(r#"{"room":false,"*":{"allow_from":["admin"]}}"#).unwrap(),
    );
    extra.insert(
        "group_sessions_per_user".into(),
        JsValue::from("wrong type"),
    );
    extra.insert("groupSessionsPerUser".into(), JsValue::Bool(false));
    let channel = WeComChannel::new(config, js_helper_interaction(Arc::new(Responder)));
    assert!(!channel.0.allowed(&"room".into(), &"user".into(), true));
    assert!(channel.0.allowed(&"room".into(), &"admin".into(), true));
    assert!(!channel.0.per_user);
}
#[test]
fn access_lists_do_not_collapse_distinct_lone_utf16_identities() {
    let left = JsString::from_utf16(vec![0xd800]);
    let right = JsString::from_utf16(vec![0xd801]);
    assert!(!wire::entry_matches(std::slice::from_ref(&left), &right));
    assert!(wire::entry_matches(std::slice::from_ref(&left), &left));
    assert!(!wire::equal_folded(&left, &right));
    assert_eq!(
        wire::list(Some(&JsValue::String(JsString::from_utf16(vec![
            0xd800, 44, 0xd801
        ])))),
        vec![left, right]
    );
}

#[cfg(unix)]
#[derive(Debug)]
struct UpdatePidIdentity {
    pid: i32,
    ppid: i32,
    pgid: i32,
    birth: String,
    state: String,
    command: String,
}
#[cfg(unix)]
fn update_pid_identity(pid: i32) -> std::io::Result<Option<UpdatePidIdentity>> {
    let output = std::process::Command::new("/bin/ps")
        .args([
            "-p",
            &pid.to_string(),
            "-o",
            "pid=,ppid=,pgid=,lstart=,stat=,command=",
        ])
        .output()?;
    if !output.status.success() {
        return if output.status.code() == Some(1) {
            Ok(None)
        } else {
            Err(std::io::Error::other("ps process identity failed"))
        };
    }
    let text = String::from_utf8(output.stdout).map_err(std::io::Error::other)?;
    let fields = text.split_whitespace().collect::<Vec<_>>();
    if fields.len() < 10 {
        return Err(std::io::Error::other("truncated ps process identity"));
    }
    let number = |index: usize| fields[index].parse::<i32>().map_err(std::io::Error::other);
    let identity = UpdatePidIdentity {
        pid: number(0)?,
        ppid: number(1)?,
        pgid: number(2)?,
        birth: fields[3..8].join(" "),
        state: fields[8].into(),
        command: fields[9..].join(" "),
    };
    if identity.pid != pid || pid <= 0 {
        return Err(std::io::Error::other("wrong ps process identity"));
    }
    Ok(Some(identity))
}
#[cfg(unix)]
struct ExactUpdateCleanup(std::path::PathBuf);
#[cfg(unix)]
impl Drop for ExactUpdateCleanup {
    fn drop(&mut self) {
        let recorded = std::fs::read(self.0.join("update-identities.json"))
            .ok()
            .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok());
        for file in ["update-child", "update-leader"] {
            let Some(pid) = std::fs::read_to_string(self.0.join(file))
                .ok()
                .and_then(|raw| raw.trim().parse::<i32>().ok())
                .filter(|pid| *pid > 0)
            else {
                continue;
            };
            let Some(identity) = update_pid_identity(pid).ok().flatten() else {
                continue;
            };
            let recorded_match = recorded
                .as_ref()
                .and_then(|records| records.as_array())
                .is_some_and(|records| {
                    records.iter().any(|record| {
                        record["pid"].as_i64() == Some(i64::from(pid))
                            && record["pgid"].as_i64() == Some(i64::from(identity.pgid))
                            && record["birth"].as_str() == Some(identity.birth.as_str())
                            && record["command"].as_str() == Some(identity.command.as_str())
                    })
                });
            let own_unrecorded_leader = file == "update-leader"
                && identity.pgid == pid
                && identity.command.contains(self.0.to_string_lossy().as_ref());
            if !identity.state.starts_with('Z')
                && (recorded_match || own_unrecorded_leader)
                && let Some(pid) = rustix::process::Pid::from_raw(pid)
            {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            }
        }
    }
}
#[cfg(unix)]
#[tokio::test]
async fn update_stop_drains_only_its_process_group_and_inherited_pipes() {
    use std::{
        fs,
        os::unix::{fs::PermissionsExt, process::CommandExt},
        process::Command,
    };
    const CHILD: &str = "PDRS_WECOM_UPDATE_TEST_ROOT";
    if let Some(root) = std::env::var_os(CHILD) {
        let root = std::path::PathBuf::from(root);
        struct Bystander(std::process::Child);
        impl Drop for Bystander {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut command = Command::new("/bin/sleep");
        command.arg("300").process_group(0);
        let mut bystander = Bystander(command.spawn().unwrap());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let subscribe = socket.next().await.unwrap().unwrap();
            let subscribe = parse_json(subscribe.to_text().unwrap()).unwrap();
            socket
                .send(Message::Text(
                    encode(&object([
                        ("headers", subscribe.get("headers").unwrap().clone()),
                        ("body", object([("errcode", JsValue::Number(0.0))])),
                    ]))
                    .into(),
                ))
                .await
                .unwrap();
            socket.send(Message::Text(r#"{"cmd":"aibot_msg_callback","headers":{"req_id":"update-command"},"body":{"msgid":"update-command","from":{"userid":"user"},"msgtype":"text","text":{"content":"/update"}}}"#.into())).await.unwrap();
            let announcement = socket.next().await.unwrap().unwrap();
            let announcement = parse_json(announcement.to_text().unwrap()).unwrap();
            assert!(string(&announcement, "cmd").equals_str("aibot_respond_msg"));
            let text = announcement
                .get("body")
                .unwrap()
                .get("markdown")
                .map(|body| string(body, "content"))
                .unwrap();
            assert!(String::from_utf16_lossy(text.units()).starts_with("⏳ "));
            socket
                .send(Message::Text(
                    r#"{"headers":{"req_id":"update-command"},"body":{"errcode":0}}"#.into(),
                ))
                .await
                .unwrap();
            // After cancellation there must be no synthetic failure/success
            // reply. The next and only frame is the owned socket close.
            assert!(matches!(
                socket.next().await.unwrap().unwrap(),
                Message::Close(_)
            ));
        });
        let mut config = options(url);
        config.command_host = Some(Arc::new(
            crate::channel::commands::host::NativeCommandsHost::new(
                root.join("pilot"),
                root.clone(),
                Some(root.join("bin/pilotdeck")),
                Some(std::time::Instant::now()),
            ),
        ));
        let channel = WeComChannel::new(config, js_helper_interaction(Arc::new(Responder)));
        let handle = channel.start(Default::default()).await.unwrap();
        let (leader,descendant,identities)=tokio::time::timeout(Duration::from_secs(5),async {
            loop {
                let read_pid=|file:&str|fs::read_to_string(root.join(file)).ok().and_then(|raw|raw.trim().parse::<i32>().ok()).filter(|pid|*pid>0);
                if let (Some(leader),Some(descendant))=(read_pid("update-leader"),read_pid("update-child"))
                    && let (Some(parent),Some(child))=(update_pid_identity(leader).unwrap(),update_pid_identity(descendant).unwrap())
                    && parent.pgid==leader && child.pgid==leader && child.ppid==leader && !parent.state.starts_with('Z') && !child.state.starts_with('Z') {
                    let records=serde_json::json!([{"pid":leader,"pgid":parent.pgid,"birth":parent.birth,"command":parent.command},{"pid":descendant,"pgid":child.pgid,"birth":child.birth,"command":child.command}]);
                    break (leader,descendant,records);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.expect("positive PID files and owned process identities before stop");
        fs::write(
            root.join("update-identities.json"),
            serde_json::to_vec(&identities).unwrap(),
        )
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), handle.stop("held updater"))
            .await
            .expect("channel stop awaits actual updater kill/reap/pipe drain")
            .unwrap();
        // Leader wait() is synchronous in the updater owner before receipt.
        // Descendant inherited both pipes, so keeping it alive cannot pass.
        assert!(
            update_pid_identity(leader).unwrap().is_none(),
            "updater leader must already be reaped at stop receipt"
        );
        if let Some(child) = update_pid_identity(descendant).unwrap() {
            assert!(
                child.state.starts_with('Z'),
                "owned descendant still live at stop receipt: {child:?}"
            );
        }
        assert!(
            bystander.0.try_wait().unwrap().is_none(),
            "unrelated process must survive"
        );
        assert!(channel.0.handlers.lock().unwrap().is_empty());
        peer.await.unwrap();
        return;
    }
    let scratch = Scratch(
        std::env::temp_dir().join(format!("pilotdeck-wecom-update-{}", uuid::Uuid::new_v4())),
    );
    let root = &scratch.0;
    let _cleanup = ExactUpdateCleanup(root.clone());
    for directory in ["bin", "scripts", "home", "pilot"] {
        fs::create_dir_all(root.join(directory)).unwrap();
    }
    fs::write(root.join("scripts/update-web.mjs"), "// local mock only\n").unwrap();
    fs::write(root.join("bin/pilotdeck"), "").unwrap();
    fs::write(root.join("bin/node"),"#!/bin/sh\n/bin/sleep 300 &\nprintf '%s' \"$$\" > update-leader\nprintf '%s' \"$!\" > update-child\nprintf 'held stdout\\n'\nprintf 'held stderr\\n' >&2\nwait\n").unwrap();
    fs::set_permissions(root.join("bin/node"), fs::Permissions::from_mode(0o755)).unwrap();
    let exact = format!(
        "{}::update_stop_drains_only_its_process_group_and_inherited_pipes",
        module_path!().split_once("::").unwrap().1
    );
    let mut process = tokio::process::Command::new(std::env::current_exe().unwrap());
    process
        .args(["--exact", &exact, "--nocapture"])
        .env(CHILD, root)
        .env("HOME", root.join("home"))
        .env("PILOT_HOME", root.join("pilot"))
        .env("PATH", root.join("bin"))
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(15), process.output())
        .await
        .expect("isolated update test deadline")
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
