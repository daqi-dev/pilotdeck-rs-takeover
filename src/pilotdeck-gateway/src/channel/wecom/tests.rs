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
    let turns = turns.lock().unwrap();
    assert_eq!(turns.len(), 1);
    let request = &turns[0];
    assert_eq!(request.message, JsString::from("hello"));
    assert_eq!(request.session_key, JsString::from("wecom:dm=user:general"));
    assert!(request.attachments.is_none());
    assert_eq!(request.allow_plan_mode_tools, Some(false));
    assert!(request.synthetic_messages.is_some());
    drop(turns);
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
    let handle = channel.start(Default::default()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), ready_rx)
        .await
        .unwrap()
        .unwrap();
    handle.stop("reconnected").await.unwrap();
    peer.await.unwrap();
}
