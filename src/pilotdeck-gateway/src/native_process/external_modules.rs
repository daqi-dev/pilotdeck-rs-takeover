//! Session-owned selection of configured HTTP module slots.
pub(crate) mod context;
pub(crate) mod domain;
pub(crate) mod http;
pub(crate) mod model;
pub(crate) mod tools;

use std::sync::Arc;

use pilotdeck_protocol::JsValue;

use crate::skills::{HyperModuleTransport, ModuleEnvironment, ModuleHttpTransport};

use self::http::{ExternalHttpBinding, HttpModuleClient, HttpModuleError};

/// One immutable snapshot per session generation. Each turn binds its own
/// cancellation and identity to the clients selected here.
#[derive(Clone, Default)]
pub(super) struct Selection {
    pub model: Option<Arc<HttpModuleClient>>,
    pub tools: Option<Arc<HttpModuleClient>>,
    pub context: Option<Arc<HttpModuleClient>>,
    pub skills: Option<Arc<HttpModuleClient>>,
    pub knowledge_runtime: Option<Arc<domain::ExternalKnowledgeModule>>,
    pub tool_catalog: Vec<pilotdeck_protocol::JsObject>,
    pub tool_sequence: Arc<std::sync::atomic::AtomicU64>,
    pub knowledge_scope: Option<JsValue>,
    pub skill_runtime: Option<(
        Arc<domain::ExternalSkillModule>,
        Vec<domain::ExternalSkillContribution>,
    )>,
    transport: Option<Arc<dyn ModuleHttpTransport>>,
    environment: Option<Arc<dyn ModuleEnvironment>>,
}

struct Environment(crate::config::Env);

impl ModuleEnvironment for Environment {
    fn var(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
}

impl Selection {
    pub fn from_snapshot(
        snapshot: &JsValue,
        env: &crate::config::Env,
    ) -> Result<Self, HttpModuleError> {
        let modules = snapshot.get("config").and_then(|v| v.get("modules"));
        let transport = Arc::new(HyperModuleTransport::new());
        let environment = Arc::new(Environment(env.clone()));
        let select = |name: &str| -> Result<Option<Arc<HttpModuleClient>>, HttpModuleError> {
            let Some(binding) = modules.and_then(|v| v.get(name)) else {
                return Ok(None);
            };
            if binding.get("enabled") == Some(&JsValue::Bool(false))
                || binding.get("implementationId").is_none()
            {
                return Ok(None);
            }
            let binding = ExternalHttpBinding::from_value(binding)?;
            Ok(Some(Arc::new(HttpModuleClient::new(
                binding,
                transport.clone(),
                environment.clone(),
            ))))
        };
        let knowledge = select("knowledge")?;
        let knowledge_runtime = knowledge
            .as_ref()
            .map(|client| Arc::new(domain::ExternalKnowledgeModule::new(client.clone())));
        Ok(Self {
            model: select("modelProvider")?,
            tools: select("tools")?,
            context: select("context")?,
            skills: select("skills")?,
            knowledge_runtime,
            tool_catalog: match modules
                .and_then(|v| v.get("tools"))
                .and_then(|v| v.get("tools"))
            {
                Some(JsValue::Array(entries)) => entries
                    .iter()
                    .filter_map(JsValue::as_object)
                    .cloned()
                    .collect(),
                _ => Vec::new(),
            },
            tool_sequence: Arc::default(),
            knowledge_scope: modules.and_then(|v| v.get("knowledge")).cloned(),
            skill_runtime: None,
            transport: Some(transport),
            environment: Some(environment),
        })
    }

    /// A child session recreates runtime ports, while its inherited domain
    /// tools retain their parent session's skill and knowledge closures.
    pub fn for_child(&self) -> Self {
        let mut child = self.clone();
        if let (Some(transport), Some(environment)) = (&self.transport, &self.environment) {
            let fresh = |client: &Option<Arc<HttpModuleClient>>| {
                client.as_ref().map(|client| {
                    Arc::new(HttpModuleClient::new(
                        client.binding().clone(),
                        transport.clone(),
                        environment.clone(),
                    ))
                })
            };
            child.model = fresh(&self.model);
            child.tools = fresh(&self.tools);
            child.context = fresh(&self.context);
            child.tool_sequence = Arc::default();
        }
        child
    }

    pub async fn load_skills(&mut self, project: &std::path::Path) -> Result<(), HttpModuleError> {
        if let Some(client) = &self.skills {
            let module = Arc::new(domain::ExternalSkillModule::new(client.clone()));
            let mut input = pilotdeck_protocol::JsObject::new();
            input.insert(
                "projectKey".into(),
                project.to_string_lossy().into_owned().into(),
            );
            let skills = module.list(JsValue::Object(input), None).await?;
            self.skill_runtime = Some((module, skills));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_uses_normalized_catalog_and_respects_disabled_slots() {
        let snapshot = pilotdeck_protocol::parse_json(r#"{
          "config": {"modules": {
            "modelProvider": {"implementationId":"model","contract":"pilotdeck.model/v1","transport":"module-http-v2","endpoint":"http://127.0.0.1:1","methods":["prepare","stream"]},
            "tools": {"implementationId":"tools","contract":"pilotdeck.tools/v1","transport":"module-http-v2","endpoint":"http://127.0.0.1:1","methods":["execute"],"tools":[{"name":"remote"}]},
            "skills": {"enabled":false}, "knowledge": {"enabled":false}
          }}
        }"#).unwrap();
        let selection = Selection::from_snapshot(&snapshot, &Default::default()).unwrap();
        assert!(selection.model.is_some());
        assert!(selection.tools.is_some());
        assert_eq!(
            selection.tool_catalog[0].get("name"),
            Some(&"remote".into())
        );
        assert!(selection.context.is_none());
        assert!(selection.skills.is_none());
        assert!(selection.knowledge_runtime.is_none());
        let child = selection.for_child();
        assert!(!Arc::ptr_eq(
            selection.model.as_ref().unwrap(),
            child.model.as_ref().unwrap()
        ));
        assert!(!Arc::ptr_eq(
            selection.tools.as_ref().unwrap(),
            child.tools.as_ref().unwrap()
        ));
        assert!(!Arc::ptr_eq(&selection.tool_sequence, &child.tool_sequence));
    }
}
