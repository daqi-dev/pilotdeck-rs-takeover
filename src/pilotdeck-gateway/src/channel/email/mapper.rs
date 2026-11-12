use crate::channel::sms;
use pilotdeck_protocol::JsString;
use std::{collections::HashMap, sync::Arc};
#[derive(Default, Clone)]
pub struct EmailSessionMapperState {
    pub active_by_chat_id: HashMap<JsString, JsString>,
}
pub struct EmailSessionMapper {
    state: EmailSessionMapperState,
    uuid: Arc<dyn Fn() -> JsString + Send + Sync>,
}
pub struct MappedSession {
    pub session_key: JsString,
    pub command_new: bool,
    pub message: JsString,
}
impl Default for EmailSessionMapper {
    fn default() -> Self {
        Self::new(
            EmailSessionMapperState::default(),
            Arc::new(|| uuid::Uuid::new_v4().to_string().into()),
        )
    }
}
impl EmailSessionMapper {
    pub fn new(
        state: EmailSessionMapperState,
        uuid: Arc<dyn Fn() -> JsString + Send + Sync>,
    ) -> Self {
        Self { state, uuid }
    }
    pub fn snapshot(&self) -> EmailSessionMapperState {
        self.state.clone()
    }
    pub fn resolve(&mut self, chat: &JsString, text: &JsString) -> MappedSession {
        let text = sms::trim_units(text.units());
        let new = text == [47, 110, 101, 119] || text.starts_with(&[47, 110, 101, 119, 32]);
        let key = if new {
            let mut key: Vec<_> = "email:chat=".encode_utf16().collect();
            key.extend_from_slice(chat.units());
            key.extend(":s_".encode_utf16());
            key.extend_from_slice((self.uuid)().units());
            let key = JsString::from_utf16(key);
            self.state
                .active_by_chat_id
                .insert(chat.clone(), key.clone());
            key
        } else {
            self.state
                .active_by_chat_id
                .get(chat)
                .cloned()
                .unwrap_or_else(|| {
                    let mut key: Vec<_> = "email:chat=".encode_utf16().collect();
                    key.extend_from_slice(chat.units());
                    key.extend(":general".encode_utf16());
                    JsString::from_utf16(key)
                })
        };
        MappedSession {
            session_key: key,
            command_new: new,
            message: JsString::from_utf16(
                sms::trim_units(if new { &text[4..] } else { text }).to_vec(),
            ),
        }
    }
}
