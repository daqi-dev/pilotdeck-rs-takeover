//! Behavioral tests supplement the formal CLI oracle in h1-homeassistant/.
use std::{
    collections::VecDeque,
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
};

use futures_util::{SinkExt, StreamExt};
use pilotdeck_protocol::parse_json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::{net::TcpListener, sync::Notify};
use tokio_tungstenite::{accept_async, tungstenite::Message};

use super::*;
use crate::channel::{
    im_interaction::InteractionResponder,
    protocol::TurnEventStream,
    sms::{ElicitationResponse, PermissionDecision, PermissionDecisionOutcome},
};

fn value(raw: &str) -> JsValue {
    parse_json(raw).expect("fixture JSON")
}
fn rendered(value: &JsValue) -> String {
    stringify_json(value).unwrap().unwrap()
}

#[derive(Default)]
struct Responder {
    elicitation: Mutex<Vec<ElicitationResponse>>,
    permissions: Mutex<Vec<PermissionDecision>>,
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
        self.permissions.lock().unwrap().push(decision);
        Box::pin(async { Ok(PermissionDecisionOutcome { delivered: true }) })
    }
}

struct Gateway {
    requests: Arc<Mutex<Vec<TurnRequest>>>,
    events: Vec<JsValue>,
    release: Option<Arc<Notify>>,
    cancelled: Arc<AtomicUsize>,
    started: Arc<AtomicUsize>,
    fail_cancel_once: bool,
    drain_gate: Option<Arc<Notify>>,
    drain_entered: Arc<AtomicUsize>,
    drained: Arc<AtomicUsize>,
    stream_dropped: Arc<AtomicUsize>,
    panic_next: bool,
}
impl TurnGateway for Gateway {
    fn submit(
        &self,
        request: TurnRequest,
    ) -> BoxFuture<Result<Box<dyn TurnEventStream>, ChannelError>> {
        self.requests.lock().unwrap().push(request);
        let stream = Events {
            events: self.events.clone().into(),
            release: self.release.clone(),
            cancelled: self.cancelled.clone(),
            fail_cancel_once: self.fail_cancel_once,
            drain_gate: self.drain_gate.clone(),
            drain_entered: self.drain_entered.clone(),
            drained: self.drained.clone(),
            stream_dropped: self.stream_dropped.clone(),
            panic_next: self.panic_next,
        };
        let started = self.started.clone();
        Box::pin(async move {
            started.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(stream) as Box<dyn TurnEventStream>)
        })
    }
}
struct Events {
    events: VecDeque<JsValue>,
    release: Option<Arc<Notify>>,
    cancelled: Arc<AtomicUsize>,
    fail_cancel_once: bool,
    drain_gate: Option<Arc<Notify>>,
    drain_entered: Arc<AtomicUsize>,
    drained: Arc<AtomicUsize>,
    stream_dropped: Arc<AtomicUsize>,
    panic_next: bool,
}
impl Drop for Events {
    fn drop(&mut self) {
        self.stream_dropped.fetch_add(1, Ordering::SeqCst);
    }
}
impl TurnEventStream for Events {
    fn next(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<JsValue>, ChannelError>> + Send + '_>> {
        Box::pin(async move {
            assert!(
                !std::mem::take(&mut self.panic_next),
                "fixture event stream panic"
            );
            if let Some(event) = self.events.pop_front() {
                return Ok(Some(event));
            }
            if let Some(release) = self.release.take() {
                release.notified().await;
            }
            Ok(None)
        })
    }
    fn cancel_and_drain(
        &mut self,
        _reason: JsString,
    ) -> Pin<Box<dyn Future<Output = Result<(), ChannelError>> + Send + '_>> {
        self.cancelled.fetch_add(1, Ordering::SeqCst);
        if std::mem::take(&mut self.fail_cancel_once) {
            return Box::pin(async {
                Err(ChannelError::error("fixture cancellation failed once"))
            });
        }
        self.events.clear();
        self.release = None;
        let gate = self.drain_gate.take();
        Box::pin(async move {
            self.drain_entered.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = gate {
                gate.notified().await;
            }
            self.drained.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

fn channel() -> (HomeAssistantChannel, Arc<Responder>) {
    let responder = Arc::new(Responder::default());
    let channel = HomeAssistantChannel::new(
        HomeAssistantChannelOptions {
            token: Some("fixture-token".into()),
            env: Some(Arc::new(|_| None)),
            ..Default::default()
        },
        js_helper_interaction(responder.clone()),
    );
    (channel, responder)
}

fn attach(
    shared: &Arc<Shared>,
    gateway: Option<Arc<dyn TurnGateway>>,
) -> mpsc::UnboundedReceiver<String> {
    let (sender, receiver) = mpsc::unbounded_channel();
    let (stop, _) = watch::channel(false);
    let mut runtime = shared.runtime.lock().unwrap();
    runtime.socket = Some(sender);
    runtime.stop = Some(stop);
    runtime.gateway = gateway;
    receiver
}
async fn outgoing(receiver: &mut mpsc::UnboundedReceiver<String>) -> JsValue {
    value(
        &tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("outbound deadline")
            .expect("outbound open"),
    )
}
fn ack(shared: &Arc<Shared>, frame: &JsValue, success: bool) {
    let id = frame.get("id").unwrap().clone();
    socket::on_message(
        shared,
        &rendered(&object([
            ("type", "result".into()),
            ("id", id),
            ("success", JsValue::Bool(success)),
        ])),
    );
}
fn gateway(events: Vec<JsValue>, release: Option<Arc<Notify>>) -> Arc<Gateway> {
    Arc::new(Gateway {
        requests: Default::default(),
        events,
        release,
        cancelled: Arc::new(AtomicUsize::new(0)),
        started: Arc::new(AtomicUsize::new(0)),
        fail_cancel_once: false,
        drain_gate: None,
        drain_entered: Arc::new(AtomicUsize::new(0)),
        drained: Arc::new(AtomicUsize::new(0)),
        stream_dropped: Arc::new(AtomicUsize::new(0)),
        panic_next: false,
    })
}

#[test]
fn mapper_keeps_utf16_and_only_recognizes_literal_new_space() {
    let mut mapper = HomeAssistantSessionMapper::new(
        Default::default(),
        Arc::new(|| "01234567-89ab-cdef-0123-456789abcdef".into()),
    );
    let chat = JsString::from_utf16(vec![b'x' as u16, 0xd800]);
    let original = mapper.resolve(&chat, &" \u{feff}hello\u{a0}".into());
    assert_eq!(original.message, JsString::from("hello"));
    assert!(original.session_key.units().contains(&0xd800));
    assert!(!mapper.resolve(&chat, &"/new\tother".into()).command_new);
    let new = mapper.resolve(&chat, &" /new  next \u{a0}".into());
    assert!(new.command_new);
    assert_eq!(new.message, JsString::from("next"));
    assert_eq!(
        mapper.resolve(&chat, &"after".into()).session_key,
        new.session_key
    );
    assert_eq!(
        mapper.snapshot().active_by_chat_id.get(&chat),
        Some(&new.session_key)
    );
}

#[test]
fn url_and_state_filter_follow_js_identity_and_stringification() {
    for (input, expected) in [
        ("127.0.0.1:8123", "ws://127.0.0.1:8123/api/websocket"),
        (
            "https://EXAMPLE.invalid/prefix/?q=x#hash",
            "wss://example.invalid/prefix/api/websocket",
        ),
        (
            "http://u:p@example.invalid/a//",
            "ws://example.invalid/a//api/websocket",
        ),
    ] {
        assert_eq!(socket::http_to_ws(&input.into()).unwrap(), expected);
    }
    for (new, old, expected) in [
        ("null", "false", Some("null")),
        ("false", "true", Some("false")),
        ("0", "1", Some("0")),
        ("{\"x\":1}", "{\"x\":1}", Some("{\"x\":1}")),
        ("[]", "[]", Some("[]")),
        ("\"same\"", "\"same\"", None),
        ("\" \\ufeff\"", "\"old\"", None),
    ] {
        let event = value(&format!(
            r#"{{"event":{{"event_type":"state_changed","data":{{"entity_id":"conversation.a","new_state":{{"state":{new}}},"old_state":{{"state":{old}}}}}}}}}"#
        ));
        assert_eq!(
            socket::event_input(&event, &["conversation.".into()]).map(|(_, text)| text),
            expected.map(JsString::from)
        );
        assert!(socket::event_input(&event, &["sensor.".into()]).is_none());
    }
}

#[test]
fn renderer_preserves_fragments_and_failure_name_nullish_semantics() {
    for (event, expected) in [
        (
            r#"{"type":"assistant_text_delta","text":"x\ud800"}"#,
            JsString::from_utf16(vec![120, 0xd800]),
        ),
        (
            r#"{"type":"tool_call_finished","ok":false,"toolName":"","toolCallId":"fallback"}"#,
            "\n⚠️  failed\n".into(),
        ),
        (
            r#"{"type":"tool_call_finished","ok":false,"toolName":null,"toolCallId":"call"}"#,
            "\n⚠️ call failed\n".into(),
        ),
        (
            r#"{"type":"error","message":"failed"}"#,
            "\n❌ failed\n".into(),
        ),
    ] {
        assert_eq!(
            render::render_homeassistant_event(&value(event)),
            Some(expected)
        );
    }
}

#[tokio::test(start_paused = true)]
async fn service_result_requires_matching_numeric_id_and_strict_true_or_times_out() {
    let (channel, _) = channel();
    let shared = channel.shared;
    let mut wire = attach(&shared, None);
    let deliver = {
        let shared = shared.clone();
        tokio::spawn(async move {
            shared
                .send_reply("conversation.a".into(), "中文😀".into())
                .await
        })
    };
    let frame = outgoing(&mut wire).await;
    assert_eq!(
        frame.get("domain"),
        Some(&JsValue::from("persistent_notification"))
    );
    assert_eq!(
        frame.get("service_data").unwrap().get("title"),
        Some(&JsValue::from("Gateway · conversation.a"))
    );
    socket::on_message(&shared, r#"{"type":"result","id":"1","success":true}"#);
    socket::on_message(&shared, r#"{"type":"result","id":99,"success":true}"#);
    tokio::task::yield_now().await;
    assert!(!deliver.is_finished());
    socket::on_message(&shared, r#"{"type":"result","id":1,"success":1}"#);
    assert!(!deliver.await.unwrap());
    let timeout = {
        let shared = shared.clone();
        tokio::spawn(async move {
            shared
                .send_reply("conversation.a".into(), "timeout".into())
                .await
        })
    };
    outgoing(&mut wire).await;
    tokio::time::advance(Duration::from_secs(9)).await;
    assert!(!timeout.is_finished());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(!timeout.await.unwrap());
    assert!(shared.runtime.lock().unwrap().pending.is_empty());
}

#[tokio::test]
async fn close_releases_pending_and_cron_uses_the_same_ack_path() {
    let (channel, _) = channel();
    let mut wire = attach(&channel.shared, None);
    let delivery = CronResultDelivery {
        task_id: "t".into(),
        run_id: "r".into(),
        session_key: "homeassistant:chat=conversation.cron:general".into(),
        channel_key: ChannelKey::new("homeassistant"),
        origin_session_key: None,
        origin_channel_key: None,
        text: "cron-result".into(),
    };
    let task = tokio::spawn(channel.deliver_cron_result(&delivery));
    let frame = outgoing(&mut wire).await;
    ack(&channel.shared, &frame, true);
    assert!(task.await.unwrap());
    let task = tokio::spawn(channel.deliver_cron_result(&delivery));
    outgoing(&mut wire).await;
    channel.shared.disconnect();
    assert!(!task.await.unwrap());
    assert!(
        !channel
            .deliver_cron_result(&CronResultDelivery {
                origin_channel_key: Some(ChannelKey::new("other")),
                ..delivery
            })
            .await
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_frame_admission_precedes_busy_new_and_stop_cancels_owned_stream() {
    let (channel, _) = channel();
    let gateway = gateway(Vec::new(), Some(Arc::new(Notify::new())));
    let _wire = attach(&channel.shared, Some(gateway.clone()));
    channel
        .shared
        .dispatch("conversation.a".into(), "first".into());
    channel
        .shared
        .dispatch("conversation.a".into(), "/new".into());
    channel
        .shared
        .dispatch("conversation.a".into(), "second".into());
    assert_eq!(gateway.requests.lock().unwrap().len(), 1);
    assert_eq!(
        gateway.requests.lock().unwrap()[0].message,
        JsString::from("first")
    );
    assert!(
        channel
            .shared
            .runtime
            .lock()
            .unwrap()
            .mapper
            .snapshot()
            .active_by_chat_id
            .is_empty()
    );
    while gateway.started.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    channel.shared.stop_and_drain().await.unwrap();
    assert_eq!(gateway.cancelled.load(Ordering::SeqCst), 1);
    assert!(channel.shared.runtime.lock().unwrap().active.is_empty());
    channel
        .shared
        .dispatch("conversation.a".into(), "after-stop".into());
    channel.shared.stop_and_drain().await.unwrap();
    assert_eq!(gateway.requests.lock().unwrap().len(), 1);
    assert_eq!(gateway.cancelled.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn elicitation_answer_bypasses_busy_and_keeps_utf16_request_and_option() {
    let (channel, responder) = channel();
    let event = value(
        r#"{"type":"elicitation_request","requestId":"e\ud800","questions":[{"header":"choose","question":"Q","options":[{"label":"a","description":""},{"label":"b\ud800","description":""}]}]}"#,
    );
    let gateway = gateway(vec![event], Some(Arc::new(Notify::new())));
    let mut wire = attach(&channel.shared, Some(gateway.clone()));
    channel
        .shared
        .dispatch("conversation.a".into(), "question".into());
    let prompt = outgoing(&mut wire).await;
    ack(&channel.shared, &prompt, true);
    channel.shared.dispatch("conversation.a".into(), "2".into());
    {
        let responses = responder.elicitation.lock().unwrap();
        assert_eq!(responses.len(), 1);
        assert_eq!(
            responses[0].request_id,
            Some(JsString::from_utf16(vec![101, 0xd800]))
        );
        assert_eq!(
            responses[0].answer.get("answers").unwrap().get("Q"),
            Some(&JsValue::String(JsString::from_utf16(vec![98, 0xd800])))
        );
    }
    assert_eq!(gateway.requests.lock().unwrap().len(), 1);
    channel.shared.stop_and_drain().await.unwrap();
}

#[tokio::test]
async fn permission_failed_prompt_retries_before_accepting_an_answer() {
    let (channel, responder) = channel();
    let event = value(
        r#"{"type":"permission_request","requestId":"p1","toolName":"bash","payload":{"command":"fixture-only"}}"#,
    );
    let release = Arc::new(Notify::new());
    let gateway = gateway(vec![event], Some(release.clone()));
    let mut wire = attach(&channel.shared, Some(gateway));
    channel
        .shared
        .dispatch("conversation.a".into(), "permission".into());
    let first = outgoing(&mut wire).await;
    ack(&channel.shared, &first, false);
    tokio::task::yield_now().await;
    channel.shared.dispatch("conversation.a".into(), "1".into());
    let status = outgoing(&mut wire).await;
    ack(&channel.shared, &status, true);
    let retry = outgoing(&mut wire).await;
    assert_eq!(
        retry.get("service_data").unwrap().get("message"),
        first.get("service_data").unwrap().get("message")
    );
    assert!(responder.permissions.lock().unwrap().is_empty());
    ack(&channel.shared, &retry, true);
    tokio::task::yield_now().await;
    channel.shared.dispatch("conversation.a".into(), "1".into());
    let confirmation = outgoing(&mut wire).await;
    ack(&channel.shared, &confirmation, false);
    assert_eq!(responder.permissions.lock().unwrap().len(), 1);
    assert_eq!(
        responder.permissions.lock().unwrap()[0].request_id,
        Some("p1".into())
    );
    while channel
        .shared
        .interactions
        .permission_pending(&"conversation.a".into())
    {
        tokio::task::yield_now().await;
    }
    release.notify_one();
    while channel
        .shared
        .runtime
        .lock()
        .unwrap()
        .active
        .contains(&JsString::from("conversation.a"))
    {
        tokio::task::yield_now().await;
    }
    channel
        .shared
        .dispatch("conversation.a".into(), "following turn".into());
    let next = outgoing(&mut wire).await;
    assert_eq!(
        next.get("service_data").unwrap().get("message"),
        first.get("service_data").unwrap().get("message")
    );
    channel.shared.stop_and_drain().await.unwrap();
    assert!(
        !channel
            .shared
            .interactions
            .permission_pending(&"conversation.a".into())
    );
}

#[tokio::test]
async fn real_websocket_auth_subscribe_rejection_and_cancelled_start() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (mut channel, _) = channel();
    Arc::get_mut(&mut channel.shared).unwrap().url =
        format!("http://127.0.0.1:{port}/prefix/").as_str().into();
    let start = tokio::spawn(channel.start(ChannelStartDeps::default()));
    let (socket, _) = listener.accept().await.unwrap();
    let mut socket = accept_async(socket).await.unwrap();
    socket
        .send(Message::Text("{\"type\":\"auth_required\"}".into()))
        .await
        .unwrap();
    assert_eq!(
        value(socket.next().await.unwrap().unwrap().to_text().unwrap()),
        value(r#"{"type":"auth","access_token":"fixture-token"}"#)
    );
    socket
        .send(Message::Text("{\"type\":\"auth_ok\"}".into()))
        .await
        .unwrap();
    let subscribe = value(socket.next().await.unwrap().unwrap().to_text().unwrap());
    assert_eq!(
        subscribe,
        value(r#"{"id":1,"type":"subscribe_events","event_type":"state_changed"}"#)
    );
    socket
        .send(Message::Text(
            "{\"type\":\"result\",\"id\":1,\"success\":false}".into(),
        ))
        .await
        .unwrap();
    let handle = start.await.unwrap().unwrap();
    handle.stop("reload").await.unwrap();
    assert!(matches!(
        socket.next().await,
        Some(Ok(Message::Close(_))) | None
    ));

    let start = tokio::spawn(channel.start(ChannelStartDeps::default()));
    let (socket, _) = listener.accept().await.unwrap();
    let mut socket = accept_async(socket).await.unwrap();
    // A completed server handshake alone does not prove the client has left
    // CONNECTING. Its auth response proves the owned socket loop is installed.
    socket
        .send(Message::Text("{\"type\":\"auth_required\"}".into()))
        .await
        .unwrap();
    assert_eq!(
        value(socket.next().await.unwrap().unwrap().to_text().unwrap()),
        value(r#"{"type":"auth","access_token":"fixture-token"}"#)
    );
    start.abort();
    let _ = start.await;
    let observed = tokio::time::timeout(Duration::from_secs(2), socket.next())
        .await
        .unwrap();
    let runtime = channel.shared.runtime.lock().unwrap();
    assert!(
        matches!(&observed, Some(Ok(Message::Close(_))) | None),
        "cancelled-start peer outcome: {observed:?}; closed={}, socket_present={}, network_present={}",
        runtime.closed,
        runtime.socket.is_some(),
        runtime.network.is_some(),
    );
}

#[tokio::test]
async fn cancelled_connecting_start_closes_transport_and_drains_owners() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (mut channel, _) = channel();
    Arc::get_mut(&mut channel.shared).unwrap().url =
        JsValue::String(format!("http://{}", listener.local_addr().unwrap()).into());
    let start = tokio::spawn(channel.start(ChannelStartDeps::default()));
    let (mut peer, _) = listener.accept().await.unwrap();
    // Withhold HTTP 101 after observing the request: cancellation is certainly
    // in CONNECTING, where a WebSocket Close frame cannot be sent.
    let mut request = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !request.ends_with(b"\r\n\r\n") {
            request.push(peer.read_u8().await.unwrap());
        }
    })
    .await
    .unwrap();
    assert!(request.starts_with(b"GET /api/websocket HTTP/1.1\r\n"));
    start.abort();
    assert!(matches!(start.await, Err(error) if error.is_cancelled()));
    let mut byte = [0];
    let outcome = tokio::time::timeout(Duration::from_secs(2), peer.read(&mut byte))
        .await
        .unwrap();
    assert!(
        matches!(&outcome, Ok(0))
            || matches!(&outcome, Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset),
        "CONNECTING transport outcome: {outcome:?}"
    );
    channel.shared.stop_and_drain().await.unwrap();
    let runtime = channel.shared.runtime.lock().unwrap();
    assert!(runtime.closed);
    assert!(runtime.socket.is_none());
    assert!(runtime.network.is_none());
    assert!(runtime.tasks.is_empty());
}

#[tokio::test]
async fn auth_deadline_closes_socket_after_twenty_seconds() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (mut channel, _) = channel();
    Arc::get_mut(&mut channel.shared).unwrap().url =
        JsValue::String(format!("http://{}", listener.local_addr().unwrap()).into());
    let logs = Arc::new(Mutex::new(Vec::new()));
    let log = logs.clone();
    let start = tokio::spawn(channel.start(ChannelStartDeps {
        log: Some(Arc::new(move |_, message| {
            log.lock().unwrap().push(message.to_owned())
        })),
        ..Default::default()
    }));
    let (stream, _) = listener.accept().await.unwrap();
    let mut peer = accept_async(stream).await.unwrap();
    peer.send(Message::Text("{\"type\":\"auth_required\"}".into()))
        .await
        .unwrap();
    peer.next().await.unwrap().unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(19)).await;
    assert!(!start.is_finished());
    tokio::time::advance(Duration::from_secs(2)).await;
    let handle = start.await.unwrap().unwrap();
    assert!(
        logs.lock()
            .unwrap()
            .iter()
            .any(|line| line == "homeassistant: WebSocket auth failed or timed out")
    );
    handle.stop("noop").await.unwrap();
    assert!(channel.shared.runtime.lock().unwrap().socket.is_none());
}

async fn reject_upgrade(listener: &TcpListener) {
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut request = Vec::new();
    let mut buffer = [0; 1024];
    while !request.windows(4).any(|part| part == b"\r\n\r\n") {
        let count = stream.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0, "client closed before its upgrade request");
        request.extend_from_slice(&buffer[..count]);
    }
    assert!(request.starts_with(b"GET /api/websocket HTTP/1.1\r\n"));
    stream
        .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    stream.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejected_upgrade_keeps_auth_deadline_and_error_event_order() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (mut channel, _) = channel();
    Arc::get_mut(&mut channel.shared).unwrap().url =
        JsValue::String(format!("http://{}", listener.local_addr().unwrap()).into());
    let logs = Arc::new(Mutex::new(Vec::new()));
    let log = logs.clone();
    let start = tokio::spawn(channel.start(ChannelStartDeps {
        log: Some(Arc::new(move |_, message| {
            log.lock().unwrap().push(message.to_owned())
        })),
        ..Default::default()
    }));
    reject_upgrade(&listener).await;
    while logs.lock().unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        *logs.lock().unwrap(),
        vec!["homeassistant: ws error: [object ErrorEvent]"]
    );
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(19)).await;
    assert!(!start.is_finished());
    tokio::time::advance(Duration::from_secs(2)).await;
    let handle = start.await.unwrap().unwrap();
    assert_eq!(
        *logs.lock().unwrap(),
        vec![
            "homeassistant: ws error: [object ErrorEvent]",
            "homeassistant: WebSocket auth failed or timed out",
            "homeassistant: ws error: [object ErrorEvent]",
        ]
    );
    handle.stop("noop").await.unwrap();
    assert!(channel.shared.runtime.lock().unwrap().network.is_none());
}

#[tokio::test]
async fn rejected_reconnect_emits_no_close_or_further_attempt() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (mut channel, _) = channel();
    Arc::get_mut(&mut channel.shared).unwrap().url =
        JsValue::String(format!("http://{}", listener.local_addr().unwrap()).into());
    let logs = Arc::new(Mutex::new(Vec::new()));
    let log = logs.clone();
    let start = tokio::spawn(channel.start(ChannelStartDeps {
        log: Some(Arc::new(move |_, message| {
            log.lock().unwrap().push(message.to_owned())
        })),
        ..Default::default()
    }));
    let (stream, _) = listener.accept().await.unwrap();
    let mut peer = accept_async(stream).await.unwrap();
    peer.send(Message::Text("{\"type\":\"auth_ok\"}".into()))
        .await
        .unwrap();
    peer.next().await.unwrap().unwrap();
    let handle = start.await.unwrap().unwrap();
    peer.close(None).await.unwrap();
    while channel.shared.runtime.lock().unwrap().socket.is_some() {
        tokio::task::yield_now().await;
    }
    tokio::time::pause();
    tokio::time::advance(RECONNECT_DELAY).await;
    // The reconnect delay is virtual; the TCP connect and WebSocket upgrade
    // are real. Restore the clock before waiting for I/O so Tokio cannot
    // auto-advance the connector's timeout while the kernel readies the peer.
    tokio::time::resume();
    reject_upgrade(&listener).await;
    while !logs
        .lock()
        .unwrap()
        .iter()
        .any(|line| line == "homeassistant: ws error: [object ErrorEvent]")
    {
        tokio::task::yield_now().await;
    }
    // Observe real loopback acceptance for longer than the retry interval. An
    // incorrectly synthesized close would establish the third connection.
    assert!(
        tokio::time::timeout(
            RECONNECT_DELAY + Duration::from_millis(500),
            listener.accept()
        )
        .await
        .is_err()
    );
    handle.stop("reload").await.unwrap();
    assert_eq!(
        logs.lock()
            .unwrap()
            .iter()
            .filter(
                |line| line.starts_with("homeassistant: ws error:") || line.contains("auth failed")
            )
            .cloned()
            .collect::<Vec<_>>(),
        vec![
            "homeassistant: ws error: [object ErrorEvent]",
            "homeassistant: ws error: [object ErrorEvent]",
        ]
    );
    assert!(channel.shared.runtime.lock().unwrap().network.is_none());
}

#[tokio::test]
async fn reconnect_waits_five_seconds_preserves_ids_and_stop_clears_retry() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (mut channel, _) = channel();
    Arc::get_mut(&mut channel.shared).unwrap().url =
        JsValue::String(format!("http://{}", listener.local_addr().unwrap()).into());
    let start = tokio::spawn(channel.start(ChannelStartDeps::default()));
    let (stream, _) = listener.accept().await.unwrap();
    let mut first = accept_async(stream).await.unwrap();
    first
        .send(Message::Text("{\"type\":\"auth_ok\"}".into()))
        .await
        .unwrap();
    assert_eq!(
        value(first.next().await.unwrap().unwrap().to_text().unwrap()).get("id"),
        Some(&JsValue::Number(1.0))
    );
    let handle = start.await.unwrap().unwrap();
    first.close(None).await.unwrap();
    while channel.shared.runtime.lock().unwrap().socket.is_some() {
        tokio::task::yield_now().await;
    }
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(4)).await;
    let accept = listener.accept();
    tokio::pin!(accept);
    assert!(
        accept
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    let (stream, _) = accept.await.unwrap();
    let mut second = accept_async(stream).await.unwrap();
    second
        .send(Message::Text("{\"type\":\"auth_ok\"}".into()))
        .await
        .unwrap();
    assert_eq!(
        value(second.next().await.unwrap().unwrap().to_text().unwrap()).get("id"),
        Some(&JsValue::Number(2.0))
    );
    second.close(None).await.unwrap();
    while channel.shared.runtime.lock().unwrap().socket.is_some() {
        tokio::task::yield_now().await;
    }
    handle.stop("reload").await.unwrap();
    tokio::time::advance(Duration::from_secs(20)).await;
    let accept = listener.accept();
    tokio::pin!(accept);
    assert!(
        accept
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
}

struct PendingSubmit {
    dropped: Arc<AtomicUsize>,
}
struct AdmissionGuard(Arc<AtomicUsize>);
impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
impl TurnGateway for PendingSubmit {
    fn submit(&self, _: TurnRequest) -> BoxFuture<Result<Box<dyn TurnEventStream>, ChannelError>> {
        let dropped = self.dropped.clone();
        Box::pin(async move {
            let _owned = AdmissionGuard(dropped);
            std::future::pending().await
        })
    }
}

#[tokio::test]
async fn stopping_pending_admission_drops_its_owned_future_and_drains_task() {
    let (channel, _) = channel();
    let dropped = Arc::new(AtomicUsize::new(0));
    let _wire = attach(
        &channel.shared,
        Some(Arc::new(PendingSubmit {
            dropped: dropped.clone(),
        })),
    );
    channel
        .shared
        .dispatch("conversation.a".into(), "pending-admission".into());
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    assert_eq!(dropped.load(Ordering::SeqCst), 0);
    channel.shared.stop_and_drain().await.unwrap();
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert!(channel.shared.runtime.lock().unwrap().active.is_empty());
}

#[tokio::test]
async fn empty_ready_stream_still_yields_before_releasing_busy_chat() {
    let (channel, _) = channel();
    let gateway = gateway(Vec::new(), None);
    let _wire = attach(&channel.shared, Some(gateway.clone()));
    channel
        .shared
        .dispatch("conversation.a".into(), "first".into());
    channel
        .shared
        .dispatch("conversation.a".into(), "second".into());
    assert_eq!(gateway.requests.lock().unwrap().len(), 1);
    assert!(
        channel
            .shared
            .runtime
            .lock()
            .unwrap()
            .active
            .contains(&JsString::from("conversation.a"))
    );
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    channel.shared.stop_and_drain().await.unwrap();
}

#[tokio::test]
async fn stop_closes_admission_before_the_returned_future_is_polled() {
    let (channel, _) = channel();
    let gateway = gateway(Vec::new(), None);
    let _wire = attach(&channel.shared, Some(gateway.clone()));
    let stop = Handle(channel.shared.clone()).stop("reload");
    channel
        .shared
        .dispatch("conversation.a".into(), "too late".into());
    assert!(gateway.requests.lock().unwrap().is_empty());
    stop.await.unwrap();
}

#[tokio::test]
async fn failed_cancel_retains_owned_stream_for_retry_and_only_then_clears_helpers() {
    let (channel, _) = channel();
    let mut gateway = gateway(
        vec![value(
            r#"{"type":"elicitation_request","requestId":"e1","questions":[]}"#,
        )],
        Some(Arc::new(Notify::new())),
    );
    Arc::get_mut(&mut gateway).unwrap().fail_cancel_once = true;
    let mut wire = attach(&channel.shared, Some(gateway.clone()));
    channel
        .shared
        .dispatch("conversation.a".into(), "pending".into());
    outgoing(&mut wire).await;
    assert!(
        channel
            .shared
            .interactions
            .elicitation_pending(&"conversation.a".into())
    );
    assert!(channel.shared.stop_and_drain().await.is_err());
    assert_eq!(
        channel.shared.runtime.lock().unwrap().failed_streams.len(),
        1
    );
    assert!(
        channel
            .shared
            .interactions
            .elicitation_pending(&"conversation.a".into())
    );
    channel.shared.stop_and_drain().await.unwrap();
    assert_eq!(gateway.cancelled.load(Ordering::SeqCst), 2);
    assert!(
        !channel
            .shared
            .interactions
            .elicitation_pending(&"conversation.a".into())
    );
    assert!(
        channel
            .shared
            .runtime
            .lock()
            .unwrap()
            .failed_streams
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compressed_same_write_runs_ready_turns_but_keeps_held_turn_busy() {
    use base64::Engine;
    use tokio_tungstenite::tungstenite::handshake::server::{
        Callback, ErrorResponse, Request, Response,
    };

    struct DeflateHandshake;
    impl Callback for DeflateHandshake {
        fn on_request(
            self,
            request: &Request,
            mut response: Response,
        ) -> Result<Response, ErrorResponse> {
            assert_eq!(
                request.headers()["sec-websocket-extensions"],
                "permessage-deflate; client_max_window_bits"
            );
            response.headers_mut().insert(
                "sec-websocket-extensions",
                "permessage-deflate".parse().unwrap(),
            );
            Ok(response)
        }
    }

    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/h1-homeassistant/compressed-ready.json"
    ))
    .unwrap();
    let batch = base64::engine::general_purpose::STANDARD
        .decode(fixture["wireBase64"].as_str().unwrap())
        .unwrap();
    assert_eq!(batch.len(), 115);
    for round in 0..9 {
        let held = round == 8;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (mut channel, _) = channel();
        Arc::get_mut(&mut channel.shared).unwrap().url =
            JsValue::String(format!("http://{}", listener.local_addr().unwrap()).into());
        let gateway = gateway(Vec::new(), held.then(|| Arc::new(Notify::new())));
        let busy = Arc::new(AtomicUsize::new(0));
        let logged_busy = busy.clone();
        let start = tokio::spawn(channel.start(ChannelStartDeps {
            turn_gateway: Some(gateway.clone()),
            log: Some(Arc::new(move |_, line| {
                if line.contains("already active, skipping") {
                    logged_busy.fetch_add(1, Ordering::SeqCst);
                }
            })),
            ..Default::default()
        }));
        let (stream, _) = listener.accept().await.unwrap();
        let mut peer = tokio_tungstenite::accept_hdr_async(stream, DeflateHandshake)
            .await
            .unwrap();
        peer.send(Message::Text("{\"type\":\"auth_ok\"}".into()))
            .await
            .unwrap();
        peer.next().await.unwrap().unwrap();
        let handle = start.await.unwrap().unwrap();

        // These exact two RSV1 frames were captured together in one native
        // Node TCP read. The second frame uses the first frame's dictionary.
        peer.get_mut().write_all(&batch).await.unwrap();
        peer.send(Message::Ping(b"after-both-frames".to_vec().into()))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            match peer.next().await.unwrap().unwrap() {
                Message::Pong(payload) => {
                    assert_eq!(payload.as_ref(), b"after-both-frames");
                }
                other => panic!("unexpected client frame after compressed input: {other:?}"),
            }
        })
        .await
        .expect("held turn must not block socket progress or its pong");

        // Pong proves socket progress, but can flush before the last handler
        // finishes. Observe actual empty-stream destruction and active release.
        // The held branch deliberately never waits for stream completion.
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let admitted = gateway.requests.lock().unwrap().len();
                let active = channel
                    .shared
                    .runtime
                    .lock()
                    .unwrap()
                    .active
                    .contains(&JsString::from("conversation.ready"));
                let settled = if held {
                    admitted == 1 && active && busy.load(Ordering::SeqCst) == 1
                } else {
                    admitted == 2 && !active && gateway.stream_dropped.load(Ordering::SeqCst) == 2
                };
                if settled {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("real handler completion after compressed input");
        let requests = gateway.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), if held { 1 } else { 2 }, "round {round}");
        assert_eq!(requests[0].message, JsString::from("first"));
        if !held {
            assert_eq!(requests[1].message, JsString::from("second"));
        }
        assert!(requests.iter().all(|request| {
            request.session_key == JsString::from("homeassistant:chat=conversation.ready:general")
        }));
        assert_eq!(busy.load(Ordering::SeqCst), usize::from(held));
        assert_eq!(
            channel
                .shared
                .runtime
                .lock()
                .unwrap()
                .active
                .contains(&JsString::from("conversation.ready")),
            held
        );
        handle.stop("compressed batch complete").await.unwrap();
        assert_eq!(gateway.cancelled.load(Ordering::SeqCst), usize::from(held));
        assert!(channel.shared.runtime.lock().unwrap().tasks.is_empty());
        assert!(channel.shared.runtime.lock().unwrap().network.is_none());
    }
}

