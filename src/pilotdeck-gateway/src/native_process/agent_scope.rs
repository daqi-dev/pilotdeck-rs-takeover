//! Serializable AgentDefinition scope; no JS callback/function loading.
use super::*;
use crate::runtime::mcp::{
    bridge::{
        CreateToolDefinitionsOptions, McpBridgeTool, create_mcp_tool_definitions_from_runtime,
    },
    protocol::{McpServerSpec, McpTransportSpec},
    registry::ProjectMcpRuntimeProvider,
    runtime::McpRuntimePort,
};
use crate::runtime::turn::submit::SubmitError;
use std::sync::Mutex;

/// Fixed ProjectRuntimeRegistry.resolveSdkSubagentMcpServers: references are
/// copied at config admission; toggling the parent table later cannot mutate
/// an already admitted fork definition.
pub(crate) fn resolve_sdk_agent_mcp_agents(
    agents: &mut JsObject,
    session_servers: &[(JsString, JsValue)],
) -> Result<(), SubmitError> {
    let keys = agents
        .own_entries()
        .into_iter()
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    for agent_id in keys {
        let Some(mut agent) = agents
            .get_key(&agent_id)
            .and_then(JsValue::as_object)
            .cloned()
        else {
            continue;
        };
        let Some(specs) = agent.get("mcpServers").filter(|v| !v.is_undefined()) else {
            continue;
        };
        let entries = if let JsValue::Array(entries) = specs {
            entries.clone()
        } else {
            vec![specs.clone()]
        };
        let mut resolved = JsObject::new();
        for entry in entries {
            if let Some(reference) = entry.as_string() {
                let Some((_, server)) = session_servers.iter().find(|(id, _)| id == reference)
                else {
                    return Err(SubmitError::new(
                        "SDK_AGENT_MCP_REFERENCE_NOT_FOUND",
                        format!(
                            "Agent {} references SDK-session MCP server {}, but it is not configured for this session.",
                            text(&agent_id),
                            text(reference)
                        ),
                    ));
                };
                if !server.get("enabled").is_some_and(truthy) {
                    return Err(SubmitError::new(
                        "SDK_AGENT_MCP_REFERENCE_DISABLED",
                        format!(
                            "Agent {} references SDK-session MCP server {}, but it is disabled.",
                            text(&agent_id),
                            text(reference)
                        ),
                    ));
                }
                resolved.insert(
                    reference.clone(),
                    server.get("config").cloned().unwrap_or(JsValue::Undefined),
                );
            } else if let Some(spec) = entry.as_object() {
                for (name, config) in spec.own_entries() {
                    if resolved.get_key(name).is_some_and(truthy) {
                        return Err(SubmitError::new(
                            "INVALID_MCP_SERVER",
                            format!(
                                "Agent {} repeats MCP server {}.",
                                text(&agent_id),
                                text(name)
                            ),
                        ));
                    }
                    resolved.insert(name.clone(), config.clone());
                }
            }
        }
        agent.insert("mcpServers".into(), JsValue::Object(resolved));
        agents.insert(agent_id, JsValue::Object(agent));
    }
    Ok(())
}
fn server_specs(config: &JsObject) -> Vec<McpServerSpec> {
    let string = |value: Option<&JsValue>| value.and_then(JsValue::as_string).map(text);
    let record = |value: Option<&JsValue>| {
        value.and_then(JsValue::as_object).map(|object| {
            object
                .own_entries()
                .into_iter()
                .filter_map(|(key, value)| Some((text(key), text(value.as_string()?))))
                .collect::<Vec<_>>()
        })
    };
    config
        .own_entries()
        .into_iter()
        .filter_map(|(id, value)| {
            let value = value.as_object()?;
            let kind = string(value.get("type")).unwrap_or_default();
            let transport = if kind == "stdio" {
                McpTransportSpec::Stdio {
                    command: string(value.get("command"))?,
                    args: match value.get("args") {
                        Some(JsValue::Array(items)) if !items.is_empty() => Some(
                            items
                                .iter()
                                .filter_map(|v| v.as_string().map(text))
                                .collect(),
                        ),
                        _ => None,
                    },
                    env: record(value.get("env")),
                    cwd: string(value.get("cwd")).filter(|s| !s.is_empty()),
                    per_session: false,
                }
            } else if kind == "sse" {
                McpTransportSpec::Sse {
                    url: string(value.get("url"))?,
                    headers: record(value.get("headers")),
                }
            } else {
                McpTransportSpec::StreamableHttp {
                    url: string(value.get("url"))?,
                    headers: record(value.get("headers")),
                }
            };
            Some(McpServerSpec {
                id: text(id),
                transport,
                call_timeout_ms: value.get("timeout").and_then(JsValue::as_number),
            })
        })
        .collect()
}
#[derive(Clone)]
pub(super) struct ForkMcp(Arc<McpOwner>);
type McpCleanupReceipt = tokio::sync::watch::Receiver<Option<Result<(), JsString>>>;
struct McpOwner {
    keepalive: Mutex<Option<Arc<dyn Send + Sync>>>,
    runtime: Arc<dyn McpRuntimePort>,
    cleanup: Mutex<Option<McpCleanupReceipt>>,
    executor: tokio::runtime::Handle,
}
impl ForkMcp {
    pub(super) async fn start(
        provider: Arc<ProjectMcpRuntimeProvider>,
        generation: Generation,
        config: &JsObject,
        definition: &SubagentDefinition,
    ) -> Result<(Vec<Arc<McpBridgeTool>>, Self), JsString> {
        let owner = Self::prepare(provider, generation, config);
        let tools = owner.attach(definition).await?;
        Ok((tools, owner))
    }
    pub(super) fn prepare(
        provider: Arc<ProjectMcpRuntimeProvider>,
        generation: Generation,
        config: &JsObject,
    ) -> Self {
        Self::new(
            provider.create_runtime(server_specs(config)),
            Arc::new(generation),
        )
    }
    pub(super) async fn attach(
        &self,
        definition: &SubagentDefinition,
    ) -> Result<Vec<Arc<McpBridgeTool>>, JsString> {
        let started = self.0.runtime.start().await;
        let result = match started {
            Ok(_) => create_mcp_tool_definitions_from_runtime(
                self.0.runtime.clone(),
                CreateToolDefinitionsOptions::default(),
            )
            .await
            .map_err(|error| JsString::from(error.to_string())),
            Err(error) => Err(error.to_string().into()),
        };
        let tools = match result {
            Ok(tools) => tools,
            Err(error) => {
                let _ = self.stop().await;
                return Err(error);
            }
        };
        let wildcard = definition.allowed_tools.iter().any(|name| name == "*");
        Ok(tools
            .into_iter()
            .filter(|tool| {
                let name = text(&tool.spec().wire_name);
                (wildcard || definition.allowed_tools.contains(&name))
                    && !definition.disallowed_tools.contains(&name)
            })
            .collect())
    }
    fn new(runtime: Arc<dyn McpRuntimePort>, keepalive: Arc<dyn Send + Sync>) -> Self {
        Self(Arc::new(McpOwner {
            keepalive: Mutex::new(Some(keepalive)),
            runtime,
            cleanup: Mutex::new(None),
            executor: tokio::runtime::Handle::current(),
        }))
    }
    fn start_stop(&self) -> McpCleanupReceipt {
        let mut cleanup = self.0.cleanup.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(receipt) = cleanup.as_ref() {
            return receipt.clone();
        }
        let (sender, receipt) = tokio::sync::watch::channel(None);
        *cleanup = Some(receipt.clone());
        let runtime = self.0.runtime.clone();
        let keepalive = self
            .0
            .keepalive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.0.executor.spawn(async move {
            let result = runtime
                .stop()
                .await
                .map_err(|e| JsString::from(e.to_string()));
            drop(keepalive);
            sender.send_replace(Some(result));
        });
        receipt
    }
    pub(super) async fn stop(&self) -> Result<(), JsString> {
        let mut receipt = self.start_stop();
        loop {
            let outcome = receipt.borrow().clone();
            if let Some(outcome) = outcome {
                return outcome;
            }
            if receipt.changed().await.is_err() {
                return Err("Fork MCP cleanup terminated before completion.".into());
            }
        }
    }
}
impl Drop for ForkMcp {
    fn drop(&mut self) {
        if Arc::strong_count(&self.0) == 1 {
            self.start_stop();
        }
    }
}

