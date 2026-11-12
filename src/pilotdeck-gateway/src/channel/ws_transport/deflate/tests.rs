use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
};
use tokio_tungstenite::tungstenite::{
    Message, client::IntoClientRequest, handshake::derive_accept_key,
};

const BUDGET: Duration = Duration::from_secs(10);

async fn request_header(stream: &mut TcpStream) -> String {
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        header.push(stream.read_u8().await.expect("WS request header"));
    }
    String::from_utf8(header).unwrap()
}

fn fields(header: &str) -> std::collections::BTreeMap<String, String> {
    header
        .lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect()
}

fn response(request: &str, extension: &str) -> Vec<u8> {
    let headers = fields(request);
    let accept = derive_accept_key(headers["sec-websocket-key"].as_bytes());
    let extension = if extension.is_empty() {
        String::new()
    } else {
        format!("Sec-WebSocket-Extensions: {extension}\r\n")
    };
    format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n{extension}\r\n"
    )
    .into_bytes()
}

async fn client_frame(stream: &mut TcpStream) -> io::Result<(u8, Vec<u8>)> {
    let first = stream.read_u8().await?;
    let second = stream.read_u8().await?;
    assert_eq!(first & 0x70, 0, "native client sends uncompressed frames");
    assert_ne!(second & 128, 0, "client frame must be masked");
    let length = match second & 127 {
        126 => u64::from(stream.read_u16().await?),
        127 => stream.read_u64().await?,
        short => u64::from(short),
    };
    let mut mask = [0; 4];
    stream.read_exact(&mut mask).await?;
    let mut payload = vec![0; usize::try_from(length).unwrap()];
    stream.read_exact(&mut payload).await?;
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[index % 4];
    }
    Ok((first & 15, payload))
}

async fn wait_for_drop(stream: &mut TcpStream) {
    let mut bytes = [0; 1024];
    loop {
        match stream.read(&mut bytes).await {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::ConnectionReset => break,
            Err(error) => panic!("WS peer waiting for client drop: {error}"),
        }
    }
}

fn frame(opcode: u8, payload: &[u8], fin: bool, compressed: bool) -> Vec<u8> {
    let header = FrameHeader {
        is_final: fin,
        rsv1: compressed,
        rsv2: false,
        rsv3: false,
        opcode: opcode.into(),
        mask: None,
    };
    let mut bytes = Vec::new();
    header.format(payload.len() as u64, &mut bytes).unwrap();
    bytes.extend_from_slice(payload);
    bytes
}

#[tokio::test]
async fn native_twenty_wire_cases_preserve_messages_and_rejection() {
    let fixture: Value = serde_json::from_str(include_str!("../deflate_cases.json")).unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 20);
    replay_cases(cases).await;
}

#[tokio::test]
async fn native_receive_buffer_boundaries_match_recorded_bytes() {
    let fixture: Value =
        serde_json::from_str(include_str!("../deflate_buffer_cases.json")).unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 2);
    assert_eq!(
        cases[0]["messages"][0]["text"].as_str().unwrap().len(),
        8192
    );
    assert_eq!(
        cases[1]["messages"][0]["text"].as_str().unwrap().len(),
        20480
    );
    replay_cases(cases).await;
}

