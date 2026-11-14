//! `read_skill` (baseline `tool/builtin/readSkill.ts`): loads a skill recipe
//! by name from the project's plugin runtime, which is both the `loader`
//! (`loadSkillPrompt`) and the `lister` (`getAllSkills`) the baseline wires
//! (`ProjectRuntimeResourcesBundle.ts:265`).
use super::{field, object, schema, string, text, tool_error};
use crate::{
    host::{HostError, modules::capability::ToolDefinition},
    runtime::{
        plugins::{LoadedPlugin, PluginRuntime},
        tools::{ExecutableTool, ToolFuture, ToolOutput, ToolRunContext},
    },
};
use pilotdeck_protocol::{JsObject, JsValue};
use std::sync::Arc;

/// `createSkillModulePort(binding)` (composition/domainPorts.ts:37-62): an
/// external `modules.skills` binding as the session's skill source. One per
/// session, like the baseline's per-session port (its HTTP message and request
/// counters start over with each session).
pub struct ExternalSkillSource {
    port: crate::skills::ExternalSkillsPort<crate::skills::HyperModuleTransport>,
    project_key: pilotdeck_protocol::JsString,
}

/// One `readSkillContribution` entry of the module's `list`.
#[derive(Clone, Debug)]
pub struct ExternalSkill {
    pub name: String,
    pub path: String,
    pub description: Option<String>,
}

impl ExternalSkillSource {
    /// `createSkillModulePort(binding)` for an external binding
    /// (`isExternalModuleBinding`); `None` otherwise.
    pub fn from_binding(binding: &JsValue, project_root: &std::path::Path) -> Option<Self> {
        if !binding
            .as_object()
            .is_some_and(|binding| binding.contains_key("implementationId"))
        {
            return None;
        }
        let parsed = crate::skills::ExternalSkillBinding::from_value(binding).ok()?;
        Some(Self {
            port: crate::skills::ExternalSkillsPort::skills_runtime(
                parsed,
                Arc::new(crate::skills::HyperModuleTransport::new()),
            ),
            project_key: project_root.to_string_lossy().as_ref().into(),
        })
    }

    fn protocol(message: &str) -> crate::skills::SkillError {
        crate::skills::SkillError::protocol_failure(message.to_owned())
    }

    /// `list({ projectKey })`: an array, or a management list result's
    /// `items`, each read by `readSkillContribution`.
    pub async fn list(&self) -> Result<Vec<ExternalSkill>, crate::skills::SkillError> {
        let input = JsValue::Object(object([(
            "projectKey",
            JsValue::String(self.project_key.clone()),
        )]));
        let value = self.port.skill_runtime_call("list", &input).await?;
        let not_a_list =
            || Self::protocol("Skill list result must be an array or a management list result.");
        let items = match &value {
            JsValue::Array(items) => items.clone(),
            JsValue::Object(record) => match record.get("items") {
                Some(JsValue::Array(items)) => items.clone(),
                _ => return Err(not_a_list()),
            },
            _ => return Err(not_a_list()),
        };
        let text_of = |value: Option<&JsValue>| value.and_then(JsValue::as_string).map(text);
        items
            .iter()
            .map(|item| {
                let Some(record) = item.as_object() else {
                    return Err(Self::protocol("Skill list item requires name and path."));
                };
                // `typeof slug === "string" ? slug : name`, and `path` before
                // `skillFile`.
                let name = match record.get("slug") {
                    Some(JsValue::String(slug)) => Some(text(slug)),
                    _ => text_of(record.get("name")),
                };
                let path = match record.get("path") {
                    Some(JsValue::String(path)) => Some(text(path)),
                    _ => text_of(record.get("skillFile")),
                };
                let (Some(name), Some(path)) = (name, path) else {
                    return Err(Self::protocol("Skill list item requires name and path."));
                };
                Ok(ExternalSkill {
                    name,
                    path,
                    description: text_of(record.get("description")),
                })
            })
            .collect()
    }

    /// `read({ name, projectKey })`: `null` / `undefined`, a string, or a
    /// record with string `content`.
    pub async fn read(&self, name: &str) -> Result<Option<String>, crate::skills::SkillError> {
        let input = JsValue::Object(object([
            ("name", string(name)),
            ("projectKey", JsValue::String(self.project_key.clone())),
        ]));
        let not_content =
            || Self::protocol("Skill read result must contain string content or be null.");
        match &self.port.skill_runtime_call("read", &input).await? {
            JsValue::Null | JsValue::Undefined => Ok(None),
            JsValue::String(content) => Ok(Some(text(content))),
            JsValue::Object(record) => match record.get("content") {
                Some(JsValue::String(content)) => Ok(Some(text(content))),
                _ => Err(not_content()),
            },
            _ => Err(not_content()),
        }
    }
}

pub struct ReadSkillTool {
    fields: JsObject,
    plugins: Arc<PluginRuntime>,
    /// An SDK skill-scoped session's names (ProjectSessionRuntimeBundle.ts:
    /// 302-316): the loader answers only an exact selected name and the
    /// lister lists only the selected skills.
    selected: Option<Vec<String>>,
    /// `sdkSessionPlugins`: an SDK skill-scoped session's replacement
    /// `read_skill` reads the session contributions, which add these to the
    /// project's plugins (ProjectSessionRuntimeBundle.ts:247-248, 302-316).
    /// The project-level tool of an unscoped session never sees them.
    session_plugins: Vec<Arc<LoadedPlugin>>,
    /// An external `modules.skills` binding: the replacement `read_skill`
    /// loads through the module and lists the session's (filtered) module
    /// skills as `(name, path)` (ProjectSessionRuntimeBundle.ts:302-316).
    external: Option<ExternalReadSkill>,
}

