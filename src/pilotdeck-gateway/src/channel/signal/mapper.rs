use std::collections::HashMap;

use pilotdeck_protocol::JsString;

use crate::channel::sms::{concat_units, trim_units};

#[derive(Clone, Default)]
pub struct SignalSessionMapperState {
    pub active_by_chat_id: HashMap<JsString, JsString>,
}

pub struct SignalSessionMapper {
    state: SignalSessionMapperState,
    uuid: Box<dyn Fn() -> JsString + Send + Sync>,
}

pub struct MappedSession {
    pub session_key: JsString,
    pub command_new: bool,
    pub message: JsString,
}

impl Default for SignalSessionMapper {
    fn default() -> Self {
        Self::new(
            SignalSessionMapperState::default(),
            Box::new(|| JsString::from(uuid::Uuid::new_v4().to_string())),
        )
    }
}

impl SignalSessionMapper {
    pub fn new(
        state: SignalSessionMapperState,
        uuid: Box<dyn Fn() -> JsString + Send + Sync>,
    ) -> Self {
        Self { state, uuid }
    }

    pub fn resolve(&mut self, chat_id: &JsString, text: &JsString) -> MappedSession {
        let text = trim_units(text.units());
        let command_new = text == [47, 110, 101, 119] || text.starts_with(&[47, 110, 101, 119, 32]);
        if command_new {
            let key = concat_units(&[
                &"signal:chat=".encode_utf16().collect::<Vec<_>>(),
                chat_id.units(),
                &[58, 115, 95],
                (self.uuid)().units(),
            ]);
            self.state
                .active_by_chat_id
                .insert(chat_id.clone(), key.clone());
            return MappedSession {
                session_key: key,
                command_new,
                message: JsString::from_utf16(trim_units(&text[4..]).to_vec()),
            };
        }
        MappedSession {
            session_key: self
                .state
                .active_by_chat_id
                .get(chat_id)
                .cloned()
                .unwrap_or_else(|| {
                    concat_units(&[
                        &"signal:chat=".encode_utf16().collect::<Vec<_>>(),
                        chat_id.units(),
                        &":general".encode_utf16().collect::<Vec<_>>(),
                    ])
                }),
            command_new,
            message: JsString::from_utf16(text.to_vec()),
        }
    }

    pub fn snapshot(&self) -> SignalSessionMapperState {
        self.state.clone()
    }
}
