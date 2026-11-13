//! The CLI's connection to a running server: `connectRemoteGatewayIfAvailable`
//! (src/gateway/client/probeServer.ts:33-45) and the slice of `RemoteGateway`
//! over `GatewayWsClient` (src/gateway/client/RemoteGateway.ts,
//! GatewayWsClient.ts) that `pilotdeck cron` and the default CliChannel use.
//! Only the fixed probe address is ever connected to.
use super::paths::Env;
use super::probe;
use futures_util::{SinkExt, StreamExt};
use pilotdeck_protocol::{GATEWAY_PROTOCOL_VERSION, JsObject, JsValue, parse_json, stringify_json};
use std::path::Path;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::runtime::Runtime;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// `toWsUrl("http://127.0.0.1:18789")` (probeServer.ts:47-52).
const URL: &str = "ws://127.0.0.1:18789/ws";
/// The hello wait (GatewayWsClient.ts:120-125).
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
/// Every pending request and stream fails with this when the socket closes
/// (GatewayWsClient.ts:92-107).
const CLOSED: &str = "Gateway WebSocket closed.";

pub struct RemoteGateway {
    runtime: Runtime,
    socket: Socket,
}

/// Probe, read the token, connect and say hello as client `cli`. Any failure
/// on the way is "no server" (`undefined`), as in the baseline.
pub fn connect_if_available(env: &Env, cwd: &Path) -> Option<RemoteGateway> {
    let token = probe::probe_gateway_server(env, cwd)?;
    RemoteGateway::connect(&token)
}

fn object<const N: usize>(entries: [(&str, JsValue); N]) -> JsObject {
    let mut object = JsObject::new();
    for (name, value) in entries {
        object.insert(name.into(), value);
    }
    object
}

fn text(value: JsValue) -> Message {
    let raw = stringify_json(&value).ok().flatten().unwrap_or_default();
    Message::text(raw)
}

fn is_type(frame: &JsValue, kind: &str) -> bool {
    frame
        .get("type")
        .and_then(JsValue::as_string)
        .is_some_and(|value| value.equals_str(kind))
}

fn has_id(frame: &JsValue, id: &str) -> bool {
    frame
        .get("id")
        .and_then(JsValue::as_string)
        .is_some_and(|value| value.equals_str(id))
}

/// JS truthiness of a frame field (`if (frame.ok)`, `if (!frame.final)`).
fn truthy(value: Option<&JsValue>) -> bool {
    match value {
        None | Some(JsValue::Undefined | JsValue::Null) => false,
        Some(JsValue::Bool(value)) => *value,
        Some(JsValue::Number(value)) => *value != 0.0 && !value.is_nan(),
        Some(JsValue::String(value)) => !value.is_empty(),
        Some(JsValue::Array(_) | JsValue::Object(_)) => true,
    }
}

/// The next parsed frame; a close (either side), a socket error or the end of
/// the stream is the `close` event.
async fn next_frame(socket: &mut Socket) -> Result<JsValue, String> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(raw))) => {
                if let Ok(frame) = parse_json(&raw) {
                    return Ok(frame);
                }
            }
            Some(Ok(Message::Close(_)) | Err(_)) | None => return Err(CLOSED.to_owned()),
            Some(Ok(_)) => {}
        }
    }
}

impl RemoteGateway {
    /// `createRemoteGateway({ url, token, clientName: "cli" })`: open, send the
    /// hello, wait up to ten seconds for `hello_ok`. A close before it (for
    /// example 4003 `auth_failed`) rejects.
    fn connect(token: &str) -> Option<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        let socket = runtime.block_on(async {
            let (mut socket, _) = tokio_tungstenite::connect_async(URL).await.ok()?;
            let hello = object([
                ("type", "hello".into()),
                ("protocolVersion", GATEWAY_PROTOCOL_VERSION.into()),
                ("clientName", "cli".into()),
                ("clientVersion", "0.1.0".into()),
                ("token", token.into()),
            ]);
            socket.send(text(JsValue::Object(hello))).await.ok()?;
            let hello_ok = tokio::time::timeout(HELLO_TIMEOUT, async {
                loop {
                    let frame = next_frame(&mut socket).await.ok()?;
                    if is_type(&frame, "hello_ok") {
                        return Some(());
                    }
                }
            });
            hello_ok.await.ok()??;
            Some(socket)
        })?;
        Some(Self { runtime, socket })
    }

    fn send_request(&mut self, method: &str, params: JsObject) -> Result<String, String> {
        let id = uuid::Uuid::new_v4().to_string();
        let frame = object([
            ("type", "request".into()),
            ("id", id.as_str().into()),
            ("method", method.into()),
            ("params", JsValue::Object(params)),
        ]);
        let Self { runtime, socket } = self;
        runtime
            .block_on(socket.send(text(JsValue::Object(frame))))
            .map_err(|_| "Gateway WebSocket is not connected.".to_owned())?;
        Ok(id)
    }

    /// `client.request(method, params)`: the matching response's `result`
    /// (undefined when absent), or its `error.message` as a thrown error.
    /// There is no timeout; only a close ends the wait.
    pub fn request(&mut self, method: &str, params: JsObject) -> Result<JsValue, String> {
        let id = self.send_request(method, params)?;
        let Self { runtime, socket } = self;
        runtime.block_on(async {
            loop {
                let frame = next_frame(socket).await?;
                if !is_type(&frame, "response") || !has_id(&frame, &id) {
                    continue;
                }
                if truthy(frame.get("ok")) {
                    return Ok(frame.get("result").cloned().unwrap_or(JsValue::Undefined));
                }
                let message = frame
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(JsValue::as_string)
                    .and_then(|message| message.to_utf8().ok());
                return Err(message.unwrap_or_else(|| "Gateway request failed.".to_owned()));
            }
        })
    }

    /// `client.stream("submit_turn", params)`: each non-final event frame's
    /// `event` in order; the final frame ends the stream without yielding its
    /// event. Response frames for a stream id have no pending request and are
    /// ignored.
    pub fn submit_turn(
        &mut self,
        params: JsObject,
        mut on_event: impl FnMut(&JsValue),
    ) -> Result<(), String> {
        let id = self.send_request("submit_turn", params)?;
        let Self { runtime, socket } = self;
        runtime.block_on(async {
            loop {
                let frame = next_frame(socket).await?;
                if !is_type(&frame, "event") || !has_id(&frame, &id) {
                    continue;
                }
                if truthy(frame.get("final")) {
                    return Ok(());
                }
                if let Some(event) = frame.get("event").filter(|event| truthy(Some(event))) {
                    on_event(event);
                }
            }
        })
    }

    /// Close the socket with a close handshake (bounded).
    pub fn close(mut self) {
        let Self { runtime, socket } = &mut self;
        runtime.block_on(async {
            let _ = tokio::time::timeout(CLOSE_TIMEOUT, socket.close(None)).await;
        });
    }
}
