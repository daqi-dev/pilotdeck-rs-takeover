//! The ordinary CLI's `GatewayWsClient`, shared by default chat and cron.
//! Its stream contract deliberately differs from the SDK: no sequence gate,
//! response frames do not finish streams, and a final event is not rendered.
use futures_util::{SinkExt, StreamExt, stream::SplitSink};
use pilotdeck_protocol::{JsObject, JsValue, parse_json, stringify_json};
use serde::Serialize;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio::{
    net::TcpStream,
    sync::{Mutex as AsyncMutex, mpsc},
    task::JoinHandle,
    time::timeout,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub(crate) struct RemoteGateway {
    writer: Arc<AsyncMutex<SplitSink<Socket, Message>>>,
    incoming: mpsc::UnboundedReceiver<Result<JsValue, String>>,
    reader: JoinHandle<()>,
    active: Arc<Mutex<Option<String>>>,
    connected: Arc<AtomicBool>,
}

impl RemoteGateway {
    pub(crate) async fn connect(url: &str, token: &str) -> Result<Self, String> {
        // GatewayWsClient uses the platform WebSocket without a message cap.
        let config = WebSocketConfig::default()
            .max_message_size(None)
            .max_frame_size(None);
        let (mut socket, _) = connect_async_with_config(url, Some(config), false)
            .await
            .map_err(|_| "Failed to connect to gateway WebSocket.".to_owned())?;
        let hello = async {
            let raw = serde_json::to_string(&Hello {
                kind: "hello",
                protocol_version: pilotdeck_protocol::GATEWAY_PROTOCOL_VERSION,
                client_name: "cli",
                client_version: "0.1.0",
                token,
            })
            .map_err(|error| error.to_string())?;
            socket
                .send(Message::Text(raw.into()))
                .await
                .map_err(|_| "Gateway WebSocket is not connected.".to_owned())?;
            loop {
                let frame = receive_socket(&mut socket).await?;
                if field_is(&frame, "type", "hello_ok") {
                    return Ok(());
                }
            }
        };
        let result = timeout(Duration::from_secs(10), hello)
            .await
            .map_err(|_| "Gateway hello timed out.".to_owned())
            .and_then(|result| result);
        if let Err(error) = result {
            let _ = timeout(Duration::from_secs(1), socket.close(None)).await;
            return Err(error);
        }
        let (writer, mut stream) = socket.split();
        let writer = Arc::new(AsyncMutex::new(writer));
        let (send, incoming) = mpsc::unbounded_channel();
        let active = Arc::new(Mutex::new(None::<String>));
        let connected = Arc::new(AtomicBool::new(true));
        let reader_writer = writer.clone();
        let reader_active = active.clone();
        let reader_connected = connected.clone();
        let reader = tokio::spawn(async move {
            let error = loop {
                match stream.next().await {
                    Some(Ok(Message::Text(raw))) => {
                        let frame = match parse_json(&raw) {
                            Ok(frame) => frame,
                            Err(_) => break "Invalid Gateway JSON frame.",
                        };
                        let wanted = reader_active
                            .lock()
                            .expect("CLI active request lock")
                            .as_ref()
                            .is_some_and(|id| field_is(&frame, "id", id));
                        if wanted
                            && (field_is(&frame, "type", "event")
                                || field_is(&frame, "type", "response"))
                            && send.send(Ok(frame)).is_err()
                        {
                            return;
                        }
                    }
                    Some(Ok(Message::Ping(_))) => {
                        if reader_writer.lock().await.flush().await.is_err() {
                            break "Gateway WebSocket closed.";
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                        break "Gateway WebSocket closed.";
                    }
                    Some(Ok(_)) => break "Invalid Gateway JSON frame.",
                }
            };
            reader_connected.store(false, Ordering::Release);
            let _ = send.send(Err(error.to_owned()));
        });
        Ok(Self {
            writer,
            incoming,
            reader,
            active,
            connected,
        })
    }

    /// GatewayWsClient.request has no deadline. Keep the caller's input field
    /// order, and return the lossless result needed for CLI JSON formatting.
    pub(crate) async fn request(
        &mut self,
        method: &str,
        params: impl Serialize,
    ) -> Result<JsValue, String> {
        let id = uuid::Uuid::new_v4().to_string();
        *self.active.lock().expect("CLI active request lock") = Some(id.clone());
        self.send(Request {
            kind: "request",
            id: &id,
            method,
            params,
        })
        .await?;
        loop {
            let frame = self.receive().await?;
            if !field_is(&frame, "type", "response") || !field_is(&frame, "id", &id) {
                continue;
            }
            if truthy(frame.get("ok")) {
                self.finish_request();
                return Ok(take_field(frame, "result"));
            }
            self.finish_request();
            let message = frame.get("error").and_then(|error| error.get("message"));
            return Err(match message {
                Some(JsValue::String(message)) => String::from_utf16_lossy(message.units()),
                _ => "Gateway request failed.".to_owned(),
            });
        }
    }

    pub(crate) async fn submit_turn(
        &mut self,
        input: JsValue,
        mut emit: impl FnMut(JsValue),
    ) -> Result<(), String> {
        let id = uuid::Uuid::new_v4().to_string();
        *self.active.lock().expect("CLI active request lock") = Some(id.clone());
        let mut frame = JsObject::new();
        frame.insert("type".into(), JsValue::String("request".into()));
        frame.insert("id".into(), JsValue::String(id.as_str().into()));
        frame.insert("method".into(), JsValue::String("submit_turn".into()));
        frame.insert("params".into(), input);
        let raw = stringify_json(&JsValue::Object(frame))
            .map_err(|error| error.to_string())?
            .expect("a request object always serializes");
        self.send_text(raw).await?;
        loop {
            let frame = self.receive().await?;
            if !field_is(&frame, "type", "event") || !field_is(&frame, "id", &id) {
                continue;
            }
            // Unlike SDK query(), GatewayWsClient closes without enqueueing
            // the payload of the final frame. It does not inspect `seq`.
            if truthy(frame.get("final")) {
                self.finish_request();
                return Ok(());
            }
            emit(take_field(frame, "event"));
        }
    }

    /// Cleanup is bounded so a peer that never acknowledges close cannot
    /// recreate the completed-command hang fixed by the CLI owner.
    pub(crate) async fn close(&mut self) {
        self.connected.store(false, Ordering::Release);
        self.finish_request();
        let _ = timeout(Duration::from_secs(1), async {
            self.writer.lock().await.close().await
        })
        .await;
        self.reader.abort();
        let _ = (&mut self.reader).await;
    }

    async fn send(&mut self, frame: impl Serialize) -> Result<(), String> {
        let raw = serde_json::to_string(&frame).map_err(|error| error.to_string())?;
        self.send_text(raw).await
    }

    async fn send_text(&mut self, raw: String) -> Result<(), String> {
        if !self.connected.load(Ordering::Acquire) {
            return Err("Gateway WebSocket is not connected.".to_owned());
        }
        self.writer
            .lock()
            .await
            .send(Message::Text(raw.into()))
            .await
            .map_err(|_| "Gateway WebSocket is not connected.".to_owned())
    }

    async fn receive(&mut self) -> Result<JsValue, String> {
        self.incoming
            .recv()
            .await
            .unwrap_or_else(|| Err("Gateway WebSocket closed.".to_owned()))
    }

    fn finish_request(&self) {
        *self.active.lock().expect("CLI active request lock") = None;
    }
}

impl Drop for RemoteGateway {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

async fn receive_socket(socket: &mut Socket) -> Result<JsValue, String> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(raw))) => {
                return parse_json(&raw).map_err(|_| "Invalid Gateway JSON frame.".to_owned());
            }
            Some(Ok(Message::Ping(_))) => {
                socket
                    .flush()
                    .await
                    .map_err(|_| "Gateway WebSocket closed.".to_owned())?;
            }
            Some(Ok(Message::Pong(_))) => {}
            Some(Ok(Message::Close(frame))) => {
                let reason = match frame {
                    Some(frame) if !frame.reason.is_empty() => frame.reason.to_string(),
                    Some(frame) => format!("code {}", u16::from(frame.code)),
                    None => "code 1005".to_owned(),
                };
                return Err(format!("Gateway closed during hello: {reason}"));
            }
            Some(Err(_)) | None => {
                return Err("Gateway closed during hello: code 1006".to_owned());
            }
            Some(Ok(_)) => return Err("Invalid Gateway JSON frame.".to_owned()),
        }
    }
}

fn field_is(value: &JsValue, name: &str, expected: &str) -> bool {
    value
        .get(name)
        .and_then(JsValue::as_string)
        .is_some_and(|value| value.equals_str(expected))
}

fn take_field(mut frame: JsValue, name: &str) -> JsValue {
    match &mut frame {
        JsValue::Object(frame) => frame
            .insert(name.into(), JsValue::Undefined)
            .unwrap_or(JsValue::Undefined),
        _ => JsValue::Undefined,
    }
}

fn truthy(value: Option<&JsValue>) -> bool {
    match value {
        None | Some(JsValue::Undefined | JsValue::Null) => false,
        Some(JsValue::Bool(value)) => *value,
        Some(JsValue::Number(value)) => *value != 0.0 && !value.is_nan(),
        Some(JsValue::String(value)) => !value.is_empty(),
        Some(JsValue::Array(_) | JsValue::Object(_)) => true,
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Hello<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    protocol_version: &'a str,
    client_name: &'a str,
    client_version: &'a str,
    token: &'a str,
}

#[derive(Serialize)]
struct Request<'a, P> {
    #[serde(rename = "type")]
    kind: &'a str,
    id: &'a str,
    method: &'a str,
    params: P,
}
