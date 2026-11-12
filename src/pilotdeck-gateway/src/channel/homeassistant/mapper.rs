//! cc7c4e6 `HomeAssistantSessionMapper.ts`: instance-local chat sessions.
use std::{collections::HashMap, sync::Arc};

use pilotdeck_protocol::JsString;

use crate::channel::sms::trim_units;

#[derive(Clone, Default)]
pub struct HomeAssistantSessionMapperState {
    pub active_by_chat_id: HashMap<JsString, JsString>,
}

pub struct HomeAssistantSessionMapper {
    state: HomeAssistantSessionMapperState,
    uuid: Arc<dyn Fn() -> JsString + Send + Sync>,
}

pub struct MappedSession {
    pub session_key: JsString,
    pub command_new: bool,
    pub message: JsString,
}

impl Default for HomeAssistantSessionMapper {
    fn default() -> Self {
        Self::new(
            Default::default(),
            Arc::new(|| uuid::Uuid::new_v4().to_string().into()),
        )
    }
}

impl HomeAssistantSessionMapper {
    pub fn new(
        state: HomeAssistantSessionMapperState,
        uuid: Arc<dyn Fn() -> JsString + Send + Sync>,
    ) -> Self {
        Self { state, uuid }
    }

    pub fn resolve(&mut self, chat: &JsString, text: &JsString) -> MappedSession {
        let units = trim_units(text.units());
        let command_new =
            units == [47, 110, 101, 119] || units.starts_with(&[47, 110, 101, 119, 32]);
        let session_key = if command_new {
            let mut key: Vec<u16> = "homeassistant:chat=".encode_utf16().collect();
            key.extend(chat.units());
            key.extend(":s_".encode_utf16());
            key.extend((self.uuid)().units());
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
                    let mut key: Vec<u16> = "homeassistant:chat=".encode_utf16().collect();
                    key.extend(chat.units());
                    key.extend(":general".encode_utf16());
                    JsString::from_utf16(key)
                })
        };
        MappedSession {
            session_key,
            command_new,
            message: JsString::from_utf16(
                if command_new {
                    trim_units(&units[4..])
                } else {
                    units
                }
                .to_vec(),
            ),
        }
    }

    pub fn snapshot(&self) -> HomeAssistantSessionMapperState {
        self.state.clone()
    }
}