async fn replay_cases(cases: &[Value]) {
    for case in cases {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let case = case.clone();
        let name = case["name"].as_str().unwrap().to_owned();
        let outgoing = "uncompressed-client-content".repeat(200);
        let expected_outgoing = outgoing.clone();
        let extension = case["extension"].as_str().unwrap().to_owned();
        let encoded = case["frames"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|value| STANDARD.decode(value.as_str().unwrap()).unwrap())
            .collect::<Vec<_>>();
        let mut peers = JoinSet::new();
        peers.spawn(async move {
            tokio::time::timeout(BUDGET, async {
                let (mut peer, _) = listener.accept().await.unwrap();
                let request = request_header(&mut peer).await;
                assert!(request.starts_with("GET /prefix?token=a%2Fb%27 HTTP/1.1\r\n"));
                let headers = fields(&request);
                for (name, expected) in [
                    ("connection", "upgrade"),
                    ("upgrade", "websocket"),
                    ("sec-websocket-version", "13"),
                    (
                        "sec-websocket-extensions",
                        "permessage-deflate; client_max_window_bits",
                    ),
                    ("accept", "*/*"),
                    ("accept-language", "*"),
                    ("sec-fetch-mode", "websocket"),
                    ("user-agent", "node"),
                    ("pragma", "no-cache"),
                    ("cache-control", "no-cache"),
                    ("accept-encoding", "gzip, deflate"),
                    ("origin", "https://fixture.invalid"),
                ] {
                    assert_eq!(headers.get(name).map(String::as_str), Some(expected));
                }
                assert_eq!(headers.len(), 14);
                let mut wire = response(&request, &extension);
                wire.extend_from_slice(&encoded);
                wire.extend_from_slice(&frame(8, &[3, 232], true, false));
                // One write deliberately places 101 and compressed messages in
                // the same read. Handshake read-ahead must not bypass inflation.
                peer.write_all(&wire).await.unwrap();
                if extension != "unrequested-extension" {
                    let (opcode, payload) = client_frame(&mut peer).await.unwrap();
                    assert_eq!(opcode, 1);
                    assert_eq!(payload, expected_outgoing.as_bytes());
                }
                wait_for_drop(&mut peer).await;
            })
            .await
            .expect("bounded raw WS peer");
        });
        let result = tokio::time::timeout(BUDGET, async {
            let mut request = format!("ws://{address}/prefix?token=a%2Fb%27")
                .into_client_request()
                .unwrap();
            request.headers_mut().insert(
                "origin",
                HeaderValue::from_static("https://fixture.invalid"),
            );
            let stream = TcpStream::connect(address).await.unwrap();
            let connected = handshake(request, stream).await;
            let mut messages = Vec::new();
            let mut failed = false;
            match connected {
                Err(error) => {
                    assert_ne!(case["baselineCrash"], true, "{name}: invalid window must fail lazily on compressed input, not during handshake: {error:?}");
                    failed = true;
                },
                Ok((mut socket, _)) => {
                    socket.send(Message::text(outgoing)).await.unwrap();
                    while let Some(message) = socket.next().await {
                        match message {
                            Ok(Message::Text(text)) => {
                                messages.push(json!({"type":"text","text":text.as_str()}));
                            }
                            Ok(Message::Binary(bytes)) => messages
                                .push(json!({"type":"binary","base64":STANDARD.encode(bytes)})),
                            Ok(Message::Ping(_)) => socket.flush().await.unwrap(),
                            Ok(Message::Close(_)) => break,
                            Err(error) => {
                                if case["baselineCrash"] == true {
                                    assert!(matches!(&error, tokio_tungstenite::tungstenite::Error::Io(inner)
                                        if inner.kind() == io::ErrorKind::InvalidData
                                        && inner.to_string() == "Invalid server_max_window_bits"),
                                        "{name}: expected the narrow invalid-window connection error, got {error:?}");
                                }
                                failed = true;
                                break;
                            }
                            _ => {}
                        }
                    }
                }
            }
            assert_eq!(Value::Array(messages), case["messages"], "{name}");
            assert_eq!(failed, case["failure"].as_bool().unwrap(), "{name}");
        })
        .await;
        result.unwrap_or_else(|_| panic!("bounded native WS case {name}"));
        peers.join_next().await.unwrap().expect("raw WS peer task");
    }
}

