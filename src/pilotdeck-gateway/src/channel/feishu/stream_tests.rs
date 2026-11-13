use super::super::protocol::TurnEventStream;
use super::wire::Frame;
use super::*;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::{Response, service::service_fn};
use serde_json::{Value, json};
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot},
    time::timeout,
};
use tokio_tungstenite::{accept_async, tungstenite::Message};

struct Empty(bool);
impl TurnEventStream for Empty {
    fn next(
        &mut self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<JsValue>, ChannelError>> + Send + '_>,
    > {
        let first = std::mem::replace(&mut self.0, false);
        Box::pin(async move {
            if first {
                Ok(Some(
                    pilotdeck_protocol::parse_json(
                        r#"{"type":"assistant_text_delta","text":"ACTUAL-STREAM-REPLY"}"#,
                    )
                    .unwrap(),
                ))
            } else {
                Ok(None)
            }
        })
    }
}
struct Gateway(mpsc::UnboundedSender<TurnRequest>);
impl TurnGateway for Gateway {
    fn submit(
        &self,
        input: TurnRequest,
    ) -> BoxFuture<Result<Box<dyn TurnEventStream>, ChannelError>> {
        self.0.send(input).unwrap();
        Box::pin(async { Ok(Box::new(Empty(true)) as Box<dyn TurnEventStream>) })
    }
}
fn event(kind: &str, id: &str, text: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({"schema":"2.0","header":{"event_type":kind,"event_id":"header-does-not-dedupe"},"event":{"message":{"message_id":id,"chat_id":"local-chat","message_type":"text","content":json!({"text":text}).to_string()}}})).unwrap()
}
fn frame(payload: Vec<u8>, key: &str, sum: usize, seq: usize) -> Frame {
    Frame {
        seq: 9,
        log: 17,
        service: 42,
        method: 1,
        headers: vec![
            ("type".into(), "event".into()),
            ("message_id".into(), key.into()),
            ("sum".into(), sum.to_string()),
            ("seq".into(), seq.to_string()),
        ],
        payload,
        ..Default::default()
    }
}

#[test]
fn frame_schema_matches_fixed_sdk_binary_and_rejects_corruption() {
    // Real fixed SDK ping oracle-third.json, encoded independently by SDK protobuf.
    let bytes = [
        8, 0, 16, 0, 24, 3, 32, 0, 42, 12, 10, 4, 116, 121, 112, 101, 18, 4, 112, 105, 110, 103,
    ];
    let parsed = Frame::decode(&bytes).unwrap();
    assert_eq!(parsed.service, 3);
    assert_eq!(parsed.header("type"), Some("ping"));
    assert_eq!(parsed.encode(), bytes);
    assert!(Frame::decode(&[66, 128]).is_err());
    assert!(Frame::decode(&[8, 255, 255, 255, 255, 255, 255, 255, 255, 255, 2]).is_err());
    let wrong = Frame {
        service: 4,
        ..parsed.clone()
    };
    assert_ne!(
        wrong.encode(),
        bytes,
        "deliberately wrong service cannot satisfy fixed wire oracle"
    );
}