/// The module source and the lister's `(name, path)` pairs.
pub type ExternalReadSkill = (Arc<ExternalSkillSource>, Vec<(String, String)>);

impl ReadSkillTool {
    pub fn new(plugins: Arc<PluginRuntime>) -> Self {
        Self {
            plugins,
            selected: None,
            session_plugins: Vec::new(),
            external: None,
            fields: object([
                ("name", string("read_skill")),
                ("aliases", JsValue::Array(vec![string("ReadSkill")])),
                (
                    "description",
                    string(
                        "Load a skill recipe by name and return its resolved SKILL.md path with the full content. Use this when the system prompt lists an available skill relevant to the current task.",
                    ),
                ),
                ("kind", string("session")),
                (
                    "inputSchema",
                    JsValue::Object(schema(
                        r#"{"type":"object","required":["skillName"],"additionalProperties":false,"properties":{"skillName":{"type":"string","description":"The skill name as listed in <available-skills>."}}}"#,
                    )),
                ),
            ]),
        }
    }
}

impl ReadSkillTool {
    pub fn with_selected(mut self, selected: Option<Vec<String>>) -> Self {
        self.selected = selected;
        self
    }

    pub fn with_session_plugins(mut self, plugins: Vec<Arc<LoadedPlugin>>) -> Self {
        self.session_plugins = plugins;
        self
    }

    pub fn with_external(mut self, external: Option<ExternalReadSkill>) -> Self {
        self.external = external;
        self
    }
}

impl ToolDefinition for ReadSkillTool {
    fn fields(&self) -> &JsObject {
        &self.fields
    }
    fn is_read_only(&self, _input: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(true))
    }
    fn is_concurrency_safe(&self, _input: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(true))
    }
}

/// `escapeXmlText`.
fn escape_xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The tool's reply for one lookup: `content` is the loader's result, `available`
/// the lister's `(name, path)` pairs.
fn render(name: &str, content: Option<String>, available: &[(String, String)]) -> String {
    if let Some(content) = content.filter(|content| !content.is_empty()) {
        return match available.iter().find(|(known, _)| known == name) {
            None => content,
            Some((known, path)) => [
                "<skill>".to_owned(),
                format!("<name>{}</name>", escape_xml_text(known)),
                format!("<path>{}</path>", escape_xml_text(path)),
                content,
                "</skill>".to_owned(),
            ]
            .join("\n"),
        };
    }
    if available.is_empty() {
        return format!("Skill '{name}' not found. No skills are currently loaded.");
    }
    let names: Vec<&str> = available.iter().map(|(name, _)| name.as_str()).collect();
    format!(
        "Skill '{name}' not found. Available skills: {}",
        names.join(", ")
    )
}

impl ExecutableTool for ReadSkillTool {
    fn execute(&self, input: JsValue, _context: ToolRunContext) -> ToolFuture<ToolOutput> {
        let plugins = self.plugins.clone();
        let selected = self.selected.clone();
        let session_plugins = self.session_plugins.clone();
        let external = self.external.clone();
        Box::pin(async move {
            let name = match field(&input, "skillName") {
                JsValue::String(name) => text(name),
                other => crate::js_value::display(other).unwrap_or_default(),
            };
            let chosen = |skill: &str| {
                selected
                    .as_ref()
                    .is_none_or(|names| names.iter().any(|name| name == skill))
            };
            if let Some((source, available)) = external {
                let content = source
                    .read(&name)
                    .await
                    .map_err(|error| tool_error("tool_execution_failed", error.message))?;
                return Ok(ToolOutput {
                    content: vec![JsValue::Object(object([
                        ("type", string("text")),
                        ("text", string(render(&name, content, &available))),
                    ]))],
                    ..ToolOutput::default()
                });
            }
            // The scoped replacement reads the session's contributions; the
            // project-level tool reads the project runtime alone.
            let view = plugins
                .create_view(if selected.is_some() {
                    session_plugins
                } else {
                    Vec::new()
                })
                .await;
            let content = if chosen(&name) {
                view.load_skill_prompt(&name)
            } else {
                None
            };
            let available: Vec<(String, String)> = view
                .all_skills()
                .into_iter()
                .filter(|skill| chosen(&skill.name))
                .map(|skill| (skill.name, skill.path.to_string_lossy().into_owned()))
                .collect();
            Ok(ToolOutput {
                content: vec![JsValue::Object(object([
                    ("type", string("text")),
                    ("text", string(render(&name, content, &available))),
                ]))],
                ..ToolOutput::default()
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn found_skill_is_wrapped_with_its_escaped_name_and_path() {
        let available = vec![("a<b>&c".to_owned(), "/skills/a&b/SKILL.md".to_owned())];
        assert_eq!(
            render("a<b>&c", Some("body <raw>".into()), &available),
            "<skill>\n<name>a&lt;b&gt;&amp;c</name>\n<path>/skills/a&amp;b/SKILL.md</path>\nbody <raw>\n</skill>"
        );
        // Loaded by short name: the lister has no exact entry, so no wrapper.
        assert_eq!(render("c", Some("body".into()), &available), "body");
    }

    #[test]
    fn missing_skill_lists_what_is_loaded() {
        assert_eq!(
            render("x", None, &[]),
            "Skill 'x' not found. No skills are currently loaded."
        );
        assert_eq!(
            render(
                "x",
                Some(String::new()),
                &[("a".into(), "p".into()), ("b".into(), "q".into())]
            ),
            "Skill 'x' not found. Available skills: a, b"
        );
    }
}
