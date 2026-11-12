use crate::channel::sms::{concat_units, trim_units};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::sync::Arc;
#[derive(Clone, Default)]
pub struct WeComSessionMapperState {
    pub active_by_chat_id: JsObject,
    pub project_by_scope_key: JsObject,
}
impl WeComSessionMapperState {
    /// State persistence has no schema validation in the fixed baseline.
    /// Keep every property of present records, including values not read as
    /// session/project strings, so a snapshot never silently deletes data.
    pub fn from_js(value: &JsValue) -> Self {
        Self {
            active_by_chat_id: value
                .get("activeByChatId")
                .and_then(JsValue::as_object)
                .cloned()
                .unwrap_or_default(),
            project_by_scope_key: value
                .get("projectByScopeKey")
                .and_then(JsValue::as_object)
                .cloned()
                .unwrap_or_default(),
        }
    }
    pub fn to_js(&self) -> JsValue {
        let mut value = JsObject::new();
        value.insert(
            "activeByChatId".into(),
            JsValue::Object(self.active_by_chat_id.clone()),
        );
        value.insert(
            "projectByScopeKey".into(),
            JsValue::Object(self.project_by_scope_key.clone()),
        );
        JsValue::Object(value)
    }
}
#[derive(Clone)]
pub struct WeComSessionMapper {
    state: WeComSessionMapperState,
    uuid: Arc<dyn Fn() -> JsString + Send + Sync>,
}
pub struct Scope<'a> {
    pub chat_id: &'a JsString,
    pub user_id: Option<&'a JsString>,
    pub group: bool,
    pub group_sessions_per_user: bool,
}
pub struct MappedSession {
    pub session_key: JsString,
    pub message: JsString,
    pub command_new: bool,
    pub project_key: Option<JsString>,
}
impl Default for WeComSessionMapper {
    fn default() -> Self {
        Self::new(
            Default::default(),
            Arc::new(|| uuid::Uuid::new_v4().to_string().into()),
        )
    }
}
impl WeComSessionMapper {
    pub fn new(
        state: WeComSessionMapperState,
        uuid: Arc<dyn Fn() -> JsString + Send + Sync>,
    ) -> Self {
        Self { state, uuid }
    }
    pub fn scope_key(input: &Scope<'_>) -> JsString {
        let chat = trim_units(input.chat_id.units());
        let user = input
            .user_id
            .map(|v| trim_units(v.units()))
            .unwrap_or_default();
        if input.group {
            if input.group_sessions_per_user && !user.is_empty() {
                concat_units(&[
                    &"wecom:group=".encode_utf16().collect::<Vec<_>>(),
                    chat,
                    &":user=".encode_utf16().collect::<Vec<_>>(),
                    user,
                ])
            } else {
                concat_units(&[&"wecom:group=".encode_utf16().collect::<Vec<_>>(), chat])
            }
        } else {
            concat_units(&[
                &"wecom:dm=".encode_utf16().collect::<Vec<_>>(),
                if user.is_empty() { chat } else { user },
            ])
        }
    }
    pub fn resolve(&mut self, input: &Scope<'_>, text: &JsString) -> MappedSession {
        let scope = Self::scope_key(input);
        let text = trim_units(text.units());
        let command_new = text == [47, 110, 101, 119] || text.starts_with(&[47, 110, 101, 119, 32]);
        let project_key = self
            .state
            .project_by_scope_key
            .get_key(&scope)
            .and_then(JsValue::as_string)
            .cloned();
        if command_new {
            let key = concat_units(&[scope.units(), &[58, 115, 95], (self.uuid)().units()]);
            self.state
                .active_by_chat_id
                .insert(scope, JsValue::String(key.clone()));
            return MappedSession {
                session_key: key,
                message: JsString::from_utf16(trim_units(&text[4..]).to_vec()),
                command_new,
                project_key,
            };
        }
        MappedSession {
            session_key: self
                .state
                .active_by_chat_id
                .get_key(&scope)
                .and_then(JsValue::as_string)
                .cloned()
                .unwrap_or_else(|| {
                    concat_units(&[
                        scope.units(),
                        &":general".encode_utf16().collect::<Vec<_>>(),
                    ])
                }),
            message: JsString::from_utf16(text.to_vec()),
            command_new,
            project_key,
        }
    }
    pub fn bind_project(&mut self, input: &Scope<'_>, project: JsString) {
        self.state
            .project_by_scope_key
            .insert(Self::scope_key(input), JsValue::String(project));
    }
    pub fn get_project(&self, input: &Scope<'_>) -> Option<JsString> {
        self.state
            .project_by_scope_key
            .get_key(&Self::scope_key(input))
            .and_then(JsValue::as_string)
            .cloned()
    }
    pub fn snapshot(&self) -> WeComSessionMapperState {
        self.state.clone()
    }
}