pub(super) fn fork_input(
    definition: &SubagentDefinition,
    directive: &JsString,
    session: &JsString,
    turn: &JsString,
) -> (JsValue, JsValue) {
    let prefix = definition
        .initial_prompt
        .as_ref()
        .map(|value| crate::runtime::memory::js_trim_units(value.units()))
        .filter(|units| !units.is_empty());
    let input = prefix
        .map(|units| JsString::from_utf16(units.to_vec()))
        .unwrap_or_else(|| directive.clone());
    let mut options = object([
        ("sessionId", JsValue::String(session.clone())),
        ("turnId", JsValue::String(turn.clone())),
    ]);
    if prefix.is_some() {
        options.insert(
            "syntheticMessages".into(),
            JsValue::Array(vec![JsValue::Object(object([
                ("role", "user".into()),
                (
                    "content",
                    JsValue::Array(vec![JsValue::Object(object([
                        ("type", "text".into()),
                        ("text", JsValue::String(directive.clone())),
                    ]))]),
                ),
            ]))]),
        );
    }
    (
        JsValue::Object(object([
            ("type", "text".into()),
            ("text", JsValue::String(input)),
        ])),
        JsValue::Object(options),
    )
}
#[cfg(test)]
#[path = "agent_scope_tests.rs"]
mod tests;
