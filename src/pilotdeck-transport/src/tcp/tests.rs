use super::*;
use crate::{
    server::{ExecuteRequest, ExecutionContext, ExecutionTerminal},
    system_clock::SystemExecutionClock,
};
use pilotdeck_agent::ports::PortResult;
use pilotdeck_protocol::{JsObject, admission::CallEffect, module::ModuleOutcome};
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicUsize, Ordering};

struct CallbackFactory {
    calls: Arc<AtomicUsize>,
    events: usize,
}
impl ExecutionFactory for CallbackFactory {
    fn execute(
        &self,
        _: &ExecuteRequest,
        context: &mut ExecutionContext,
    ) -> PortResult<ExecutionTerminal> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        for sequence in 0..self.events {
            context.emit(
                "agent_event".into(),
                object([
                    ("type", "model_event".into()),
                    ("text", JsValue::String(format!("event-{sequence}").into())),
                ]),
            )?;
        }
        context.call(
            context.identity("session".into(), "turn".into()),
            "capability",
            "execute",
            CallEffect::SideEffect,
            object([("tool", "effect".into())]),
        )?;
        context.control.check()?;
        Ok(ExecutionTerminal {
            outcome: ModuleOutcome::Completed,
            payload: object([("done", JsValue::Bool(true))]),
            error: None,
        })
    }
}
struct Peer {
    writer: TcpStream,
    reader: BufReader<TcpStream>,
}
impl Peer {
    fn connect(server: &TcpExecutionServer) -> Self {
        let writer = TcpStream::connect(server.local_addr()).unwrap();
        writer
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        Self {
            reader: BufReader::new(writer.try_clone().unwrap()),
            writer,
        }
    }
    fn send(&mut self, value: &JsValue) {
        self.writer.write_all(&line(value).unwrap()).unwrap();
    }
    fn read(&mut self) -> JsValue {
        let mut line = String::new();
        assert!(self.reader.read_line(&mut line).unwrap() > 0, "peer EOF");
        parse_json(&line).unwrap()
    }
    fn request(&mut self, method: &str, fields: &[(&str, JsValue)]) -> JsValue {
        let mut request = object([
            ("kind", "request".into()),
            ("messageId", method.into()),
            ("method", method.into()),
            ("payload", JsValue::Object(JsObject::new())),
        ]);
        for (key, value) in fields {
            request.insert((*key).into(), value.clone());
        }
        self.send(&JsValue::Object(request));
        self.read()
    }
    fn execute(&mut self, suffix: &str) {
        self.send(&JsValue::Object(object([
            ("kind", "request".into()),
            ("method", "execute".into()),
            (
                "messageId",
                JsValue::String(format!("execute-{suffix}").into()),
            ),
            ("runId", JsValue::String(format!("run-{suffix}").into())),
            (
                "operationId",
                JsValue::String(format!("operation-{suffix}").into()),
            ),
            (
                "requestId",
                JsValue::String(format!("request-{suffix}").into()),
            ),
            ("payload", JsValue::Object(JsObject::new())),
        ])));
    }
    fn reply(&mut self, callback: &JsValue) {
        self.send(&JsValue::Object(object([
            ("kind", "response".into()),
            ("messageId", "host-response".into()),
            ("inReplyTo", callback.get("messageId").unwrap().clone()),
            ("requestId", callback.get("requestId").unwrap().clone()),
            ("ok", JsValue::Bool(true)),
            ("payload", JsValue::Object(JsObject::new())),
        ])));
    }
}
fn object<const N: usize>(fields: [(&str, JsValue); N]) -> JsObject {
    let mut object = JsObject::new();
    for (key, value) in fields {
        object.insert(key.into(), value);
    }
    object
}
fn fixture(mut options: TcpServerOptions, events: usize) -> (TcpExecutionServer, Arc<AtomicUsize>) {
    options.host = "127.0.0.1".into();
    options.port = 0;
    let calls = Arc::new(AtomicUsize::new(0));
    let server = TcpExecutionServer::listen(
        options,
        Arc::new(CallbackFactory {
            calls: calls.clone(),
            events,
        }),
        Arc::new(SystemExecutionClock),
    )
    .unwrap();
    (server, calls)
}
fn resume_fields(hello: &JsValue, stream: &JsValue, cursor: f64) -> Vec<(&'static str, JsValue)> {
    vec![
        ("streamId", stream.clone()),
        ("lastAppliedSequence", JsValue::Number(cursor)),
        (
            "previousBinding",
            JsValue::Object(object([
                (
                    "moduleInstanceId",
                    hello.get("moduleInstanceId").unwrap().clone(),
                ),
                (
                    "connectionGeneration",
                    hello.get("connectionGeneration").unwrap().clone(),
                ),
            ])),
        ),
    ]
}

