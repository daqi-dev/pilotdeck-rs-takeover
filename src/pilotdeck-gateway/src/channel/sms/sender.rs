//! Twilio Messages boundary. The production factory implements provider REST
//! directly. The injectable factory remains for fixed-baseline SDK-call tests;
//! no optional JS SDK is required by a native production adapter.
use std::sync::Arc;

use pilotdeck_protocol::{JsObject, JsString, JsValue};

use crate::channel::protocol::{BoxFuture, ChannelError};

/// `client.messages.create({body, from, to})`.
#[derive(Clone, Debug)]
pub struct SmsOutgoingMessage {
    pub body: JsString,
    pub from: JsString,
    pub to: JsString,
}
impl SmsOutgoingMessage {
    pub fn to_js(&self) -> JsObject {
        let mut object = JsObject::new();
        object.insert("body".into(), JsValue::String(self.body.clone()));
        object.insert("from".into(), JsValue::String(self.from.clone()));
        object.insert("to".into(), JsValue::String(self.to.clone()));
        object
    }
}

pub trait SmsSender: Send + Sync {
    fn send(&self, message: SmsOutgoingMessage) -> BoxFuture<Result<(), ChannelError>>;
}

/// `twilio(accountSid, authToken)` — the constructor call in `start()`.
pub type TwilioFactory =
    Arc<dyn Fn(&JsString, &JsString) -> Result<Arc<dyn SmsSender>, ChannelError> + Send + Sync>;

/// Provider REST implementation. The baseline SDK is unpinned/optional; this
/// boundary implements Twilio's documented Messages resource, with no Node.
pub(crate) fn native_factory(
    transport: Arc<dyn crate::runtime::model::http::ModelHttpTransport>,
) -> TwilioFactory {
    native_factory_at(transport, "https://api.twilio.com".into())
}
pub(crate) fn native_factory_at(
    transport: Arc<dyn crate::runtime::model::http::ModelHttpTransport>,
    base: String,
) -> TwilioFactory {
    Arc::new(move |sid, token| {
        Ok(Arc::new(NativeSender {
            transport: transport.clone(),
            base: base.clone(),
            sid: sid.clone(),
            token: token.clone(),
        }))
    })
}
struct NativeSender {
    transport: Arc<dyn crate::runtime::model::http::ModelHttpTransport>,
    base: String,
    sid: JsString,
    token: JsString,
}
impl SmsSender for NativeSender {
    fn send(&self, message: SmsOutgoingMessage) -> BoxFuture<Result<(), ChannelError>> {
        use base64::Engine;
        use bytes::Bytes;
        use http_body_util::BodyExt;
        use hyper::{HeaderMap, Method};
        let transport = self.transport.clone();
        let sid = String::from_utf16_lossy(self.sid.units());
        let authorization = base64::engine::general_purpose::STANDARD.encode(format!(
            "{sid}:{}",
            String::from_utf16_lossy(self.token.units())
        ));
        let target = format!(
            "{}/2010-04-01/Accounts/{}/Messages.json",
            self.base.trim_end_matches('/'),
            crate::channel::http_client::encode_uri_component(&sid)
        );
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("Body", &String::from_utf16_lossy(message.body.units()))
            .append_pair("From", &String::from_utf16_lossy(message.from.units()))
            .append_pair("To", &String::from_utf16_lossy(message.to.units()))
            .finish();
        Box::pin(async move {
            let url = url::Url::parse(&target).map_err(|e| ChannelError::error(e.to_string()))?;
            let mut headers = HeaderMap::new();
            headers.insert(
                "authorization",
                format!("Basic {authorization}")
                    .parse()
                    .map_err(|_| ChannelError::error("invalid SMS authorization"))?,
            );
            headers.insert(
                "content-type",
                "application/x-www-form-urlencoded"
                    .parse()
                    .expect("content type"),
            );
            headers.insert("accept", "application/json".parse().expect("accept"));
            let response = crate::runtime::model::http::fetch::fetch(
                transport.as_ref(),
                url,
                Method::POST,
                headers,
                Bytes::from(body),
            )
            .await
            .map_err(|e| ChannelError::error(e.message))?;
            let status = response.status();
            if status.is_success() {
                return Ok(());
            }
            let body = tokio::time::timeout(
                crate::runtime::network::UNDICI_BODY_TIMEOUT,
                response.into_body().collect(),
            )
            .await
            .map_err(|_| ChannelError::error("SMS response body timed out"))?
            .map_err(|_| ChannelError::error("SMS response body terminated"))?
            .to_bytes();
            let text = String::from_utf8_lossy(&body);
            let message = pilotdeck_protocol::parse_json(&text)
                .ok()
                .and_then(|v| v.get("message").map(super::js_string_of))
                .map(|v| String::from_utf16_lossy(v.units()))
                .unwrap_or_else(|| format!("Twilio HTTP {}: {text}", status.as_u16()));
            Err(ChannelError::error(message))
        })
    }
}
