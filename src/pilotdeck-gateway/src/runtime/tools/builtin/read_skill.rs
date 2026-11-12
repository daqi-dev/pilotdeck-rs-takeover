//! `read_skill` (baseline `tool/builtin/readSkill.ts`): loads a skill recipe
//! by name from the project's plugin runtime, which is both the `loader`
//! (`loadSkillPrompt`) and the `lister` (`getAllSkills`) the baseline wires
//! (`ProjectRuntimeResourcesBundle.ts:265`).
use super::{field, object, schema, string, text};
use crate::{
    host::{HostError, modules::capability::ToolDefinition},
    runtime::{
        plugins::PluginRuntime,
        tools::{ExecutableTool, ToolFuture, ToolOutput, ToolRunContext},
    },
};
use pilotdeck_protocol::{JsObject, JsValue};
use std::sync::Arc;

pub struct ReadSkillTool {
    fields: JsObject,
    plugins: Arc<PluginRuntime>,
    /// An SDK skill-scoped session's names (ProjectSessionRuntimeBundle.ts:
    /// 302-316): the loader answers only an exact selected name and the
    /// lister lists only the selected skills.
    selected: Option<Vec<String>>,
}

impl ReadSkillTool {
    pub fn new(plugins: Arc<PluginRuntime>) -> Self {
        Self {
            plugins,
            selected: None,
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
            let content = if chosen(&name) {
                plugins.load_skill_prompt(&name).await
            } else {
                None
            };
            let available: Vec<(String, String)> = plugins
                .all_skills()
                .await
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
