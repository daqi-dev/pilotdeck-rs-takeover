//! Characterized against cc7c4e6 CliChannel + GatewayWsClient on 2026-11-13.
//! These use ephemeral loopback ports; the fixed-port executable scenarios
//! are separate Docker-private-network acceptance runs.
#[path = "../src/cli_channel/remote.rs"]
mod remote;

use futures_util::{SinkExt, StreamExt};
use pilotdeck_gateway::channel::cli;
use pilotdeck_protocol::{parse_json, stringify_json};
use remote::RemoteGateway;
use serde::Serialize;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{net::TcpListener, time::timeout};
use tokio_tungstenite::{
    WebSocketStream, accept_async,
    tungstenite::{Message, protocol::CloseFrame, protocol::frame::coding::CloseCode},
};

type Peer = WebSocketStream<tokio::net::TcpStream>;

async fn frame(peer: &mut Peer) -> Value {
    let raw = timeout(Duration::from_secs(3), peer.next())
        .await
        .expect("client sent its frame")
        .expect("client remains connected")
        .expect("valid websocket message")
        .into_text()
        .expect("JSON text");
    serde_json::from_str(&raw).expect("valid JSON")
}

async fn send(peer: &mut Peer, value: Value) {
    peer.send(Message::Text(value.to_string().into()))
        .await
        .expect("mock sends frame");
}

async fn accept(listener: &TcpListener) -> Peer {
    let (stream, _) = listener.accept().await.expect("client connects");
    let mut peer = accept_async(stream).await.expect("websocket upgrade");
    assert_eq!(
        frame(&mut peer).await,
        json!({
            "type":"hello", "protocolVersion":"1.2", "clientName":"cli",
            "clientVersion":"0.1.0", "token":"mock-token"
        })
    );
    send(&mut peer, json!({"type":"hello_ok"})).await;
    peer
}

#[tokio::test]
async fn cli_stream_ignores_responses_and_sequence_and_discards_final_payload() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/ws", listener.local_addr().unwrap());
    let peer = tokio::spawn(async move {
        let mut peer = accept(&listener).await;
        for message in ["first", "second"] {
            let request = frame(&mut peer).await;
            assert_eq!(request["method"], "submit_turn");
            assert_eq!(
                request["params"],
                json!({
                    "sessionKey":"cli:project=/project:default", "channelKey":"cli",
                    "message":message,"canPrompt":false
                })
            );
            let id = &request["id"];
            // Both RPC responses and unrelated event IDs are ignored by CLI
            // streams, even though SDK streams interpret responses and seq.
            send(
                &mut peer,
                json!({"type":"response","id":id,"ok":false,"error":{"message":"ignored"}}),
            )
            .await;
            send(&mut peer, json!({"type":"event","id":"unrelated","event":{"type":"assistant_text_delta","text":"wrong"}})).await;
            for (kind, text) in [
                ("assistant_text_delta", message),
                ("assistant_thinking_delta", " thought"),
            ] {
                send(
                    &mut peer,
                    json!({"type":"event","id":id,"seq":7,"event":{"type":kind,"text":text}}),
                )
                .await;
            }
            send(&mut peer, json!({"type":"event","id":id,"final":true,"event":{"type":"assistant_text_delta","text":"must-not-render"}})).await;
        }
        // Keep the server alive until the CLI explicitly releases its socket.
        assert!(matches!(peer.next().await, Some(Ok(Message::Close(_)))));
    });
    let mut client = RemoteGateway::connect(&url, "mock-token").await.unwrap();
    for message in ["first", "second"] {
        let input = parse_json(&format!(
            r#"{{"sessionKey":"cli:project=/project:default","channelKey":"cli","message":"{message}","canPrompt":false}}"#
        )).unwrap();
        let mut output = String::new();
        timeout(
            Duration::from_secs(3),
            client.submit_turn(input, |event| {
                if let Some(text) = cli::render_event(&event) {
                    output.push_str(&text);
                }
            }),
        )
        .await
        .expect("stream ends at final frame")
        .unwrap();
        assert_eq!(output, format!("{message}[thinking]  thought"));
    }
    client.close().await;
    timeout(Duration::from_secs(3), peer)
        .await
        .unwrap()
        .unwrap();
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ListParams<'a> {
    project_key: &'a str,
    session_key: &'a str,
}

