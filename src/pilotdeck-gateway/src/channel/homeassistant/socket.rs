use std::sync::Arc;

use futures_util::{FutureExt, SinkExt, StreamExt};
use pilotdeck_protocol::{JsString, JsValue, parse_json, stringify_json};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

use super::{
    ChannelError, RECONNECT_DELAY, Shared, is_kind, js_string_of, nullish, object, trim_units,
    truthy,
};

pub(super) fn http_to_ws(base: &JsValue) -> Result<String, ChannelError> {
    let Some(base) = base.as_string() else {
        return Err(ChannelError::Plain(
            "TypeError: base.startsWith is not a function".into(),
        ));
    };
    let base = String::from_utf16_lossy(base.units());
    let base = if base.starts_with("http") {
        base
    } else {
        format!("http://{base}")
    };
    let url =
        url::Url::parse(&base).map_err(|_| ChannelError::Plain("TypeError: Invalid URL".into()))?;
    let protocol = if url.scheme() == "https" { "wss" } else { "ws" };
    let host = &url[url::Position::BeforeHost..url::Position::AfterPort];
    let path = url.path().strip_suffix('/').unwrap_or(url.path());
    Ok(format!("{protocol}://{host}{path}/api/websocket"))
}

pub(super) async fn run(shared: Arc<Shared>, url: String, mut stopped: watch::Receiver<bool>) {
    loop {
        if *stopped.borrow() {
            return;
        }
        let connected = tokio::select! {
            biased;
            _ = stopped.changed() => return,
            result = shared.network.connect_ws(url.as_str()) => result,
        };
        if let Ok((mut socket, _)) = connected {
            let (sender, mut outbound) = mpsc::unbounded_channel();
            {
                shared.runtime.lock().expect("HA runtime").socket = Some(sender);
            }
            loop {
                tokio::select! {
                    biased;
                    _ = stopped.changed() => {
                        // stop() itself is synchronous like ws.close(); ownership
                        // stays in this task while a bounded close frame flushes.
                        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), socket.close(None)).await;
                        shared.disconnect(); return;
                    }
                    message = outbound.recv() => {
                        let Some(message) = message else { break; };
                        let sent = tokio::select! { biased; _ = stopped.changed() => { shared.disconnect(); return; }, result = socket.send(Message::Text(message.into())) => result };
                        if let Err(error) = sent { shared.log("error", format!("homeassistant: send failed: Error: {error}")); break; }
                    }
                    frame = socket.next() => {
                        // Delay busy release until the frames already ready in
                        // this read batch have been dispatched, like Node's
                        // message callbacks before async-iterator microtasks.
                        let batch = shared.read_batch();
                        let mut frame = frame;
                        let mut disconnected = false;
                        loop {
                            match frame {
                                Some(Ok(Message::Text(raw))) => on_message(&shared, raw.as_str()),
                                // The formal ESM entry uses Node WebSocket's
                                // default Blob mode. String(Blob) is not JSON.
                                Some(Ok(Message::Binary(_))) => {},
                                Some(Ok(Message::Close(_))) | None => { disconnected = true; break; }
                                Some(Err(error)) => { shared.log("error", format!("homeassistant: ws error: Error: {error}")); disconnected = true; break; }
                                _ => {}
                            }
                            match socket.next().now_or_never() { Some(next) => frame = next, None => break }
                        }
                        shared.finish_read_batch(batch).await;
                        if disconnected { break; }
                    },
                }
            }
        } else if let Err(error) = connected {
            if matches!(error, tokio_tungstenite::tungstenite::Error::Http(_)) {
                // The formal Node WebSocket emits ErrorEvent, without a close
                // event, when the upgrade is rejected. Initial startup still
                // waits for its auth deadline; a rejected reconnect does not
                // schedule another attempt. Closing that failed CONNECTING
                // instance emits a second ErrorEvent.
                shared.log("error", "homeassistant: ws error: [object ErrorEvent]");
                if !*stopped.borrow() {
                    let _ = stopped.changed().await;
                }
                shared.log("error", "homeassistant: ws error: [object ErrorEvent]");
                return;
            }
            shared.log("error", format!("homeassistant: ws error: Error: {error}"));
        }
        if !shared.disconnect() {
            return;
        }
        tokio::select! { biased; _ = stopped.changed() => return, _ = tokio::time::sleep(RECONNECT_DELAY) => {} }
    }
}

pub(super) fn on_message(shared: &Arc<Shared>, raw: &str) {
    let Ok(message) = parse_json(raw) else {
        return;
    };
    if is_kind(&message, "result") {
        if let Some(JsValue::Number(id)) = message.get("id")
            && let Some(pending) = shared
                .runtime
                .lock()
                .expect("HA runtime")
                .pending
                .remove(&id.to_bits())
        {
            let _ = pending.send(matches!(message.get("success"), Some(JsValue::Bool(true))));
        }
    } else if is_kind(&message, "auth_required") {
        shared.send_json(object([
            ("type", "auth".into()),
            ("access_token", shared.token.clone()),
        ]));
    } else if is_kind(&message, "auth_ok") {
        let id = shared.next_id();
        shared.send_json(object([
            ("id", JsValue::Number(id)),
            ("type", "subscribe_events".into()),
            ("event_type", "state_changed".into()),
        ]));
        shared.runtime.lock().expect("HA runtime").ready = true;
        shared.settle_auth(true);
    } else if is_kind(&message, "auth_invalid") {
        let message = message
            .get("message")
            .filter(|value| !nullish(value))
            .map(js_string_of)
            .unwrap_or_else(|| "invalid token".into());
        shared.log(
            "error",
            format!("homeassistant: auth invalid: {}", super::display(&message)),
        );
        shared.settle_auth(false);
    } else if is_kind(&message, "event")
        && let Some((chat, text)) = event_input(&message, &shared.prefixes)
    {
        shared.dispatch(chat, text);
    }
}

fn strict_equal(left: &JsValue, right: &JsValue) -> bool {
    match (left, right) {
        (JsValue::Undefined, JsValue::Undefined) | (JsValue::Null, JsValue::Null) => true,
        (JsValue::Bool(a), JsValue::Bool(b)) => a == b,
        (JsValue::Number(a), JsValue::Number(b)) => a == b,
        (JsValue::String(a), JsValue::String(b)) => a == b,
        _ => false, // Distinct object/array occurrences have distinct JS identity.
    }
}

pub(super) fn event_input(
    message: &JsValue,
    prefixes: &[JsString],
) -> Option<(JsString, JsString)> {
    let event = message.get("event")?;
    if !event
        .get("event_type")?
        .as_string()?
        .equals_str("state_changed")
    {
        return None;
    }
    let data = event.get("data")?;
    let chat = data.get("entity_id")?.as_string()?;
    if chat.is_empty()
        || !prefixes
            .iter()
            .any(|prefix| chat.units().starts_with(prefix.units()))
    {
        return None;
    }
    let missing = JsValue::Undefined;
    let new = data
        .get("new_state")
        .and_then(|state| state.get("state"))
        .unwrap_or(&missing);
    let old = data
        .get("old_state")
        .and_then(|state| state.get("state"))
        .unwrap_or(&missing);
    if strict_equal(new, old) {
        return None;
    }
    let text = match new {
        JsValue::String(text) => text.clone(),
        _ => stringify_json(new).ok().flatten()?.into(),
    };
    if !truthy(&JsValue::String(text.clone())) || trim_units(text.units()).is_empty() {
        return None;
    }
    Some((chat.clone(), text))
}
