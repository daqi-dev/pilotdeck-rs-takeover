//! Real loopback TCP sockets with one retained mock stream. Reconnect replaces
//! only the socket: the peer asserts resume (never execute), and duplicate
//! callback delivery must reuse its first response instead of repeating work.
use super::*;
use serde_json::{Value, json};
use std::sync::Mutex;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};

type Peer = BufReader<TcpStream>;
async fn read(peer: &mut Peer) -> Value {
    let mut line = String::new();
    assert_ne!(
        peer.read_line(&mut line).await.unwrap(),
        0,
        "unexpected EOF"
    );
    serde_json::from_str(&line).unwrap()
}
async fn send(peer: &mut Peer, frame: Value) {
    peer.get_mut()
        .write_all(format!("{frame}\n").as_bytes())
        .await
        .unwrap();
}
fn reply(request: &Value, fields: Value) -> Value {
    let mut result = json!({"kind":"response","messageId":format!("reply-{}", request["messageId"].as_str().unwrap()),"inReplyTo":request["messageId"],"ok":true});
    result
        .as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    result
}
async fn handshake(peer: &mut Peer, instance: &str, generation: &str, stop_after_hello: bool) {
    let mut binding = json!({"protocolVersion":"2.0","moduleId":"tcp-loop","moduleInstanceId":instance,"connectionGeneration":generation,"capabilitiesVersion":"v1"});
    let hello = read(peer).await;
    assert_eq!(hello["method"], "hello");
    send(peer, reply(&hello, binding.clone())).await;
    if stop_after_hello {
        return;
    }
    let capabilities = read(peer).await;
    assert_eq!(capabilities["method"], "capabilities");
    binding["payload"] = json!({"capabilitiesVersion":"v1","methods":[{"name":"execute","profiles":["streaming"],"resumeSupport":"streaming"},{"name":"resume"},{"name":"ack"},{"name":"cancel"}]});
    send(peer, reply(&capabilities, binding)).await;
}
fn event(sequence: u32, final_event: bool) -> Value {
    let mut event = json!({"kind":"event","eventType":"agent_event","streamId":"retained-stream","sequence":sequence,"final":final_event,"runId":"run","operationId":"operation","requestId":"request"});
    if final_event {
        event["outcome"] = json!("completed");
        event["payload"] = json!({"messages":[],"result":{"type":"success","sessionId":"session","turnId":"turn","stopReason":"stop","turns":1}});
    } else {
        event["payload"] =
            json!({"type":"text_delta","sessionId":"session","turnId":"turn","text":"first"});
    }
    event
}
struct CountingHandler {
    effects: AtomicUsize,
    observations: Mutex<Vec<ReconnectObservation>>,
}
impl HostHandler for CountingHandler {
    fn module_call(&self, _: JsValue) -> HostFuture<JsObject> {
        let effect = self.effects.fetch_add(1, Ordering::SeqCst) + 1;
        Box::pin(async move {
            let mut result = JsObject::new();
            result.insert("effect".into(), JsValue::Number(effect as f64));
            Ok(result)
        })
    }
    fn observe_transport(&self, observation: ReconnectObservation) {
        self.observations.lock().unwrap().push(observation);
    }
}
async fn retained_peer(listener: TcpListener, change_instance: bool) {
    let (socket, _) = listener.accept().await.unwrap();
    let mut peer = BufReader::new(socket);
    handshake(&mut peer, "one-instance", "generation-1", false).await;
    let execute = read(&mut peer).await;
    assert_eq!(execute["method"], "execute");
    send(
        &mut peer,
        reply(
            &execute,
            json!({"requestId":"request","streamId":"retained-stream"}),
        ),
    )
    .await;
    send(&mut peer, event(0, false)).await;
    let ack = read(&mut peer).await;
    assert_eq!(ack["method"], "ack");
    assert_eq!(ack["lastAppliedSequence"], 0);
    let callback = json!({"kind":"request","messageId":"effect-1","method":"module_call","module":"capability","runId":"run","operationId":"operation","requestId":"callback-request","payload":{"operation":"execute"}});
    send(&mut peer, callback.clone()).await;
    let original_response = read(&mut peer).await;
    assert_eq!(original_response["inReplyTo"], "effect-1");
    assert_eq!(original_response["payload"]["effect"], 1);
    drop(peer);
    let (socket, _) = listener.accept().await.unwrap();
    let mut peer = BufReader::new(socket);
    handshake(
        &mut peer,
        if change_instance {
            "different-instance"
        } else {
            "one-instance"
        },
        "generation-2",
        change_instance,
    )
    .await;
    if change_instance {
        let mut line = String::new();
        assert_eq!(
            peer.read_line(&mut line).await.unwrap(),
            0,
            "must close restarted instance without execute/resume"
        );
        return;
    }
    let resume = read(&mut peer).await;
    assert_eq!(
        resume["method"], "resume",
        "a second execute would repeat side effects"
    );
    assert_eq!(resume["streamId"], "retained-stream");
    assert_eq!(resume["lastAppliedSequence"], 0);
    assert_eq!(
        resume["previousBinding"],
        json!({"moduleInstanceId":"one-instance","connectionGeneration":"generation-1"})
    );
    send(
        &mut peer,
        reply(&resume, json!({"streamId":"retained-stream"})),
    )
    .await;
    send(&mut peer, callback).await;
    let cached = read(&mut peer).await;
    assert_eq!(
        cached, original_response,
        "replayed callback must use the immutable cached result"
    );
    send(&mut peer, event(1, true)).await;
    let mut trailing = String::new();
    assert_eq!(peer.read_line(&mut trailing).await.unwrap(), 0);
}

