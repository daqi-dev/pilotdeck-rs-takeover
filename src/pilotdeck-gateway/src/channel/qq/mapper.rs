use pilotdeck_protocol::{JsObject, JsString, JsValue};

use crate::channel::sms::{concat_units, trim_units};

#[derive(Clone, Default)]
pub struct QQSessionMapperState {
    pub active_by_chat_key: JsObject,
}

pub struct QQSessionMapper {
    state: QQSessionMapperState,
    uuid: Box<dyn Fn() -> JsString + Send + Sync>,
}

pub struct MappedSession {
    pub session_key: JsString,
    pub command_new: bool,
    pub message: JsString,
}

impl Default for QQSessionMapper {
    fn default() -> Self {
        Self::new(
            QQSessionMapperState::default(),
            Box::new(|| JsString::from(uuid::Uuid::new_v4().to_string())),
        )
    }
}

impl QQSessionMapper {
    pub fn new(state: QQSessionMapperState, uuid: Box<dyn Fn() -> JsString + Send + Sync>) -> Self {
        Self { state, uuid }
    }

    pub fn resolve(
        &mut self,
        group_id: &JsString,
        user_id: &JsString,
        text: &JsString,
    ) -> MappedSession {
        let chat_id = concat_units(&[group_id.units(), &[58], user_id.units()]);
        let text = trim_units(text.units());
        let command_new = text == [47, 110, 101, 119] || text.starts_with(&[47, 110, 101, 119, 32]);
        if command_new {
            let key = concat_units(&[
                &"qq:group=".encode_utf16().collect::<Vec<_>>(),
                group_id.units(),
                &":user=".encode_utf16().collect::<Vec<_>>(),
                user_id.units(),
                &[58, 115, 95],
                (self.uuid)().units(),
            ]);
            self.state
                .active_by_chat_key
                .insert(chat_id.clone(), JsValue::String(key.clone()));
            return MappedSession {
                session_key: key,
                command_new,
                message: JsString::from_utf16(trim_units(&text[4..]).to_vec()),
            };
        }
        MappedSession {
            session_key: self
                .state
                .active_by_chat_key
                .get_key(&chat_id)
                .and_then(JsValue::as_string)
                .cloned()
                .unwrap_or_else(|| {
                    concat_units(&[
                        &"qq:group=".encode_utf16().collect::<Vec<_>>(),
                        group_id.units(),
                        &":user=".encode_utf16().collect::<Vec<_>>(),
                        user_id.units(),
                        &":general".encode_utf16().collect::<Vec<_>>(),
                    ])
                }),
            command_new,
            message: JsString::from_utf16(text.to_vec()),
        }
    }

    pub fn snapshot(&self) -> QQSessionMapperState {
        self.state.clone()
    }
}

impl QQSessionMapperState {
    /// Preserve string keys as UTF-16 units and the persisted JS property order.
    pub fn from_js(value: &JsValue) -> Self {
        Self {
            active_by_chat_key: value
                .get("activeByChatKey")
                .and_then(JsValue::as_object)
                .cloned()
                .unwrap_or_default(),
        }
    }
    pub fn to_js(&self) -> JsValue {
        let mut result = JsObject::new();
        result.insert(
            "activeByChatKey".into(),
            JsValue::Object(self.active_by_chat_key.clone()),
        );
        JsValue::Object(result)
    }
}