fn server_frame(message: &str) -> Vec<u8> {
    let mut frame = vec![0x81];
    if message.len() < 126 {
        frame.push(message.len() as u8);
    } else {
        frame.push(126);
        frame.extend((message.len() as u16).to_be_bytes());
    }
    frame.extend(message.as_bytes());
    frame
}
fn ha_state(text: &str) -> String {
    format!(
        r#"{{"type":"event","event":{{"event_type":"state_changed","data":{{"entity_id":"conversation.batch","new_state":{{"state":"{text}"}},"old_state":{{"state":"old"}}}}}}}}"#
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_single_write_batch_skips_second_ready_turn_but_split_completed_writes_run() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (mut channel, _) = channel();
    Arc::get_mut(&mut channel.shared).unwrap().url =
        JsValue::String(format!("http://{}", listener.local_addr().unwrap()).into());
    let gateway = gateway(Vec::new(), None);
    let start = tokio::spawn(channel.start(ChannelStartDeps {
        turn_gateway: Some(gateway.clone()),
        ..Default::default()
    }));
    let (stream, _) = listener.accept().await.unwrap();
    let mut peer = accept_async(stream).await.unwrap();
    peer.send(Message::Text("{\"type\":\"auth_ok\"}".into()))
        .await
        .unwrap();
    peer.next().await.unwrap().unwrap();
    let handle = start.await.unwrap().unwrap();
    peer.send(Message::Binary(
        ha_state("binary-must-ignore").into_bytes().into(),
    ))
    .await
    .unwrap();
    for round in 0..20 {
        let mut batch = server_frame(&ha_state(&format!("first-{round}")));
        batch.extend(server_frame(&ha_state("must-skip")));
        peer.get_mut().write_all(&batch).await.unwrap();
        while gateway.requests.lock().unwrap().len() <= round
            || !channel.shared.runtime.lock().unwrap().active.is_empty()
        {
            tokio::task::yield_now().await;
        }
        assert_eq!(gateway.requests.lock().unwrap().len(), round + 1);
        assert_eq!(
            gateway.requests.lock().unwrap()[round].message,
            JsString::from(format!("first-{round}"))
        );
    }
    for index in 0..2 {
        peer.get_mut()
            .write_all(&server_frame(&ha_state(&format!("split-{index}"))))
            .await
            .unwrap();
        while gateway.requests.lock().unwrap().len() < 21 + index
            || !channel.shared.runtime.lock().unwrap().active.is_empty()
        {
            tokio::task::yield_now().await;
        }
    }
    assert_eq!(gateway.requests.lock().unwrap().len(), 22);
    handle.stop("test complete").await.unwrap();
}

#[tokio::test]
async fn dropped_stop_receipt_keeps_pending_handler_and_retry_stream_ownership() {
    for fail_first in [false, true] {
        let (channel, _) = channel();
        let gate = Arc::new(Notify::new());
        let mut gateway = gateway(
            vec![value(
                r#"{"type":"elicitation_request","requestId":"pending","questions":[]}"#,
            )],
            Some(Arc::new(Notify::new())),
        );
        let config = Arc::get_mut(&mut gateway).unwrap();
        config.fail_cancel_once = fail_first;
        config.drain_gate = Some(gate.clone());
        let mut wire = attach(&channel.shared, Some(gateway.clone()));
        channel
            .shared
            .dispatch("conversation.a".into(), "pending".into());
        outgoing(&mut wire).await;
        let handle = Handle(channel.shared.clone());
        if fail_first {
            assert!(handle.stop("initial failure").await.is_err());
        }
        let first = tokio::spawn(handle.stop("drop this receipt"));
        tokio::time::timeout(Duration::from_secs(2), async {
            while gateway.drain_entered.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        first.abort();
        let _ = first.await;
        let mut second = handle.stop("wait for same worker");
        assert!(
            second
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert_eq!(gateway.stream_dropped.load(Ordering::SeqCst), 0);
        assert_eq!(gateway.drained.load(Ordering::SeqCst), 0);
        assert!(
            channel
                .shared
                .interactions
                .elicitation_pending(&"conversation.a".into())
        );
        gate.notify_one();
        tokio::time::timeout(Duration::from_secs(2), second)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(gateway.drained.load(Ordering::SeqCst), 1);
        assert_eq!(gateway.stream_dropped.load(Ordering::SeqCst), 1);
        assert!(
            !channel
                .shared
                .interactions
                .elicitation_pending(&"conversation.a".into())
        );
    }
}

#[tokio::test]
async fn aborted_shutdown_worker_publishes_persistent_failure() {
    let (channel, _) = channel();
    let gate = Arc::new(Notify::new());
    let mut gateway = gateway(Vec::new(), Some(Arc::new(Notify::new())));
    Arc::get_mut(&mut gateway).unwrap().drain_gate = Some(gate.clone());
    let _wire = attach(&channel.shared, Some(gateway.clone()));
    channel
        .shared
        .dispatch("conversation.a".into(), "pending".into());
    while gateway.started.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    let receipt = tokio::spawn(Handle(channel.shared.clone()).stop("abort worker test"));
    while gateway.drain_entered.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    channel
        .shared
        .shutdown
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        ._worker
        .abort();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), receipt)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    gate.notify_one(); // Allow the detached, still-owned message task to settle.
    while gateway.drained.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    assert!(channel.shared.stop_and_drain().await.is_err());
}

#[tokio::test]
async fn reaping_finished_panicked_message_never_turns_stop_into_success() {
    let (channel, _) = channel();
    let mut gateway = gateway(Vec::new(), None);
    Arc::get_mut(&mut gateway).unwrap().panic_next = true;
    let _wire = attach(&channel.shared, Some(gateway));
    channel
        .shared
        .dispatch("conversation.a".into(), "panic".into());
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if channel
                .shared
                .runtime
                .lock()
                .unwrap()
                .tasks
                .first()
                .is_some_and(tokio::task::JoinHandle::is_finished)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    channel
        .shared
        .dispatch("conversation.b".into(), "reap prior task".into());
    assert!(
        !channel
            .shared
            .runtime
            .lock()
            .unwrap()
            .stop_errors
            .is_empty()
    );
    assert!(channel.shared.stop_and_drain().await.is_err());
    assert!(channel.shared.stop_and_drain().await.is_err());
}

#[tokio::test]
async fn cron_receipt_can_pause_or_drop_while_stop_drains_real_ws_operation() {
    for (drop_caller, acknowledge) in [(false, false), (true, false), (false, true)] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (mut channel, _) = channel();
        Arc::get_mut(&mut channel.shared).unwrap().url =
            JsValue::String(format!("http://{}", listener.local_addr().unwrap()).into());
        let start = tokio::spawn(channel.start(ChannelStartDeps::default()));
        let (stream, _) = listener.accept().await.unwrap();
        let mut peer = accept_async(stream).await.unwrap();
        peer.send(Message::Text("{\"type\":\"auth_ok\"}".into()))
            .await
            .unwrap();
        peer.next().await.unwrap().unwrap();
        let handle = start.await.unwrap().unwrap();
        let delivery = CronResultDelivery {
            task_id: "t".into(),
            run_id: "r".into(),
            session_key: "homeassistant:chat=conversation.cron:general".into(),
            channel_key: ChannelKey::new("homeassistant"),
            origin_session_key: None,
            origin_channel_key: None,
            text: "owned cron".into(),
        };
        let mut caller = channel.deliver_cron_result(&delivery);
        assert!(
            caller
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        let frame = value(peer.next().await.unwrap().unwrap().to_text().unwrap());
        assert!(is_kind(&frame, "call_service"));
        let caller = if drop_caller {
            drop(caller);
            None
        } else {
            Some(caller)
        };
        assert_eq!(channel.shared.runtime.lock().unwrap().outbound.len(), 1);
        if acknowledge {
            peer.send(Message::Text(
                rendered(&object([
                    ("type", "result".into()),
                    ("id", frame.get("id").unwrap().clone()),
                    ("success", JsValue::Bool(true)),
                ]))
                .into(),
            ))
            .await
            .unwrap();
            tokio::time::timeout(Duration::from_secs(2), async {
                while !channel.shared.runtime.lock().unwrap().pending.is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(2), handle.stop("cron shutdown"))
            .await
            .unwrap()
            .unwrap();
        assert!(channel.shared.runtime.lock().unwrap().pending.is_empty());
        assert!(channel.shared.runtime.lock().unwrap().outbound.is_empty());
        assert!(matches!(
            peer.next().await,
            Some(Ok(Message::Close(_))) | None
        ));
        if let Some(caller) = caller {
            assert_eq!(caller.await, acknowledge);
        }
        assert!(!channel.deliver_cron_result(&delivery).await);
    }
}

#[tokio::test]
async fn never_polled_receipt_and_aborted_outbound_worker_report_failure() {
    let (channel, _) = channel();
    let mut wire = attach(&channel.shared, None);
    let receipt = channel
        .shared
        .send_reply("conversation.a".into(), "owned without caller poll".into());
    outgoing(&mut wire).await;
    assert_eq!(channel.shared.runtime.lock().unwrap().outbound.len(), 1);
    channel.shared.runtime.lock().unwrap().outbound[0].abort();
    assert!(!receipt.await);
    assert!(channel.shared.runtime.lock().unwrap().pending.is_empty());
    // A later admission must reap and retain the cancelled worker's error.
    let following = channel
        .shared
        .send_reply("conversation.a".into(), "following".into());
    let frame = outgoing(&mut wire).await;
    ack(&channel.shared, &frame, true);
    assert!(following.await);
    assert!(channel.shared.stop_and_drain().await.is_err());
    assert!(channel.shared.stop_and_drain().await.is_err());
}
