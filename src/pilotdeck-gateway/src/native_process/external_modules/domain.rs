//! Runtime skills and generic knowledge adapters for the fixed cc7c4e6
//! `composition/domainPorts.ts:37-72,107-191`. Skill management remains in
//! `crate::skills`; these ports own the independent runtime request sequences.
use super::http::{HttpModuleClient, HttpModuleError, ModuleCallInput};
use crate::{
    host::{HostError, modules::capability::ToolDefinition},
    runtime::{
        network::Cancellation,
        tools::{ExecutableTool, ToolError, ToolFuture, ToolOutput, ToolRunContext},
    },
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// External paths are protocol strings, never local filesystem paths. Retain
/// UTF-16 and optional fields instead of projecting into native plugin types.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ExternalSkillContribution {
    pub name: JsString,
    pub path: JsString,
    pub description: Option<JsString>,
    pub content: Option<JsString>,
    pub namespace: Option<JsString>,
}

pub(crate) struct ExternalSkillModule {
    client: Arc<HttpModuleClient>,
    sequence: AtomicU64,
}

impl ExternalSkillModule {
    pub fn new(client: Arc<HttpModuleClient>) -> Self {
        Self {
            client,
            sequence: AtomicU64::new(0),
        }
    }

    /// Input is `{ projectKey?: string | null }`, unchanged on the wire.
    pub async fn list(
        &self,
        input: JsValue,
        abort: Option<Cancellation>,
    ) -> Result<Vec<ExternalSkillContribution>, HttpModuleError> {
        let value = call(
            &self.client,
            &self.sequence,
            "skills",
            "Skill",
            "list",
            input,
            abort,
        )
        .await?;
        let entries = match &value {
            JsValue::Array(entries) => Some(entries),
            JsValue::Object(object) => object.get("items").and_then(|items| match items {
                JsValue::Array(items) => Some(items),
                _ => None,
            }),
            _ => None,
        }
        .ok_or_else(|| {
            HttpModuleError::protocol(
                "Skill list result must be an array or a management list result.",
            )
        })?;
        entries.iter().map(skill_contribution).collect()
    }

    /// Input is `{ name: string, projectKey?: string | null }`. In particular,
    /// missing remote content never falls back to local plugin files.
    pub async fn read(
        &self,
        input: JsValue,
        abort: Option<Cancellation>,
    ) -> Result<Option<JsString>, HttpModuleError> {
        let value = call(
            &self.client,
            &self.sequence,
            "skills",
            "Skill",
            "read",
            input,
            abort,
        )
        .await?;
        match &value {
            JsValue::Null | JsValue::Undefined => Ok(None),
            JsValue::String(content) => Ok(Some(content.clone())),
            JsValue::Object(object) => object
                .get("content")
                .and_then(JsValue::as_string)
                .cloned()
                .map(Some)
                .ok_or_else(|| {
                    HttpModuleError::protocol(
                        "Skill read result must contain string content or be null.",
                    )
                }),
            _ => Err(HttpModuleError::protocol(
                "Skill read result must contain string content or be null.",
            )),
        }
    }
}

pub(crate) struct ExternalKnowledgeModule {
    client: Arc<HttpModuleClient>,
    sequence: AtomicU64,
}

impl ExternalKnowledgeModule {
    pub fn new(client: Arc<HttpModuleClient>) -> Self {
        Self {
            client,
            sequence: AtomicU64::new(0),
        }
    }

    /// Generic knowledge has no mandatory credentials or StaffDeck authority.
    pub async fn call(
        &self,
        operation: &str,
        input: JsValue,
        abort: Option<Cancellation>,
    ) -> Result<JsValue, HttpModuleError> {
        call(
            &self.client,
            &self.sequence,
            "knowledge",
            "Knowledge",
            operation,
            input,
            abort,
        )
        .await
    }
}

async fn call(
    client: &HttpModuleClient,
    sequence: &AtomicU64,
    module: &str,
    owner: &str,
    operation: &str,
    input: JsValue,
    abort: Option<Cancellation>,
) -> Result<JsValue, HttpModuleError> {
    // The declared-method guard precedes sequence allocation and HTTP access.
    if !client.binding().supports(operation) {
        return Err(HttpModuleError::protocol(format!(
            "{owner} operation '{operation}' is not declared."
        )));
    }
    let sequence = sequence.fetch_add(1, Ordering::Relaxed) + 1;
    let response = client
        .call(ModuleCallInput {
            run_id: module.into(),
            operation_id: module.into(),
            request_id: format!("{module}-{operation}-{sequence}").into(),
            module: module.into(),
            payload: object([("operation", operation.into()), ("input", input)]),
            idempotency_key: None,
            abort,
        })
        .await?;
    if response.get("ok") != Some(&JsValue::Bool(true)) {
        return Err(HttpModuleError::from_response(&response));
    }
    Ok(response
        .get("payload")
        .and_then(|payload| payload.get("result"))
        .cloned()
        .unwrap_or(JsValue::Undefined))
}

