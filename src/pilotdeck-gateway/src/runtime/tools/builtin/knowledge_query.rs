//! `knowledge_query` (baseline `composition/domainPorts.ts:130-169`,
//! `createKnowledgeQueryTool`): registered into a session only when
//! `modules.knowledge` is bound to an external module
//! (ProjectSessionRuntimeBundle.ts:316-326). The query goes through the
//! knowledge module port (`createKnowledgeModulePort`, the HTTP module
//! client); its failures are plain errors, so the runtime reports them as
//! `tool_execution_failed`.
use super::{object, string, tool_error};
use crate::{
    host::{HostError, modules::capability::ToolDefinition},
    runtime::tools::{ExecutableTool, ToolFuture, ToolOutput, ToolRunContext},
    skills::{ExternalSkillBinding, ExternalSkillsPort, HyperModuleTransport},
};
use pilotdeck_protocol::{JsObject, JsValue};
use std::sync::Arc;

/// The model input keys an authenticated (`credentialEnv`) binding refuses.
const IDENTITY_KEYS: [&str; 8] = [
    "tenantId",
    "tenant_id",
    "actorUserId",
    "actor_user_id",
    "agentId",
    "agent_id",
    "modelConfigId",
    "model_config_id",
];

pub struct KnowledgeQueryTool {
    fields: JsObject,
    port: Arc<ExternalSkillsPort<HyperModuleTransport>>,
    /// `tenantId` / `actorUserId` / `agentId` of the binding, in that order.
    scope: Vec<(&'static str, String)>,
    /// `"credentialEnv" in scope && typeof scope.credentialEnv === "string"`.
    has_read_credential: bool,
}

impl KnowledgeQueryTool {
    /// `createKnowledgeQueryTool(createKnowledgeModulePort(binding), binding)`.
    /// `binding` is the resolved `snapshot.config.modules.knowledge`.
    pub fn from_binding(binding: &JsValue) -> Option<Self> {
        let parsed = ExternalSkillBinding::from_value(binding).ok()?;
        let has_read_credential = parsed.credential_env.is_some();
        let text = |key: &str| {
            binding
                .get(key)
                .and_then(JsValue::as_string)
                .filter(|value| !value.is_empty())
                .map(|value| String::from_utf16_lossy(value.units()))
        };
        let scope = ["tenantId", "actorUserId", "agentId"]
            .into_iter()
            .filter_map(|key| text(key).map(|value| (key, value)))
            .collect();
        Some(Self {
            fields: object([
                ("name", string("knowledge_query")),
                (
                    "description",
                    string(
                        "Query the configured knowledge module for grounded evidence and citations. The host supplies tenant and agent scope; provide a natural-language query and optionally select knowledge_base_ids.",
                    ),
                ),
                ("kind", string("custom")),
                (
                    "inputSchema",
                    JsValue::Object(super::schema(
                        r#"{"type":"object","required":["query"],"additionalProperties":true,"properties":{"query":{"type":"string"}}}"#,
                    )),
                ),
            ]),
            port: Arc::new(ExternalSkillsPort::knowledge(
                parsed,
                Arc::new(HyperModuleTransport::new()),
            )),
            scope,
            has_read_credential,
        })
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
    fn execute(&self, input: JsValue, _context: ToolRunContext) -> ToolFuture<ToolOutput> {
        let port = self.port.clone();
        let scope = self.scope.clone();
        let has_read_credential = self.has_read_credential;
        Box::pin(async move {
            let mut model_input = input.as_object().cloned().unwrap_or_default();
            if has_read_credential
                && model_input
                    .own_entries()
                    .iter()
                    .any(|(key, _)| IDENTITY_KEYS.iter().any(|name| key.equals_str(name)))
            {
                return Err(tool_error(
                    "tool_execution_failed",
                    "Knowledge query cannot supply identity or an SD model selection.",
                ));
            }
            if !has_read_credential {
                for (key, value) in scope {
                    model_input.insert(key.into(), JsValue::from(value.as_str()));
                }
            }
            let result = port
                .knowledge_call("query", &JsValue::Object(model_input))
                .await
                .map_err(|error| tool_error("tool_execution_failed", error.message))?;
            Ok(ToolOutput {
                content: vec![JsValue::Object(object([
                    ("type", string("json")),
                    ("value", result.clone()),
                ]))],
                supplemental_messages: None,
                data: Some(result),
                metadata: None,
            })
        })
    }
}
