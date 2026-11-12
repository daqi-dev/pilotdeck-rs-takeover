use super::*;
use crate::runtime::{
    model::http::{ModelBody, fetch::fetch},
    network::UNDICI_BODY_TIMEOUT,
    tools::builtin::bash::runner::Utf8Stream,
};
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::{HeaderMap, Method, Response};
use pilotdeck_protocol::parse_json;

impl Shared {
    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<String>,
    ) -> Result<Response<ModelBody>, String> {
        let raw_url = format!("{}{path}", self.bridge_url);
        let url = url::Url::parse(&raw_url)
            .map_err(|_| format!("TypeError: Failed to parse URL from {raw_url}"))?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(
                "TypeError: Request cannot be constructed from a URL that includes credentials"
                    .into(),
            );
        }
        let mut headers = HeaderMap::new();
        if body.is_some() {
            headers.insert(
                "content-type",
                "application/json".parse().expect("static header"),
            );
        }
        fetch(
            self.transport.as_ref(),
            url,
            method,
            headers,
            Bytes::from(body.unwrap_or_default()),
        )
        .await
        .map_err(|error| format!("TypeError: {}", error.message))
    }

    async fn response_text(mut body: ModelBody) -> Result<String, String> {
        let mut decoder = Utf8Stream::default();
        let mut text = String::new();
        loop {
            let frame = tokio::time::timeout(UNDICI_BODY_TIMEOUT, body.frame())
                .await
                .map_err(|_| "TypeError: terminated".to_owned())?;
            let Some(frame) = frame else {
                break;
            };
            let frame = frame.map_err(|_| "TypeError: terminated".to_owned())?;
            if let Some(bytes) = frame.data_ref() {
                text.push_str(&decoder.decode(bytes));
            }
        }
        text.push_str(&decoder.flush());
        Ok(text)
    }

    pub(super) async fn wait_for_ready(&self) -> bool {
        let started = tokio::time::Instant::now();
        while started.elapsed() < Duration::from_secs(15) {
            let response = tokio::select! {
                biased; _ = self.stopped() => return false,
                response = tokio::time::timeout(Duration::from_secs(3), self.request(Method::GET, "/messages", None)) => response,
            };
            if response.is_ok_and(|response| {
                response.is_ok_and(|response| {
                    response.status().is_success() || response.status().as_u16() == 404
                })
            }) {
                return true;
            }
            tokio::select! { biased; _ = self.stopped() => return false, _ = tokio::time::sleep(Duration::from_millis(400)) => {} }
        }
        false
    }

    pub(super) async fn poll_loop(self: Arc<Self>) {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! { biased; _ = self.stopped() => return, _ = interval.tick() => {} }
            let mut polls = self.polls.lock().expect("whatsapp polls");
            if *self.stopping.borrow() {
                return;
            }
            self.harvest_finished(&mut polls, "poll");
            let shared = self.clone();
            polls.push(tokio::spawn(async move {
                tokio::select! { biased; _ = shared.stopped() => {}, _ = shared.clone().poll_once() => {} }
            }));
        }
    }

    pub(super) async fn poll_once(self: Arc<Self>) {
        if !self.running.load(Ordering::Acquire) {
            return;
        }
        let result = async {
            let response = self.request(Method::GET, "/messages", None).await?;
            if !response.status().is_success() {
                self.log(
                    "warn",
                    &format!(
                        "whatsapp: poll {} {}",
                        response.status().as_u16(),
                        response.status().canonical_reason().unwrap_or("")
                    ),
                );
                return Ok::<_, String>(());
            }
            let text = Self::response_text(response.into_body()).await?;
            let data = parse_json(&text).map_err(|error| format!("SyntaxError: {error}"))?;
            let mut continuations = Vec::new();
            for message in wire::messages(&data)? {
                if *self.stopping.borrow() {
                    break;
                }
                if !self
                    .state
                    .lock()
                    .expect("whatsapp state")
                    .seen_ids
                    .insert(message.id.clone())
                {
                    continue;
                }
                let shared = self.clone();
                let mut future = Box::pin(async move {
                    shared.dispatch(message).await;
                });
                // JS runs each async handler to its first await before the
                // next item. Reserve all same-batch busy slots before spawn.
                if future.as_mut().now_or_never().is_none() {
                    continuations.push(future);
                }
            }
            let mut handlers = self.handlers.lock().expect("whatsapp handlers");
            self.harvest_finished(&mut handlers, "handler");
            for future in continuations {
                handlers.push(tokio::spawn(future));
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            self.log("error", &format!("whatsapp: poll error: {error}"));
        }
    }

    pub(super) async fn send_reply_owned(&self, chat: &JsString, text: &JsString) -> bool {
        if !self.running.load(Ordering::Acquire) || *self.stopping.borrow() {
            return false;
        }
        let mut body = JsObject::new();
        body.insert("chatId".into(), JsValue::String(chat.clone()));
        body.insert("message".into(), JsValue::String(text.clone()));
        let body = stringify_json(&JsValue::Object(body))
            .expect("finite WhatsApp send body")
            .expect("object JSON");
        let exchange = async {
            let response = self.request(Method::POST, "/send", Some(body)).await?;
            if response.status().is_success() {
                return Ok::<bool, String>(true);
            }
            let status = response.status().as_u16();
            let status_text = response.status().canonical_reason().unwrap_or("");
            let raw = Self::response_text(response.into_body())
                .await
                .ok()
                .and_then(|text| parse_json(&text).ok());
            let error = raw
                .as_ref()
                .and_then(|raw| raw.get("error"))
                .filter(|error| !sms::nullish(error))
                .map(wire::string)
                .transpose()?
                .unwrap_or_else(|| status_text.into());
            self.log(
                "error",
                &format!(
                    "whatsapp: send HTTP {status}: {}",
                    String::from_utf16_lossy(error.units())
                ),
            );
            Ok(false)
        };
        let result = tokio::select! {
            biased; _ = self.stopped() => return false,
            result = tokio::time::timeout(Duration::from_secs(60), exchange) => result.unwrap_or_else(|_| Err("TimeoutError: The operation was aborted due to timeout".into())),
        };
        match result {
            Ok(delivered) => delivered,
            Err(error) => {
                self.log("error", &format!("whatsapp: send failed: {error}"));
                false
            }
        }
    }
}