#[test]
fn tcp_default_capabilities_and_connection_binding_match_formal_sidecar() {
    let (server, _) = fixture(TcpServerOptions::default(), 0);
    let mut first = Peer::connect(&server);
    let mut second = Peer::connect(&server);
    let one = first.request("hello", &[]);
    let two = second.request("hello", &[]);
    assert_eq!(one.get("moduleInstanceId"), two.get("moduleInstanceId"));
    assert_ne!(
        one.get("connectionGeneration"),
        two.get("connectionGeneration")
    );
    let caps = first.request("capabilities", &[]);
    let JsValue::Array(methods) = caps.get("payload").unwrap().get("methods").unwrap() else {
        panic!("methods array")
    };
    for name in ["resume", "ack"] {
        assert_eq!(
            methods
                .iter()
                .find(|method| method.get("name") == Some(&name.into()))
                .unwrap()
                .get("enabled"),
            Some(&JsValue::Bool(false))
        );
    }
    assert_eq!(
        second
            .request("resume", &resume_fields(&one, &"missing".into(), -1.0))
            .get("code"),
        Some(&"RESUME_UNSUPPORTED".into())
    );
    assert_eq!(
        second
            .request(
                "ack",
                &[
                    ("streamId", "missing".into()),
                    ("lastAppliedSequence", JsValue::Number(0.0))
                ]
            )
            .get("code"),
        Some(&"ACK_UNSUPPORTED".into())
    );
    assert!(server.close(Duration::from_secs(3)).complete());
    let mut bytes = [0];
    assert_eq!(
        std::io::Read::read(&mut first.writer, &mut bytes).unwrap(),
        0
    );
}

#[test]
fn tcp_foreground_wait_is_released_by_explicit_close_without_error() {
    let (server, _) = fixture(TcpServerOptions::default(), 0);
    let server = Arc::new(server);
    let waiter = server.clone();
    let waiting = thread::spawn(move || waiter.wait());
    let mut peer = Peer::connect(&server);
    assert_eq!(
        peer.request("hello", &[]).get("ok"),
        Some(&JsValue::Bool(true))
    );
    assert!(server.close(Duration::from_secs(3)).complete());
    assert!(waiting.join().unwrap().unwrap().complete());
}

