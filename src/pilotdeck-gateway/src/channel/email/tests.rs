use super::*;

#[test]
fn fixed_ts_mapper_body_and_render_oracle() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/h1-email/expected.json"
    ))
    .unwrap();
    let mut mapper = EmailSessionMapper::new(
        EmailSessionMapperState::default(),
        Arc::new(|| "fixed".into()),
    );
    let inputs = [" hi ", "/new hi", "next", "/new\tignored", "/new", "after"];
    for (input, expected) in inputs.iter().zip(fixture["mapped"].as_array().unwrap()) {
        let actual = mapper.resolve(&"a@example.test".into(), &(*input).into());
        assert_eq!(
            actual.session_key,
            JsString::from(expected["sessionKey"].as_str().unwrap())
        );
        assert_eq!(
            actual.message,
            JsString::from(expected["message"].as_str().unwrap())
        );
        assert_eq!(actual.command_new, expected.get("command").is_some());
    }
    for case in fixture["extracted"].as_array().unwrap() {
        assert_eq!(
            body::extract_plain_text(case["raw"].as_str().unwrap()),
            JsString::from(case["text"].as_str().unwrap())
        );
    }
    for case in fixture["rendered"].as_array().unwrap() {
        let event = pilotdeck_protocol::parse_json(&case["event"].to_string()).unwrap();
        assert_eq!(
            sms::render::render_sms_event(&event),
            case["text"].as_str().map(JsString::from)
        );
    }
}

#[test]
fn mapper_preserves_lone_utf16_chat_and_state() {
    let chat = JsString::from_utf16(vec![0xd800]);
    let mut mapper =
        EmailSessionMapper::new(EmailSessionMapperState::default(), Arc::new(|| "id".into()));
    let mapped = mapper.resolve(&chat, &" /new hello ".into());
    assert!(mapped.session_key.units().contains(&0xd800));
    assert_eq!(
        mapper.snapshot().active_by_chat_id[&chat],
        mapped.session_key
    );
    assert_eq!(
        mapper.resolve(&chat, &"next".into()).session_key,
        mapped.session_key
    );
}