#[tokio::test]
async fn real_endpoint_fragmented_stream_dispatch_dedupe_ack_ping_and_owned_stop() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_addr = listener.local_addr().unwrap();
    let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", http.local_addr().unwrap());
    let (requests, mut received) = mpsc::unbounded_channel();
    let http_task = tokio::spawn(async move {
        loop {
            let (tcp, _) = http.accept().await.unwrap();
            let requests = requests.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                    let requests = requests.clone();
                    async move {
                        let path = request.uri().path().to_owned();
                        let locale = request
                            .headers()
                            .get("locale")
                            .and_then(|h| h.to_str().ok())
                            .map(str::to_owned);
                        let body = request.into_body().collect().await.unwrap().to_bytes();
                        let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                        requests.send((path.clone(), locale, value)).unwrap();
                        let reply = if path == "/callback/ws/endpoint" {
                            json!({"code":0,"data":{"URL":format!("ws://{ws_addr}/lark?device_id=local&service_id=42"),"ClientConfig":{"PingInterval":60,"ReconnectCount":0,"ReconnectInterval":1,"ReconnectNonce":0}}})
                        } else if path.contains("tenant_access_token") {
                            json!({"code":0,"tenant_access_token":"local-token","expire":7200})
                        } else {
                            json!({"code":0,"data":{"reaction_id":"local-reaction","message_id":"local-reply"}})
                        };
                        Ok::<_, std::convert::Infallible>(Response::new(Full::new(Bytes::from(
                            reply.to_string(),
                        ))))
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(tcp), service)
                    .await;
            });
        }
    });
    let (acked, ack_receipt) = oneshot::channel();
    let (closed, close_receipt) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(tcp).await.unwrap();
        let wrong = frame(
            event("not.registered", "wrong", "MUST-NOT-SUBMIT"),
            "wire-wrong",
            1,
            0,
        );
        socket
            .send(Message::Binary(wrong.encode().into()))
            .await
            .unwrap();
        let body = event("im.message.receive_v1", "msg-1", "ACTUAL-STREAM-INBOUND");
        let mid = body.len() / 2;
        socket
            .send(Message::Binary(
                frame(body[mid..].to_vec(), "wire-real", 2, 1)
                    .encode()
                    .into(),
            ))
            .await
            .unwrap();
        socket
            .send(Message::Binary(
                frame(body[..mid].to_vec(), "wire-real", 2, 0)
                    .encode()
                    .into(),
            ))
            .await
            .unwrap();
        socket
            .send(Message::Binary(
                frame(body, "wire-duplicate", 1, 0).encode().into(),
            ))
            .await
            .unwrap();
        let (mut acks, mut ping) = (0, false);
        while acks < 3 || !ping {
            let raw = socket.next().await.unwrap().unwrap();
            if let Message::Binary(raw) = raw {
                let ack = Frame::decode(&raw).unwrap();
                if ack.method == 0 {
                    assert_eq!(ack.header("type"), Some("ping"));
                    assert_eq!(ack.service, 42);
                    ping = true;
                } else {
                    assert_eq!(ack.seq, 9);
                    assert_eq!(ack.log, 17);
                    let payload: Value = serde_json::from_slice(&ack.payload).unwrap();
                    assert_eq!(payload["code"], 200);
                    acks += 1;
                }
            }
        }
        acked.send(()).unwrap();
        let outcome = socket.next().await;
        assert!(outcome.is_none() || matches!(outcome, Some(Ok(Message::Close(_))) | Some(Err(_))));
        closed.send(()).unwrap();
    });
    let (turns, mut turn_rx) = mpsc::unbounded_channel();
    let channel = FeishuChannel::new(
        FeishuOptions {
            app_id: "cli_0123456789abcdef".into(),
            app_secret: "local-secret".into(),
            base_url: base,
            ..Default::default()
        },
        no_interaction(),
    )
    .with_network(Arc::new(super::super::network::ChannelNetwork::default()));
    let handle = channel
        .start(ChannelStartDeps {
            turn_gateway: Some(Arc::new(Gateway(turns))),
            ..Default::default()
        })
        .await
        .unwrap();
    let endpoint = timeout(Duration::from_secs(3), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(endpoint.0, "/callback/ws/endpoint");
    assert_eq!(endpoint.1.as_deref(), Some("zh"));
    assert_eq!(
        endpoint.2,
        json!({"AppID":"cli_0123456789abcdef","AppSecret":"local-secret"})
    );
    let input = timeout(Duration::from_secs(3), turn_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(input.message, JsString::from("ACTUAL-STREAM-INBOUND"));
    assert_eq!(input.channel_key, ChannelKey::new("feishu"));
    let reply = timeout(Duration::from_secs(3), async {
        loop {
            let request = received.recv().await.unwrap();
            if request.0 == "/open-apis/im/v1/messages" {
                break request;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(reply.2["receive_id"], "local-chat");
    assert!(
        reply.2["content"]
            .as_str()
            .unwrap()
            .contains("ACTUAL-STREAM-REPLY"),
        "actual stream reply uses real Open API transport"
    );
    timeout(Duration::from_secs(3), ack_receipt)
        .await
        .unwrap()
        .unwrap();
    // First waiter is intentionally dropped. The same receipt still drains the socket owner.
    drop(handle.stop("first waiter dropped"));
    timeout(Duration::from_secs(3), handle.stop("second waiter"))
        .await
        .unwrap()
        .unwrap();
    timeout(Duration::from_secs(3), close_receipt)
        .await
        .unwrap()
        .unwrap();
    peer.await.unwrap();
    assert!(
        turn_rx.try_recv().is_err(),
        "wrong event and duplicate message_id must not submit"
    );
    http_task.abort();
    let _ = http_task.await;
}

struct HeldStream {
    polled: Option<oneshot::Sender<()>>,
    cancellations: Arc<std::sync::atomic::AtomicUsize>,
}
impl TurnEventStream for HeldStream {
    fn next(
        &mut self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<JsValue>, ChannelError>> + Send + '_>,
    > {
        Box::pin(async move {
            if let Some(signal) = self.polled.take() {
                let _ = signal.send(());
            }
            std::future::pending().await
        })
    }
    fn cancel_and_drain(
        &mut self,
        _reason: JsString,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ChannelError>> + Send + '_>>
    {
        let attempt = self
            .cancellations
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            if attempt == 0 {
                Err(ChannelError::error("held stream first drain failed"))
            } else {
                Ok(())
            }
        })
    }
}
struct HeldGateway(Mutex<Option<HeldStream>>);
impl TurnGateway for HeldGateway {
    fn submit(
        &self,
        _input: TurnRequest,
    ) -> BoxFuture<Result<Box<dyn TurnEventStream>, ChannelError>> {
        let stream = self.0.lock().unwrap().take().unwrap();
        Box::pin(async move { Ok(Box::new(stream) as Box<dyn TurnEventStream>) })
    }
}
#[tokio::test]
async fn owned_stop_retains_failed_turn_stream_and_retry_receipt_after_waiter_drop() {
    let (polled, receipt) = oneshot::channel();
    let cancellations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let gateway = Arc::new(HeldGateway(Mutex::new(Some(HeldStream {
        polled: Some(polled),
        cancellations: cancellations.clone(),
    }))));
    let channel = FeishuChannel::new(
        FeishuOptions {
            connection_mode: "webhook".into(),
            ..Default::default()
        },
        no_interaction(),
    )
    .with_network(Arc::new(super::super::network::ChannelNetwork::default()));
    let handle = channel
        .start(ChannelStartDeps {
            turn_gateway: Some(gateway),
            ..Default::default()
        })
        .await
        .unwrap();
    channel.runtime.handle_stream_event(&json!({"message":{"chat_id":"held","message_type":"text","content":"{\"text\":\"held\"}"}}));
    timeout(Duration::from_secs(3), receipt)
        .await
        .unwrap()
        .unwrap();
    assert!(
        timeout(Duration::from_secs(3), handle.stop("first"))
            .await
            .unwrap()
            .is_err()
    );
    assert_eq!(cancellations.load(std::sync::atomic::Ordering::SeqCst), 1);
    drop(handle.stop("retry waiter dropped"));
    timeout(Duration::from_secs(3), handle.stop("retry observer"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cancellations.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(channel.runtime.lifecycle.lock().unwrap().streams.is_empty());
}

#[tokio::test]
async fn owned_stop_cancels_pending_endpoint_exchange_before_ws_exists() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (accepted, receipt) = oneshot::channel();
    let (closed, close_receipt) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let (mut tcp, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 1024];
        loop {
            let n = tcp.read(&mut buffer).await.unwrap();
            assert_ne!(n, 0);
            bytes.extend_from_slice(&buffer[..n]);
            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                let size: usize = headers
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:").map(str::trim))
                    .unwrap()
                    .parse()
                    .unwrap();
                if bytes.len() >= end + 4 + size {
                    break;
                }
            }
        }
        accepted.send(()).unwrap();
        let result = tcp.read(&mut buffer).await;
        assert!(matches!(result, Ok(0)) || result.is_err());
        let _ = tcp.shutdown().await;
        closed.send(()).unwrap();
    });
    let channel = FeishuChannel::new(
        FeishuOptions {
            app_id: "cli_0123456789abcdef".into(),
            app_secret: "local-secret".into(),
            base_url: base,
            ..Default::default()
        },
        no_interaction(),
    )
    .with_network(Arc::new(super::super::network::ChannelNetwork::default()));
    let handle = channel.start(ChannelStartDeps::default()).await.unwrap();
    timeout(Duration::from_secs(3), receipt)
        .await
        .unwrap()
        .unwrap();
    drop(handle.stop("drop waiter during pending endpoint"));
    timeout(Duration::from_secs(3), handle.stop("observe receipt"))
        .await
        .unwrap()
        .unwrap();
    timeout(Duration::from_secs(3), close_receipt)
        .await
        .unwrap()
        .unwrap();
    peer.await.unwrap();
}

#[tokio::test]
async fn peer_close_repulls_endpoint_and_reconnect_delivers_real_event() {
    let ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = ws.local_addr().unwrap();
    let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", http.local_addr().unwrap());
    let pulls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = pulls.clone();
    let http_task = tokio::spawn(async move {
        loop {
            let (tcp, _) = http.accept().await.unwrap();
            let pulls = pulls.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                    let pulls = pulls.clone();
                    async move {
                        let path = request.uri().path().to_owned();
                        let _ = request.into_body().collect().await.unwrap();
                        let reply = if path == "/callback/ws/endpoint" {
                            pulls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            json!({"code":0,"data":{"URL":format!("ws://{addr}/reconnect?service_id=7&device_id=local"),"ClientConfig":{"PingInterval":60,"ReconnectCount":1,"ReconnectInterval":0,"ReconnectNonce":0}}})
                        } else {
                            json!({"code":0,"tenant_access_token":"local-token","expire":7200,"data":{"reaction_id":"r","message_id":"m"}})
                        };
                        Ok::<_, std::convert::Infallible>(Response::new(Full::new(Bytes::from(
                            reply.to_string(),
                        ))))
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(tcp), service)
                    .await;
            });
        }
    });
    let peer = tokio::spawn(async move {
        let (tcp, _) = ws.accept().await.unwrap();
        let mut first = accept_async(tcp).await.unwrap();
        let raw = first.next().await.unwrap().unwrap();
        assert!(matches!(raw, Message::Binary(_)));
        first.close(None).await.unwrap();
        drop(first);
        let (tcp, _) = ws.accept().await.unwrap();
        let mut second = accept_async(tcp).await.unwrap();
        second
            .send(Message::Binary(
                frame(
                    event(
                        "im.message.receive_v1",
                        "after-reconnect",
                        "AFTER-RECONNECT",
                    ),
                    "reconnected",
                    1,
                    0,
                )
                .encode()
                .into(),
            ))
            .await
            .unwrap();
        while let Some(message) = second.next().await {
            match message {
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
    });
    let (turns, mut received) = mpsc::unbounded_channel();
    let channel = FeishuChannel::new(
        FeishuOptions {
            app_id: "cli_0123456789abcdef".into(),
            app_secret: "local-secret".into(),
            base_url: base,
            ..Default::default()
        },
        no_interaction(),
    )
    .with_network(Arc::new(super::super::network::ChannelNetwork::default()));
    let handle = channel
        .start(ChannelStartDeps {
            turn_gateway: Some(Arc::new(Gateway(turns))),
            ..Default::default()
        })
        .await
        .unwrap();
    let request = timeout(Duration::from_secs(3), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(request.message, JsString::from("AFTER-RECONNECT"));
    assert_eq!(
        observed.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "every reconnect uses fresh endpoint credentials/config"
    );
    timeout(
        Duration::from_secs(3),
        handle.stop("reconnect test completed"),
    )
    .await
    .unwrap()
    .unwrap();
    timeout(Duration::from_secs(3), peer)
        .await
        .unwrap()
        .unwrap();
    http_task.abort();
    let _ = http_task.await;
}

#[tokio::test]
async fn fragment_count_4096_preserves_connection_4097_rejects_without_ack_or_dispatch() {
    let ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = ws.local_addr().unwrap();
    let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", http.local_addr().unwrap());
    let http_task = tokio::spawn(async move {
        loop {
            let (tcp, _) = http.accept().await.unwrap();
            tokio::spawn(async move {
                let service = service_fn(
                    move |request: hyper::Request<hyper::body::Incoming>| async move {
                        let path = request.uri().path().to_owned();
                        let _ = request.into_body().collect().await.unwrap();
                        let reply = if path == "/callback/ws/endpoint" {
                            json!({"code":0,"data":{"URL":format!("ws://{addr}/cap?service_id=11&device_id=local"),"ClientConfig":{"PingInterval":60,"ReconnectCount":0,"ReconnectInterval":0,"ReconnectNonce":0}}})
                        } else {
                            json!({"code":0,"tenant_access_token":"local-token","expire":7200,"data":{"reaction_id":"r","message_id":"m"}})
                        };
                        Ok::<_, std::convert::Infallible>(Response::new(Full::new(Bytes::from(
                            reply.to_string(),
                        ))))
                    },
                );
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(tcp), service)
                    .await;
            });
        }
    });
    let (accepted, accepted_receipt) = oneshot::channel();
    let (reject_now, reject_signal) = oneshot::channel();
    let (rejected, reject_receipt) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let (tcp, _) = ws.accept().await.unwrap();
        let mut socket = accept_async(tcp).await.unwrap();
        // This legal but incomplete message allocates exactly the permitted table.
        // The subsequent independent event proves the same connection remains usable.
        socket
            .send(Message::Binary(
                frame(vec![b'x'], "cap-4096-incomplete", 4096, 0)
                    .encode()
                    .into(),
            ))
            .await
            .unwrap();
        socket
            .send(Message::Binary(
                frame(
                    event(
                        "im.message.receive_v1",
                        "cap-witness",
                        "CAP-4096-STILL-ALIVE",
                    ),
                    "cap-legal-witness",
                    1,
                    0,
                )
                .encode()
                .into(),
            ))
            .await
            .unwrap();
        loop {
            if let Message::Binary(bytes) = socket.next().await.unwrap().unwrap() {
                let ack = Frame::decode(&bytes).unwrap();
                if ack.method == 1 {
                    assert_eq!(ack.header("message_id"), Some("cap-legal-witness"));
                    assert_eq!(
                        serde_json::from_slice::<Value>(&ack.payload).unwrap()["code"],
                        200
                    );
                    break;
                }
            }
        }
        accepted.send(()).unwrap();
        reject_signal.await.unwrap();
        socket
            .send(Message::Binary(
                frame(
                    event(
                        "im.message.receive_v1",
                        "over-limit",
                        "MUST-NOT-DISPATCH-4097",
                    ),
                    "cap-4097-reject",
                    4097,
                    0,
                )
                .encode()
                .into(),
            ))
            .await
            .unwrap();
        // No shutdown request has been issued. Only the input guard can retire
        // this connection; accepting 4097 or removing the cap hangs and fails.
        loop {
            match socket.next().await {
                None | Some(Ok(Message::Close(_))) | Some(Err(_)) => break,
                Some(Ok(Message::Binary(bytes))) => {
                    let packet = Frame::decode(&bytes).unwrap();
                    assert_ne!(
                        packet.method, 1,
                        "rejected fragment must never receive a success ACK"
                    );
                }
                _ => {}
            }
        }
        rejected.send(()).unwrap();
    });
    let (turns, mut received) = mpsc::unbounded_channel();
    let channel = FeishuChannel::new(
        FeishuOptions {
            app_id: "cli_0123456789abcdef".into(),
            app_secret: "local-secret".into(),
            base_url: base,
            ..Default::default()
        },
        no_interaction(),
    )
    .with_network(Arc::new(super::super::network::ChannelNetwork::default()));
    let handle = channel
        .start(ChannelStartDeps {
            turn_gateway: Some(Arc::new(Gateway(turns))),
            ..Default::default()
        })
        .await
        .unwrap();
    let input = timeout(Duration::from_secs(3), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(input.message, JsString::from("CAP-4096-STILL-ALIVE"));
    timeout(Duration::from_secs(3), accepted_receipt)
        .await
        .unwrap()
        .unwrap();
    reject_now.send(()).unwrap();
    timeout(Duration::from_secs(3), reject_receipt)
        .await
        .unwrap()
        .unwrap();
    timeout(Duration::from_secs(3), handle.stop("cap boundary finished"))
        .await
        .unwrap()
        .unwrap();
    assert!(
        received.try_recv().is_err(),
        "over-limit fragment must not submit a turn"
    );
    peer.await.unwrap();
    http_task.abort();
    let _ = http_task.await;
}