#[test]
fn tcp_active_repeat_is_rejected_and_completed_repeat_never_reexecutes_effect() {
    let (server, calls) = fixture(TcpServerOptions::default(), 0);
    let mut first = Peer::connect(&server);
    let mut second = Peer::connect(&server);
    first.execute("repeat");
    assert_eq!(first.read().get("ok"), Some(&JsValue::Bool(true)));
    let callback = first.read();
    assert_eq!(callback.get("kind"), Some(&"request".into()));
    for peer in [&mut first, &mut second] {
        peer.execute("repeat");
        let rejected = peer.read();
        assert_eq!(rejected.get("ok"), Some(&JsValue::Bool(false)));
        assert_eq!(
            rejected.get("code"),
            Some(&"OPERATION_ALREADY_ACTIVE".into())
        );
        assert!(rejected.get("streamId").is_none());
    }
    let mismatch = second.request(
        "execute",
        &[
            ("runId", "other-run".into()),
            ("operationId", "operation-repeat".into()),
            ("requestId", "request-repeat".into()),
        ],
    );
    assert_eq!(
        mismatch.get("code"),
        Some(&"OPERATION_IDENTITY_MISMATCH".into())
    );
    first.reply(&callback);
    let terminal = first.read();
    assert_eq!(terminal.get("final"), Some(&JsValue::Bool(true)));
    assert_eq!(terminal.get("outcome"), Some(&"completed".into()));
    second.execute("repeat");
    assert_eq!(second.read(), terminal);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(server.close(Duration::from_secs(3)).complete());
}

#[test]
fn tcp_nonresumable_disconnect_retires_workers_and_never_reaccepts_dead_execution() {
    let (server, calls) = fixture(TcpServerOptions::default(), 0);
    let mut first = Peer::connect(&server);
    first.execute("one");
    assert_eq!(first.read().get("ok"), Some(&JsValue::Bool(true)));
    assert_eq!(first.read().get("method"), Some(&"module_call".into()));
    first.writer.shutdown(std::net::Shutdown::Both).unwrap();
    drop(first);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if lock(&server.hub.state)
            .executions
            .values()
            .all(|execution| execution.retired)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "non-resumable EOF stranded an execution worker"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        server.hub.incomplete_engines.load(Ordering::Acquire),
        0,
        "retired is only successful if all actual serve workers joined"
    );
    let mut second = Peer::connect(&server);
    let status = second.request("status", &[("requestId", "request-one".into())]);
    assert_eq!(
        status.get("payload").unwrap().get("outcome"),
        Some(&"result_unknown".into())
    );
    second.execute("one");
    let rejected = second.read();
    assert_eq!(rejected.get("code"), Some(&"RESULT_UNKNOWN".into()));
    assert_eq!(rejected.get("final"), Some(&JsValue::Bool(true)));
    assert_eq!(rejected.get("outcome"), Some(&"result_unknown".into()));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "reconciliation never starts a new execution"
    );
    assert!(server.close(Duration::from_secs(3)).complete());
}

#[test]
fn tcp_resume_replays_exact_callback_once_and_revokes_old_socket_authority() {
    let (server, calls) = fixture(
        TcpServerOptions {
            stream_resume: true,
            ..Default::default()
        },
        1,
    );
    let mut first = Peer::connect(&server);
    let one = first.request("hello", &[]);
    first.execute("one");
    let accepted = first.read();
    let event = first.read();
    let callback = first.read();
    assert_eq!(event.get("sequence"), Some(&JsValue::Number(0.0)));
    assert_eq!(callback.get("method"), Some(&"module_call".into()));
    let stream = accepted.get("streamId").unwrap().clone();
    assert_eq!(
        first
            .request(
                "ack",
                &[
                    ("streamId", stream.clone()),
                    ("lastAppliedSequence", JsValue::Number(0.0))
                ]
            )
            .get("ok"),
        Some(&JsValue::Bool(true))
    );
    let mut second = Peer::connect(&server);
    let two = second.request("hello", &[]);
    assert_eq!(
        second
            .request("resume", &resume_fields(&one, &stream, 0.0))
            .get("ok"),
        Some(&JsValue::Bool(true))
    );
    assert_eq!(
        second.read(),
        callback,
        "same request identity and bytes, no repeated execute"
    );
    first.reply(&callback);
    // An ordered control on the old socket proves its earlier forged reply was
    // consumed. The still-running status rules out accepting it as the callback.
    let status = first.request("status", &[("requestId", "request-one".into())]);
    assert_eq!(
        status.get("payload").unwrap().get("state"),
        Some(&"running".into())
    );
    second.reply(&callback);
    let terminal = second.read();
    assert_eq!(terminal.get("outcome"), Some(&"completed".into()));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        second
            .request("resume", &resume_fields(&one, &stream, 0.0))
            .get("code"),
        Some(&"BINDING_MISMATCH".into())
    );
    assert_eq!(
        second
            .request("resume", &resume_fields(&two, &stream, 0.0))
            .get("ok"),
        Some(&JsValue::Bool(true))
    );
    assert_eq!(
        second.read(),
        terminal,
        "terminal replay retains original sequence"
    );
    assert!(server.close(Duration::from_secs(3)).complete());
}