struct Responder;
impl crate::channel::im_interaction::InteractionResponder for Responder {
    fn respond_elicitation(
        &self,
        _: sms::ElicitationResponse,
    ) -> BoxFuture<Result<(), ChannelError>> {
        Box::pin(async { panic!("unexpected elicitation answer") })
    }
    fn permission_decide(
        &self,
        _: sms::PermissionDecision,
    ) -> BoxFuture<Result<sms::PermissionDecisionOutcome, ChannelError>> {
        Box::pin(async { panic!("unexpected permission answer") })
    }
}
struct Gateway(Arc<Mutex<Vec<TurnRequest>>>);
struct Events(Vec<JsValue>);
impl TurnGateway for Gateway {
    fn submit(
        &self,
        request: TurnRequest,
    ) -> BoxFuture<Result<Box<dyn TurnEventStream>, ChannelError>> {
        self.0.lock().unwrap().push(request);
        Box::pin(async {
            Ok(Box::new(Events(vec![
                pilotdeck_protocol::parse_json(r#"{"type":"assistant_text_delta","text":"reply"}"#)
                    .unwrap(),
            ])) as Box<dyn TurnEventStream>)
        })
    }
}
impl TurnEventStream for Events {
    fn next(
        &mut self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<JsValue>, ChannelError>> + Send + '_>,
    > {
        Box::pin(async { Ok(self.0.pop()) })
    }
}

#[tokio::test]
async fn configured_adapter_starts_submits_sends_and_stops_on_local_mail_servers() {
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
    };
    let imap = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let smtp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let imap_port = imap.local_addr().unwrap().port();
    let smtp_port = smtp.local_addr().unwrap().port();
    let imap_task = tokio::spawn(async move {
        let (stream, _) = imap.accept().await.unwrap();
        let mut socket = BufReader::new(stream);
        socket.write_all(b"* OK mock\r\n").await.unwrap();
        let mut fetched = false;
        loop {
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            assert!(!line.is_empty());
            let (tag, command) = line.trim_end().split_once(' ').unwrap();
            let response = if command == "CAPABILITY" {
                "* CAPABILITY IMAP4rev1\r\n".into()
            } else if command.starts_with("STATUS ") {
                "* STATUS INBOX (UIDNEXT 1)\r\n".into()
            } else if command == "SEARCH UNSEEN" {
                if fetched {
                    "* SEARCH\r\n".into()
                } else {
                    "* SEARCH 1\r\n".into()
                }
            } else if command.starts_with("FETCH ") {
                fetched = true;
                let text = " /new hello ";
                format!(
                    "* 1 FETCH (UID 4 ENVELOPE (NIL NIL ((NIL NIL \"sender\" \"example.test\")) NIL NIL NIL NIL NIL NIL NIL) BODY[] {{{}}}\r\n{text})\r\n",
                    text.len()
                )
            } else {
                String::new()
            };
            socket
                .write_all(format!("{response}{tag} OK done\r\n").as_bytes())
                .await
                .unwrap();
            if command == "LOGOUT" {
                break;
            }
        }
    });
    let (delivered, receipt) = oneshot::channel();
    let smtp_task = tokio::spawn(async move {
        let mut message = None;
        for _ in 0..2 {
            let (stream, _) = smtp.accept().await.unwrap();
            let mut socket = BufReader::new(stream);
            socket.write_all(b"220 mock\r\n").await.unwrap();
            loop {
                let mut line = String::new();
                socket.read_line(&mut line).await.unwrap();
                assert!(!line.is_empty());
                let response = if line.starts_with("EHLO ") {
                    "250-mock\r\n250 AUTH PLAIN\r\n"
                } else if line.starts_with("AUTH ") {
                    "235 ok\r\n"
                } else if line.starts_with("MAIL ") || line.starts_with("RCPT ") {
                    "250 ok\r\n"
                } else if line == "DATA\r\n" {
                    "354 continue\r\n"
                } else if line == "QUIT\r\n" {
                    "221 bye\r\n"
                } else {
                    panic!("unexpected mail command {line:?}")
                };
                socket.write_all(response.as_bytes()).await.unwrap();
                if line == "DATA\r\n" {
                    let mut data = String::new();
                    loop {
                        let mut row = String::new();
                        socket.read_line(&mut row).await.unwrap();
                        if row == ".\r\n" {
                            break;
                        }
                        data.push_str(&row);
                    }
                    message = Some(data);
                    socket.write_all(b"250 accepted\r\n").await.unwrap();
                }
                if line == "QUIT\r\n" {
                    break;
                }
            }
        }
        delivered.send(message.unwrap()).unwrap();
    });
    let extra=pilotdeck_protocol::parse_json(&format!(r#"{{"address":"agent@example.test","password":"mock-password","imapHost":"127.0.0.1","smtpHost":"127.0.0.1","imapPort":{imap_port},"smtpPort":{smtp_port},"imapTls":false,"smtpTls":false,"pollIntervalMs":20}}"#)).unwrap();
    let JsValue::Object(extra) = &extra else {
        panic!("object")
    };
    let adapter = EmailChannel::new(
        EmailChannelOptions {
            extra: extra.clone(),
            mapper: Some(EmailSessionMapper::new(
                EmailSessionMapperState::default(),
                Arc::new(|| "fixed".into()),
            )),
            env: Some(Arc::new(|_| None)),
        },
        js_helper_interaction(Arc::new(Responder)),
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let handle = adapter
        .start(ChannelStartDeps {
            turn_gateway: Some(Arc::new(Gateway(requests.clone()))),
            ..Default::default()
        })
        .await
        .unwrap();
    let mail = tokio::time::timeout(Duration::from_secs(5), receipt)
        .await
        .unwrap()
        .unwrap();
    handle.stop("test").await.unwrap();
    smtp_task.await.unwrap();
    imap_task.await.unwrap();
    assert!(mail.contains("To: sender@example.test\r\n"));
    assert!(mail.contains("Subject: Message\r\n"));
    assert!(mail.ends_with("\r\n\r\nreply\r\n"));
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].channel_key, ChannelKey::new("email"));
    assert_eq!(
        requests[0].session_key,
        JsString::from("email:chat=sender@example.test:s_fixed")
    );
    assert_eq!(requests[0].message, JsString::from("hello"));
}

struct PendingGateway {
    accepted: Arc<tokio::sync::Notify>,
    cancellations: Arc<std::sync::atomic::AtomicUsize>,
}
struct PendingEvents(Arc<std::sync::atomic::AtomicUsize>);
impl TurnGateway for PendingGateway {
    fn submit(&self, _: TurnRequest) -> BoxFuture<Result<Box<dyn TurnEventStream>, ChannelError>> {
        let events = PendingEvents(self.cancellations.clone());
        let accepted = self.accepted.clone();
        Box::pin(async move {
            accepted.notify_one();
            Ok(Box::new(events) as Box<dyn TurnEventStream>)
        })
    }
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
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}
#[tokio::test]
async fn stop_receipt_owns_and_drains_only_its_accepted_turn() {
    let adapter = EmailChannel::new(
        EmailChannelOptions {
            env: Some(Arc::new(|_| None)),
            ..Default::default()
        },
        js_helper_interaction(Arc::new(Responder)),
    );
    let accepted = Arc::new(tokio::sync::Notify::new());
    let cancellations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    adapter.0.state.lock().unwrap().gateway = Some(Arc::new(PendingGateway {
        accepted: accepted.clone(),
        cancellations: cancellations.clone(),
    }));
    adapter.0.running.store(true, Ordering::Release);
    adapter.0.outbound.lock().unwrap().accepting = true;
    let shared = adapter.0.clone();
    let handler = tokio::spawn(async move {
        shared
            .incoming("sender@example.test".into(), "hello".into())
            .await;
    });
    adapter.0.handlers.lock().unwrap().push(handler);
    accepted.notified().await;
    let stop = Handle(adapter.0.clone()).stop("test cancellation");
    // Stop admission is synchronous even before polling its cleanup receipt.
    assert!(!adapter.0.running.load(Ordering::Acquire));
    tokio::time::timeout(Duration::from_secs(5), stop)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cancellations.load(Ordering::SeqCst), 1);
    assert!(adapter.0.active.lock().unwrap().is_empty());
    assert!(adapter.0.handlers.lock().unwrap().is_empty());
    assert!(
        !adapter
            .0
            .send_reply(&"sender@example.test".into(), &"late".into())
            .await
    );
}