#[tokio::test]
async fn cli_rpc_uses_cli_identity_and_preserves_results_and_empty_errors() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/ws", listener.local_addr().unwrap());
    let peer = tokio::spawn(async move {
        let mut peer = accept(&listener).await;
        for fail in [false, true] {
            let request = frame(&mut peer).await;
            assert_eq!(request["method"], "cron_list");
            assert_eq!(
                request["params"],
                json!({"projectKey":"/project","sessionKey":"session"})
            );
            send(&mut peer, json!({"type":"notification","name":"ignored"})).await;
            send(
                &mut peer,
                json!({"type":"response","id":"other","ok":true,"result":"ignored"}),
            )
            .await;
            // Raw text pins object order and a lone surrogate, which a serde
            // Value round trip would replace or reject.
            let raw = if fail {
                format!(
                    r#"{{"type":"response","id":{},"ok":false,"error":{{"message":""}}}}"#,
                    request["id"]
                )
            } else {
                format!(
                    r#"{{"type":"response","id":{},"ok":true,"result":{{"z":"\ud800","a":1}}}}"#,
                    request["id"]
                )
            };
            peer.send(Message::Text(raw.into())).await.unwrap();
        }
        assert!(matches!(peer.next().await, Some(Ok(Message::Close(_)))));
    });
    let mut client = RemoteGateway::connect(&url, "mock-token").await.unwrap();
    let params = || ListParams {
        project_key: "/project",
        session_key: "session",
    };
    let result = timeout(
        Duration::from_secs(3),
        client.request("cron_list", params()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        stringify_json(&result).unwrap().unwrap(),
        r#"{"z":"\ud800","a":1}"#
    );
    let error = timeout(
        Duration::from_secs(3),
        client.request("cron_list", params()),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(error, "");
    client.close().await;
    timeout(Duration::from_secs(3), peer)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn remote_disconnect_preserves_rendered_text_and_uses_cli_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/ws", listener.local_addr().unwrap());
    let peer = tokio::spawn(async move {
        let mut peer = accept(&listener).await;
        let request = frame(&mut peer).await;
        send(&mut peer, json!({"type":"event","id":request["id"],"event":{"type":"assistant_text_delta","text":"ok"}})).await;
        peer.close(Some(CloseFrame {
            code: CloseCode::Error,
            reason: "specific reason".into(),
        }))
        .await
        .unwrap();
    });
    let mut client = RemoteGateway::connect(&url, "mock-token").await.unwrap();
    let mut output = String::new();
    let error = timeout(
        Duration::from_secs(3),
        client.submit_turn(parse_json("{}").unwrap(), |event| {
            if let Some(text) = cli::render_event(&event) {
                output.push_str(&text);
            }
        }),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(output, "ok");
    assert_eq!(error, "Gateway WebSocket closed.");
    client.close().await;
    timeout(Duration::from_secs(3), peer)
        .await
        .unwrap()
        .unwrap();
}

#[test]
fn idle_interactive_connection_answers_ping_before_the_next_prompt() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let listener = runtime.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
    let url = format!("ws://{}/ws", listener.local_addr().unwrap());
    let (pong_seen, wait_pong) = std::sync::mpsc::channel();
    let (start_ping, ping_start) = tokio::sync::oneshot::channel();
    let peer = runtime.spawn(async move {
        let mut peer = accept(&listener).await;
        let first = frame(&mut peer).await;
        send(
            &mut peer,
            json!({"type":"event","id":first["id"],"final":true}),
        )
        .await;
        ping_start.await.unwrap();
        peer.send(Message::Ping(b"idle".to_vec().into()))
            .await
            .unwrap();
        let pong = timeout(Duration::from_secs(3), peer.next()).await.unwrap();
        assert!(matches!(pong, Some(Ok(Message::Pong(payload))) if payload == b"idle"[..]));
        pong_seen.send(()).unwrap();
        let second = frame(&mut peer).await;
        assert_eq!(second["method"], "submit_turn");
        send(
            &mut peer,
            json!({"type":"event","id":second["id"],"final":true}),
        )
        .await;
        assert!(matches!(peer.next().await, Some(Ok(Message::Close(_)))));
    });
    let mut client = runtime
        .block_on(RemoteGateway::connect(&url, "mock-token"))
        .unwrap();
    runtime
        .block_on(client.submit_turn(parse_json("{}").unwrap(), |_| {}))
        .unwrap();
    start_ping.send(()).unwrap();
    // Model the foreground's blocking stdin read: no runtime.block_on runs
    // here, yet the remote reader must process and flush the automatic pong.
    wait_pong
        .recv_timeout(Duration::from_secs(3))
        .expect("idle reader answers ping");
    runtime
        .block_on(client.submit_turn(parse_json("{}").unwrap(), |_| {}))
        .unwrap();
    runtime.block_on(client.close());
    runtime.block_on(async {
        timeout(Duration::from_secs(3), peer)
            .await
            .unwrap()
            .unwrap()
    });
}

#[tokio::test]
async fn hello_rejection_keeps_reason_or_close_code() {
    for (reason, expected) in [
        ("denied", "Gateway closed during hello: denied"),
        ("", "Gateway closed during hello: code 1008"),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/ws", listener.local_addr().unwrap());
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut peer = accept_async(stream).await.unwrap();
            assert_eq!(frame(&mut peer).await["type"], "hello");
            peer.close(Some(CloseFrame {
                code: CloseCode::Policy,
                reason: reason.into(),
            }))
            .await
            .unwrap();
        });
        let result = timeout(
            Duration::from_secs(3),
            RemoteGateway::connect(&url, "mock-token"),
        )
        .await
        .unwrap();
        assert_eq!(result.err().expect("hello rejects"), expected);
        timeout(Duration::from_secs(3), peer)
            .await
            .unwrap()
            .unwrap();
    }
}