#[tokio::test]
async fn tcp_reconnects_same_instance_without_repeating_host_effect() {
    for changed in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = tokio::spawn(retained_peer(listener, changed));
        let handler = Arc::new(CountingHandler {
            effects: AtomicUsize::new(0),
            observations: Mutex::new(vec![]),
        });
        let limits = Limits {
            handshake_timeout: Some(Duration::from_secs(2)),
            ..Limits::default()
        };
        let client = HostClient::connect_tcp(
            "127.0.0.1",
            port,
            Some(Duration::from_secs(2)),
            Some(&"tcp-loop".into()),
            limits,
        )
        .await
        .unwrap();
        assert_eq!(client.child_id(), None);
        let result = timeout(
            Duration::from_secs(8),
            client.execute(request(), handler.clone()),
        )
        .await
        .unwrap();
        if changed {
            let error = result.unwrap_err();
            assert!(
                error.message.contains("instance restarted"),
                "{}",
                error.message
            );
            assert!(error.accepted);
        } else {
            assert!(matches!(result.unwrap(), ExecuteResult::Terminal(_)));
        }
        assert_eq!(handler.effects.load(Ordering::SeqCst), 1);
        let observations = handler.observations.lock().unwrap().clone();
        assert_eq!(
            observations.first(),
            Some(&ReconnectObservation::Started {
                attempt: 1,
                last_applied_sequence: 0.0
            })
        );
        assert_eq!(
            observations.last(),
            Some(&if changed {
                ReconnectObservation::Failed { attempt: 1 }
            } else {
                ReconnectObservation::Succeeded { attempt: 1 }
            })
        );
        timeout(Duration::from_secs(2), peer)
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn tcp_connection_validation_refusal_and_handshake_deadline() {
    assert!(
        HostClient::connect_tcp(
            "127.0.0.1",
            1,
            Some(Duration::ZERO),
            None,
            Limits::default()
        )
        .await
        .is_err()
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let error = HostClient::connect_tcp(
        "127.0.0.1",
        port,
        Some(Duration::from_secs(1)),
        None,
        Limits::default(),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(error.kind, HostErrorKind::Io);
    assert_eq!(error.message, "Failed to connect to sidecar TCP server.");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let peer = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut peer = BufReader::new(socket);
        assert_eq!(read(&mut peer).await["method"], "hello");
        let mut line = String::new();
        assert_eq!(peer.read_line(&mut line).await.unwrap(), 0);
    });
    let limits = Limits {
        handshake_timeout: Some(Duration::from_millis(50)),
        ..Limits::default()
    };
    let error = HostClient::connect_tcp("127.0.0.1", port, None, None, limits)
        .await
        .err()
        .unwrap();
    assert_eq!(error.kind, HostErrorKind::Timeout);
    timeout(Duration::from_secs(2), peer)
        .await
        .unwrap()
        .unwrap();
}