fn skill_contribution(value: &JsValue) -> Result<ExternalSkillContribution, HttpModuleError> {
    let invalid = || HttpModuleError::protocol("Skill list item requires name and path.");
    let item = value.as_object().ok_or_else(invalid)?;
    let text = |key: &str| item.get(key).and_then(JsValue::as_string);
    let name = text("slug").or_else(|| text("name")).ok_or_else(invalid)?;
    let path = text("path")
        .or_else(|| text("skillFile"))
        .ok_or_else(invalid)?;
    Ok(ExternalSkillContribution {
        name: name.clone(),
        path: path.clone(),
        description: text("description").cloned(),
        content: text("content").cloned(),
        namespace: text("namespace").cloned(),
    })
}

/// A session-owned replacement for `read_skill`. The catalog supplies prompt
/// contributions and the lister; the loader always calls the selected module.
/// SDK filtering applies to the catalog, as in ProjectSessionRuntimeBundle,
/// rather than silently redirecting an unlisted name to local files.
pub(crate) struct ExternalReadSkillTool {
    fields: JsObject,
    port: Arc<ExternalSkillModule>,
    project_key: Option<JsString>,
    skills: Vec<ExternalSkillContribution>,
}

impl ExternalReadSkillTool {
    pub fn new(
        port: Arc<ExternalSkillModule>,
        project_key: Option<JsString>,
        skills: Vec<ExternalSkillContribution>,
    ) -> Self {
        Self {
            fields: fields([
                ("name", "read_skill".into()),
                ("aliases", JsValue::Array(vec!["ReadSkill".into()])),
                ("description", "Load a skill recipe by name and return its resolved SKILL.md path with the full content. Use this when the system prompt lists an available skill relevant to the current task.".into()),
                ("kind", "session".into()),
                ("inputSchema", pilotdeck_protocol::parse_json(r#"{"type":"object","required":["skillName"],"additionalProperties":false,"properties":{"skillName":{"type":"string","description":"The skill name as listed in <available-skills>."}}}"#).expect("static skill schema")),
            ]),
            port, project_key, skills,
        }
    }
}

impl ToolDefinition for ExternalReadSkillTool {
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

impl ExecutableTool for ExternalReadSkillTool {
    fn execute(&self, input: JsValue, context: ToolRunContext) -> ToolFuture<ToolOutput> {
        let port = self.port.clone();
        let project_key = self.project_key.clone();
        let skills = self.skills.clone();
        Box::pin(async move {
            let name = input
                .get("skillName")
                .cloned()
                .unwrap_or(JsValue::Undefined);
            let mut input = fields([("name", name.clone())]);
            if let Some(project_key) = project_key {
                input.insert("projectKey".into(), JsValue::String(project_key));
            }
            let content = port
                .read(JsValue::Object(input), context.cancellation().cloned())
                .await
                .map_err(tool_error)?;
            let name = name
                .as_string()
                .cloned()
                .unwrap_or_else(|| "undefined".into());
            let text = match content.filter(|content| !content.is_empty()) {
                Some(content) => match skills.iter().find(|skill| skill.name == name) {
                    Some(skill) => concatenate([
                        "<skill>\n<name>".into(),
                        escape_xml(&skill.name),
                        "</name>\n<path>".into(),
                        escape_xml(&skill.path),
                        "</path>\n".into(),
                        content,
                        "\n</skill>".into(),
                    ]),
                    None => content,
                },
                None if skills.is_empty() => concatenate([
                    "Skill '".into(),
                    name,
                    "' not found. No skills are currently loaded.".into(),
                ]),
                None => {
                    let mut names = Vec::new();
                    for (index, skill) in skills.iter().enumerate() {
                        if index != 0 {
                            names.push(", ".into());
                        }
                        names.push(skill.name.clone());
                    }
                    concatenate([
                        "Skill '".into(),
                        name,
                        "' not found. Available skills: ".into(),
                        concatenate(names),
                    ])
                }
            };
            Ok(ToolOutput {
                content: vec![object([
                    ("type", "text".into()),
                    ("text", JsValue::String(text)),
                ])],
                ..ToolOutput::default()
            })
        })
    }
}

pub(crate) struct KnowledgeQueryTool {
    fields: JsObject,
    port: Arc<ExternalKnowledgeModule>,
    scope: JsObject,
    binding_has_read_credential: bool,
}

impl KnowledgeQueryTool {
    pub fn new(port: Arc<ExternalKnowledgeModule>, binding: &JsValue) -> Self {
        let mut scope = JsObject::new();
        for name in ["tenantId", "actorUserId", "agentId"] {
            if let Some(value) = binding.get(name).filter(|value| truthy(value)) {
                scope.insert(name.into(), value.clone());
            }
        }
        Self {
            fields: fields([
                ("name", "knowledge_query".into()),
                ("description", "Query the configured knowledge module for grounded evidence and citations. The host supplies tenant and agent scope; provide a natural-language query and optionally select knowledge_base_ids.".into()),
                ("kind", "custom".into()),
                ("inputSchema", pilotdeck_protocol::parse_json(r#"{"type":"object","required":["query"],"additionalProperties":true,"properties":{"query":{"type":"string"}}}"#).expect("static knowledge schema")),
            ]),
            port, scope,
            binding_has_read_credential: binding.get("credentialEnv").and_then(JsValue::as_string).is_some(),
        }
    }
}

impl ToolDefinition for KnowledgeQueryTool {
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

impl ExecutableTool for KnowledgeQueryTool {
    fn execute(&self, input: JsValue, context: ToolRunContext) -> ToolFuture<ToolOutput> {
        let port = self.port.clone();
        let scope = self.scope.clone();
        let credential = self.binding_has_read_credential;
        Box::pin(async move {
            let mut model_input = input.as_object().cloned().unwrap_or_default();
            if credential
                && model_input.own_entries().into_iter().any(|(name, _)| {
                    [
                        "tenantId",
                        "tenant_id",
                        "actorUserId",
                        "actor_user_id",
                        "agentId",
                        "agent_id",
                        "modelConfigId",
                        "model_config_id",
                    ]
                    .iter()
                    .any(|key| name.equals_str(key))
                })
            {
                return Err(tool_error(HttpModuleError::protocol(
                    "Knowledge query cannot supply identity or an SD model selection.",
                )));
            }
            if !credential {
                for (name, value) in scope.own_entries() {
                    model_input.insert(name.clone(), value.clone());
                }
            }
            let result = port
                .call(
                    "query",
                    JsValue::Object(model_input),
                    context.cancellation().cloned(),
                )
                .await
                .map_err(tool_error)?;
            Ok(ToolOutput {
                content: vec![object([("type", "json".into()), ("value", result.clone())])],
                data: Some(result),
                ..ToolOutput::default()
            })
        })
    }
}

// domainPorts throws ordinary Error objects. ToolRuntime's normalizeToolError
// intentionally exposes their message as tool_execution_failed; direct port
// consumers retain all structured HttpModuleError fields.
fn tool_error(error: HttpModuleError) -> ToolError {
    ToolError::failed(error.message)
}
fn concatenate(parts: impl IntoIterator<Item = JsString>) -> JsString {
    let mut units = Vec::new();
    for part in parts {
        units.extend_from_slice(part.units());
    }
    JsString::from_utf16(units)
}

fn escape_xml(value: &JsString) -> JsString {
    let mut units = Vec::new();
    for unit in value.units() {
        match unit {
            0x26 => units.extend("&amp;".encode_utf16()),
            0x3c => units.extend("&lt;".encode_utf16()),
            0x3e => units.extend("&gt;".encode_utf16()),
            other => units.push(*other),
        }
    }
    JsString::from_utf16(units)
}
fn truthy(value: &JsValue) -> bool {
    match value {
        JsValue::Undefined | JsValue::Null => false,
        JsValue::Bool(v) => *v,
        JsValue::String(v) => !v.is_empty(),
        JsValue::Number(v) => *v != 0.0 && !v.is_nan(),
        _ => true,
    }
}
fn fields<const N: usize>(entries: [(&str, JsValue); N]) -> JsObject {
    let mut fields = JsObject::new();
    for (name, value) in entries {
        fields.insert(name.into(), value);
    }
    fields
}
fn object<const N: usize>(entries: [(&str, JsValue); N]) -> JsValue {
    JsValue::Object(fields(entries))
}

#[cfg(test)]
#[path = "domain_tests.rs"]
mod tests;