#[tokio::test]
async fn ignored_binary_seeds_fragmented_text_and_ping_is_answered_before_final() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/h1-ws-wire/mattermost-deflate.json"
    ))
    .unwrap();
    let binary = STANDARD
        .decode(fixture["frames"][0]["payloadBase64"].as_str().unwrap())
        .unwrap();
    let text = STANDARD
        .decode(fixture["frames"][1]["payloadBase64"].as_str().unwrap())
        .unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut peers = JoinSet::new();
    peers.spawn(async move {
        tokio::time::timeout(BUDGET, async {
            let (mut peer, _) = listener.accept().await.unwrap();
            let request = request_header(&mut peer).await;
            let split = text.len() / 2;
            let mut wire = response(&request, "permessage-deflate");
            wire.extend_from_slice(&frame(2, &binary, true, true));
            wire.extend_from_slice(&frame(1, &text[..split], false, true));
            wire.extend_from_slice(&frame(9, b"between-fragments", true, false));
            peer.write_all(&wire).await.unwrap();
            let (opcode, payload) = client_frame(&mut peer).await.unwrap();
            assert_eq!(opcode, 10);
            assert_eq!(payload, b"between-fragments");
            let mut final_frame = frame(0, &text[split..], true, false);
            final_frame.extend_from_slice(&frame(8, &[3, 232], true, false));
            peer.write_all(&final_frame).await.unwrap();
            wait_for_drop(&mut peer).await;
        })
        .await
        .expect("bounded fragment/pong peer");
    });
    tokio::time::timeout(BUDGET, async {
        let stream = TcpStream::connect(address).await.unwrap();
        let request = format!("ws://{address}/websocket")
            .into_client_request()
            .unwrap();
        let (mut socket, _) = handshake(request, stream).await.unwrap();
        let mut binary_seen = false;
        let mut ping_seen = false;
        let mut text_seen = false;
        while let Some(message) = socket.next().await {
            match message.unwrap() {
                Message::Binary(_) => binary_seen = true,
                Message::Ping(_) => {
                    assert!(binary_seen);
                    assert!(!text_seen, "partial text must not be delivered");
                    ping_seen = true;
                    socket.flush().await.unwrap();
                }
                Message::Text(text) => {
                    assert!(binary_seen && ping_seen);
                    let event: Value = serde_json::from_str(text.as_str()).unwrap();
                    let post: Value =
                        serde_json::from_str(event["data"]["post"].as_str().unwrap()).unwrap();
                    assert_eq!(post["message"], fixture["expectedWireMessage"]);
                    text_seen = true;
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        assert!(binary_seen && ping_seen && text_seen);
    })
    .await
    .expect("bounded fragment/pong client");
    peers
        .join_next()
        .await
        .unwrap()
        .expect("fragment/pong task");
}

#[test]
fn tls_default_encoding_and_custom_origin_are_preserved() {
    let mut request = "wss://fixture.invalid/websocket"
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("origin", HeaderValue::from_static("https://custom.invalid"));
    add_default_headers(&mut request);
    assert_eq!(request.headers()["accept-encoding"], "br, gzip, deflate");
    assert_eq!(request.headers()["origin"], "https://custom.invalid");
}

#[tokio::test]
async fn receive_filter_handles_one_byte_reads_and_retains_upgrade_tail() {
    let fixture: Value = serde_json::from_str(include_str!("../deflate_cases.json")).unwrap();
    let plain = fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "plain")
        .unwrap();
    let encoded = STANDARD
        .decode(plain["frames"][0].as_str().unwrap())
        .unwrap();
    let expected = frame(
        1,
        plain["messages"][0]["text"].as_str().unwrap().as_bytes(),
        true,
        false,
    );
    let header = b"HTTP/1.1 101 Switching Protocols\r\nfixture: value\r\n\r\n";
    for capacity in [1, 8192] {
        let (client, mut peer) = tokio::io::duplex(capacity);
        let (mut receive, negotiated) = ReceiveStream::new(client);
        let mut wire = header.to_vec();
        wire.extend_from_slice(&encoded);
        let mut peers = JoinSet::new();
        peers.spawn(async move { peer.write_all(&wire).await.unwrap() });
        tokio::time::timeout(BUDGET, async {
            let mut actual_header = vec![0; header.len()];
            receive.read_exact(&mut actual_header).await.unwrap();
            assert_eq!(actual_header, header);
            negotiated
                .set(Negotiation {
                    window: Some(Ok(15)),
                })
                .unwrap();
            let mut actual_frame = vec![0; expected.len()];
            receive.read_exact(&mut actual_frame).await.unwrap();
            assert_eq!(actual_frame, expected);
        })
        .await
        .expect("bounded bytewise receive filter");
        peers.join_next().await.unwrap().expect("bytewise peer");
    }
}

#[tokio::test]
async fn compressed_frames_expose_a_ready_batch_boundary_but_plain_frames_do_not() {
    let fixture: Value = serde_json::from_str(include_str!("../deflate_cases.json")).unwrap();
    let context = fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "context")
        .unwrap();
    for compressed in [false, true] {
        let (client, mut peer) = tokio::io::duplex(8192);
        let (mut receive, negotiated) = ReceiveStream::new(client);
        let header = b"HTTP/1.1 101 Switching Protocols\r\n\r\n";
        let mut wire = header.to_vec();
        if compressed {
            for encoded in context["frames"].as_array().unwrap() {
                wire.extend_from_slice(&STANDARD.decode(encoded.as_str().unwrap()).unwrap());
            }
        } else {
            wire.extend_from_slice(&frame(2, b"binary", true, false));
            wire.extend_from_slice(&frame(1, b"text", true, false));
        }
        assert!(wire.len() <= 8192, "preload fits in the duplex capacity");
        peer.write_all(&wire).await.unwrap();
        let mut actual_header = vec![0; header.len()];
        tokio::time::timeout(BUDGET, receive.read_exact(&mut actual_header))
            .await
            .expect("bounded batch-boundary header read")
            .unwrap();
        negotiated
            .set(Negotiation {
                window: Some(Ok(15)),
            })
            .unwrap();
        let mut cx = Context::from_waker(std::task::Waker::noop());
        for _ in 0..2 {
            let mut bytes = [0; 4096];
            let mut read = ReadBuf::new(&mut bytes);
            if compressed {
                assert!(
                    Pin::new(&mut receive)
                        .poll_read(&mut cx, &mut read)
                        .is_pending()
                );
                assert!(read.filled().is_empty());
            }
            assert!(matches!(
                Pin::new(&mut receive).poll_read(&mut cx, &mut read),
                Poll::Ready(Ok(()))
            ));
            assert!(!read.filled().is_empty());
        }
    }
}
