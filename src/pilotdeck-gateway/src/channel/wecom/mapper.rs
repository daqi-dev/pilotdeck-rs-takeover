use crate::channel::sms::{concat_units, trim_units};
use pilotdeck_protocol::JsString;
use std::{collections::HashMap, sync::Arc};
#[derive(Clone, Default)]
pub struct WeComSessionMapperState {
    pub active_by_chat_id: HashMap<JsString, JsString>,
    pub project_by_scope_key: HashMap<JsString, JsString>,
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
        let project_key = self.state.project_by_scope_key.get(&scope).cloned();
        if command_new {
            let key = concat_units(&[scope.units(), &[58, 115, 95], (self.uuid)().units()]);
            self.state.active_by_chat_id.insert(scope, key.clone());
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
                .get(&scope)
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
            .insert(Self::scope_key(input), project);
    }
    pub fn get_project(&self, input: &Scope<'_>) -> Option<JsString> {
        self.state
            .project_by_scope_key
            .get(&Self::scope_key(input))
            .cloned()
    }
    pub fn snapshot(&self) -> WeComSessionMapperState {
        self.state.clone()
    }
}