#[test]
fn tcp_concurrent_executions_keep_callback_and_control_namespaces_separate() {
    let (server, calls) = fixture(TcpServerOptions::default(), 0);
    let mut first = Peer::connect(&server);
    first.execute("one");
    let accepted_one = first.read();
    let callback_one = first.read();
    first.execute("two");
    let accepted_two = first.read();
    let callback_two = first.read();
    assert_ne!(accepted_one.get("streamId"), accepted_two.get("streamId"));
    assert_ne!(callback_one.get("messageId"), callback_two.get("messageId"));
    assert_ne!(callback_one.get("requestId"), callback_two.get("requestId"));
    let mut other = Peer::connect(&server);
    other.reply(&callback_one); // no stream binding on this socket
    let status = other.request("status", &[("requestId", "request-one".into())]);
    assert_eq!(status.get("inReplyTo"), Some(&"status".into()));
    assert_eq!(
        status.get("payload").unwrap().get("state"),
        Some(&"running".into())
    );
    assert_eq!(
        other
            .request(
                "cancel",
                &[
                    ("runId", "wrong".into()),
                    ("operationId", "operation-one".into()),
                    ("reason", "stop".into())
                ]
            )
            .get("code"),
        Some(&"OPERATION_IDENTITY_MISMATCH".into())
    );
    first.reply(&callback_two);
    let two = first.read();
    assert_eq!(two.get("operationId"), Some(&"operation-two".into()));
    first.reply(&callback_one);
    let one = first.read();
    assert_eq!(one.get("operationId"), Some(&"operation-one".into()));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(server.close(Duration::from_secs(3)).complete());
}

#[test]
fn tcp_replay_bounds_and_disconnected_shutdown_preserve_execution_ownership() {
    let (server, calls) = fixture(
        TcpServerOptions {
            stream_resume: true,
            replay: ReplayLimits {
                max_events_per_stream: 2,
                ..Default::default()
            },
            ..Default::default()
        },
        4,
    );
    let mut first = Peer::connect(&server);
    let one = first.request("hello", &[]);
    first.execute("one");
    let accepted = first.read();
    for sequence in 0..4 {
        assert_eq!(
            first.read().get("sequence"),
            Some(&JsValue::Number(sequence as f64))
        );
    }
    let callback = first.read();
    let stream = accepted.get("streamId").unwrap().clone();
    first.writer.shutdown(std::net::Shutdown::Both).unwrap();
    drop(first);
    let mut second = Peer::connect(&server);
    assert_eq!(
        second
            .request("resume", &resume_fields(&one, &stream, 0.0))
            .get("code"),
        Some(&"CURSOR_EXPIRED".into())
    );
    assert_eq!(
        second
            .request("resume", &resume_fields(&one, &stream, 2.0))
            .get("ok"),
        Some(&JsValue::Bool(true))
    );
    assert_eq!(second.read().get("sequence"), Some(&JsValue::Number(3.0)));
    assert_eq!(second.read(), callback);
    // Leaving an accepted host call unresolved must still release all service
    // workers on explicit shutdown; no extra factory invocation is permissible.
    second.writer.shutdown(std::net::Shutdown::Both).unwrap();
    drop(second);
    assert!(server.close(Duration::from_secs(3)).complete());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
