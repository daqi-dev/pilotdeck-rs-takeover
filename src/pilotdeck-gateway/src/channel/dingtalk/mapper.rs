use crate::channel::sms::{concat_units, trim_units};
use pilotdeck_protocol::JsString;
use std::collections::HashMap;

#[derive(Clone, Default)]
pub struct DingTalkSessionMapperState {
    pub active_by_chat_id: HashMap<JsString, JsString>,
}
pub struct DingTalkSessionMapper {
    state: DingTalkSessionMapperState,
    uuid: Box<dyn Fn() -> JsString + Send + Sync>,
}
pub struct MappedSession {
    pub session_key: JsString,
    pub message: JsString,
    pub command_new: bool,
}
impl Default for DingTalkSessionMapper {
    fn default() -> Self {
        Self::new(
            Default::default(),
            Box::new(|| uuid::Uuid::new_v4().to_string().into()),
        )
    }
}
impl DingTalkSessionMapper {
    pub fn new(
        state: DingTalkSessionMapperState,
        uuid: Box<dyn Fn() -> JsString + Send + Sync>,
    ) -> Self {
        Self { state, uuid }
    }
    pub fn resolve(&mut self, chat: &JsString, text: &JsString) -> MappedSession {
        let units = trim_units(text.units());
        let command_new =
            units == [47, 110, 101, 119] || units.starts_with(&[47, 110, 101, 119, 32]);
        let prefix = "dingtalk:chat=".encode_utf16().collect::<Vec<_>>();
        if command_new {
            let key = concat_units(&[&prefix, chat.units(), &[58, 115, 95], (self.uuid)().units()]);
            self.state
                .active_by_chat_id
                .insert(chat.clone(), key.clone());
            return MappedSession {
                session_key: key,
                message: JsString::from_utf16(trim_units(&units[4..]).to_vec()),
                command_new,
            };
        }
        MappedSession {
            session_key: self
                .state
                .active_by_chat_id
                .get(chat)
                .cloned()
                .unwrap_or_else(|| {
                    concat_units(&[
                        &prefix,
                        chat.units(),
                        &":general".encode_utf16().collect::<Vec<_>>(),
                    ])
                }),
            message: JsString::from_utf16(units.to_vec()),
            command_new,
        }
    }
    pub fn snapshot(&self) -> DingTalkSessionMapperState {
        self.state.clone()
    }
}
