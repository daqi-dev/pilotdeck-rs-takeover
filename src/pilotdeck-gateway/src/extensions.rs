//! Rust source-level extensions, scoped to an embedding and an optional project.
//!
//! Registrations are atomic generations. Updating or unregistering affects new
//! sessions; an existing session retains its tools, prompts, routers and owned
//! resources until that session closes. No process-global registry or JS loader
//! is involved. Drop the registration to unload it, and use Rust `Drop` on a
//! resource stored in `resources` for final cleanup after the last session lease.
use crate::runtime::{
    model::router::{CustomRouter, CustomRouterRegistry},
    project::{RuntimeError, custom_routers::CustomRouterResources},
    tools::ExecutableTool,
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use pilotdeck_transport::server::ExecutionFactory;
use std::{
    any::Any,
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
};

/// One generation of an extension. Every contribution follows normal gateway
/// permissions and model routing; registration grants no permission bypass.
#[derive(Default)]
pub struct NativeExtension {
    pub tools: Vec<Arc<dyn ExecutableTool>>,
    pub routers: Vec<(JsString, Arc<dyn CustomRouter>)>,
    pub prompts: Vec<String>,
    /// Optional native execution factory. At most one may apply to a project.
    pub factory: Option<Arc<dyn ExecutionFactory>>,
    pub resources: Vec<Arc<dyn Any + Send + Sync>>,
}

struct Entry {
    id: String,
    project: Option<PathBuf>,
    generation: u64,
    value: Arc<NativeExtension>,
    tool_names: Vec<JsString>,
}
#[derive(Default)]
struct State {
    generation: u64,
    entries: Vec<Entry>,
    sessions: HashMap<(PathBuf, JsString), Weak<SessionExtensions>>,
}

/// Cloneable, embedding-owned registry. An absolute project path is
/// canonicalized at registration so a contribution cannot leak to another root.
#[derive(Clone, Default)]
pub struct NativeExtensions(Arc<Mutex<State>>);

/// Ownership token for a registration. An obsolete token cannot remove a newer
/// generation with the same id. Keep this alive while accepting new sessions.
pub struct ExtensionRegistration {
    registry: Weak<Mutex<State>>,
    id: String,
    generation: u64,
}
impl ExtensionRegistration {
    pub fn unregister(&self) -> bool {
        let Some(registry) = self.registry.upgrade() else {
            return false;
        };
        let removed = {
            let mut state = registry.lock().unwrap_or_else(|e| e.into_inner());
            state
                .entries
                .iter()
                .position(|entry| entry.id == self.id && entry.generation == self.generation)
                .map(|index| state.entries.remove(index))
        };
        let existed = removed.is_some();
        // User-owned destructors run outside the registry mutex.
        drop(removed);
        existed
    }
}
impl Drop for ExtensionRegistration {
    fn drop(&mut self) {
        self.unregister();
    }
}

impl NativeExtensions {
    pub fn register(
        &self,
        id: impl Into<String>,
        project: Option<&Path>,
        extension: NativeExtension,
    ) -> Result<ExtensionRegistration, RuntimeError> {
        self.publish(id.into(), project, extension, false)
    }
    pub fn update(
        &self,
        id: impl Into<String>,
        project: Option<&Path>,
        extension: NativeExtension,
    ) -> Result<ExtensionRegistration, RuntimeError> {
        self.publish(id.into(), project, extension, true)
    }
    fn publish(
        &self,
        id: String,
        project: Option<&Path>,
        extension: NativeExtension,
        replace: bool,
    ) -> Result<ExtensionRegistration, RuntimeError> {
        if id.trim().is_empty() {
            return Err(RuntimeError::new("Native extension id must not be empty."));
        }
        let project = project.map(canonical_project).transpose()?;
        let mut names = std::collections::HashSet::new();
        for tool in &extension.tools {
            let name = tool
                .fields()
                .get("name")
                .and_then(JsValue::as_string)
                .filter(|name| !name.units().is_empty())
                .ok_or_else(|| RuntimeError::new("Native extension tool requires a name."))?;
            if !names.insert(name.clone()) {
                return Err(RuntimeError::new("Duplicate native extension tool name."));
            }
        }
        let mut routers = std::collections::HashSet::new();
        if extension
            .routers
            .iter()
            .any(|(id, _)| id.units().is_empty() || !routers.insert(id.clone()))
        {
            return Err(RuntimeError::new(
                "Native extension router ids must be nonempty and unique.",
            ));
        }
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let index = state.entries.iter().position(|entry| entry.id == id);
        if index.is_some() != replace {
            return Err(RuntimeError::new(if replace {
                "Native extension is not registered."
            } else {
                "Native extension is already registered."
            }));
        }
        state.generation = state
            .generation
            .checked_add(1)
            .ok_or_else(|| RuntimeError::new("Native extension generation exhausted."))?;
        let generation = state.generation;
        let entry = Entry {
            id: id.clone(),
            project,
            generation,
            value: Arc::new(extension),
            tool_names: names.into_iter().collect(),
        };
        let retired = match index {
            Some(index) => Some(std::mem::replace(&mut state.entries[index], entry)),
            None => {
                state.entries.push(entry);
                None
            }
        };
        drop(state);
        drop(retired);
        Ok(ExtensionRegistration {
            registry: Arc::downgrade(&self.0),
            id,
            generation,
        })
    }

    pub(crate) fn bind(
        &self,
        project: &Path,
        session: &JsString,
    ) -> Result<Arc<SessionExtensions>, RuntimeError> {
        let project = project
            .canonicalize()
            .unwrap_or_else(|_| project.to_path_buf());
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let key = (project.clone(), session.clone());
        if let Some(session) = state.sessions.get(&key).and_then(Weak::upgrade) {
            return Ok(session);
        }
        state.sessions.retain(|_, value| value.strong_count() > 0);
        let mut tool_names = std::collections::HashSet::new();
        let mut router_ids = std::collections::HashSet::new();
        let mut factories = 0;
        let mut values = Vec::new();
        for entry in state
            .entries
            .iter()
            .filter(|entry| entry.project.as_ref().is_none_or(|root| root == &project))
        {
            for name in &entry.tool_names {
                if !tool_names.insert(name.clone()) {
                    return Err(RuntimeError::new(
                        "Native extensions contribute duplicate tools to this project.",
                    ));
                }
            }
            for (id, _) in &entry.value.routers {
                if !router_ids.insert(id.clone()) {
                    return Err(RuntimeError::new(
                        "Native extensions contribute duplicate routers to this project.",
                    ));
                }
            }
            factories += usize::from(entry.value.factory.is_some());
            values.push(entry.value.clone());
        }
        if factories > 1 {
            return Err(RuntimeError::new(
                "More than one native execution factory applies to this project.",
            ));
        }
        let lease = Arc::new(SessionExtensions { values });
        state.sessions.insert(key, Arc::downgrade(&lease));
        Ok(lease)
    }
    pub(crate) fn router_family(&self) -> Arc<dyn crate::runtime::project::ResourceFamilySource> {
        Arc::new(NativeRouterFamily(self.clone()))
    }
}

fn canonical_project(project: &Path) -> Result<PathBuf, RuntimeError> {
    if !project.is_absolute() {
        return Err(RuntimeError::new(
            "Native extension project must be an absolute path.",
        ));
    }
    project
        .canonicalize()
        .map_err(|e| RuntimeError::new(format!("Cannot resolve native extension project: {e}")))
}

pub(crate) struct SessionExtensions {
    values: Vec<Arc<NativeExtension>>,
}
impl SessionExtensions {
    pub fn tools(&self) -> Vec<Arc<dyn ExecutableTool>> {
        self.values
            .iter()
            .flat_map(|value| value.tools.clone())
            .collect()
    }
    pub fn factory(&self) -> Option<Arc<dyn ExecutionFactory>> {
        self.values.iter().find_map(|value| value.factory.clone())
    }
    pub fn append_prompts(&self, config: &mut JsObject) {
        let prompts: Vec<_> = self
            .values
            .iter()
            .flat_map(|value| value.prompts.iter())
            .filter(|prompt| !prompt.trim().is_empty())
            .cloned()
            .collect();
        if prompts.is_empty() {
            return;
        }
        let mut parts = config
            .get("appendSystemPrompt")
            .and_then(JsValue::as_string)
            .map(|text| vec![String::from_utf16_lossy(text.units())])
            .unwrap_or_default();
        parts.extend(prompts);
        config.insert(
            "appendSystemPrompt".into(),
            JsValue::String(parts.join("\n\n").into()),
        );
    }
}
struct NativeRouterFamily(NativeExtensions);
impl crate::runtime::project::ResourceFamilySource for NativeRouterFamily {
    fn family(&self) -> crate::runtime::project::Family {
        crate::runtime::project::Family::RouterCustomRouters
    }
    fn stage(
        &self,
        context: &crate::runtime::project::StageContext,
        sink: &mut crate::runtime::project::StageSink<'_>,
    ) -> crate::runtime::project::RuntimeResult {
        let resource = Arc::new(CustomRouterResources {
            registry: Arc::new(ProjectRouters {
                extensions: self.0.clone(),
                project: context
                    .project_root()
                    .canonicalize()
                    .unwrap_or_else(|_| context.project_root()),
            }),
        });
        sink.register_owner(self.family(), resource.clone());
        sink.register_resource(
            crate::runtime::project::Resource::RouterCustomRouters,
            resource,
        );
        Ok(())
    }
}
struct ProjectRouters {
    extensions: NativeExtensions,
    project: PathBuf,
}
impl CustomRouterRegistry for ProjectRouters {
    fn lookup_router(
        &self,
        extension_id: &JsString,
        session_id: &JsString,
    ) -> Option<Arc<dyn CustomRouter>> {
        let lease = self
            .extensions
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sessions
            .get(&(self.project.clone(), session_id.clone()))
            .and_then(Weak::upgrade)?;
        lease
            .values
            .iter()
            .flat_map(|value| &value.routers)
            .find(|(id, _)| id == extension_id)
            .map(|(_, router)| router.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::model::router::RouterFuture;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("pdrs-native-extensions-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    struct Resource(Arc<AtomicUsize>);
    impl Drop for Resource {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct Router;
    impl CustomRouter for Router {
        fn decide(&self, input: JsValue) -> RouterFuture<JsValue> {
            Box::pin(async { Ok(input) })
        }
    }
    fn extension(prompt: &str, drops: Arc<AtomicUsize>) -> NativeExtension {
        NativeExtension {
            prompts: vec![prompt.into()],
            routers: vec![("custom".into(), Arc::new(Router))],
            resources: vec![Arc::new(Resource(drops))],
            ..Default::default()
        }
    }
    #[test]
    fn update_and_unload_retain_existing_session_generation_until_release() {
        let root = Scratch::new();
        let extensions = NativeExtensions::default();
        let drops = Arc::new(AtomicUsize::new(0));
        let old_registration = extensions
            .register("test", Some(&root.0), extension("old", drops.clone()))
            .unwrap();
        let old = extensions.bind(&root.0, &"old-session".into()).unwrap();
        let new_registration = extensions
            .update("test", Some(&root.0), extension("new", drops.clone()))
            .unwrap();
        drop(old_registration);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        let current = extensions.bind(&root.0, &"new-session".into()).unwrap();
        let mut old_config = JsObject::new();
        old.append_prompts(&mut old_config);
        assert_eq!(
            old_config.get("appendSystemPrompt"),
            Some(&JsValue::from("old"))
        );
        let mut new_config = JsObject::new();
        current.append_prompts(&mut new_config);
        assert_eq!(
            new_config.get("appendSystemPrompt"),
            Some(&JsValue::from("new"))
        );
        assert!(new_registration.unregister());
        assert!(!new_registration.unregister());
        assert!(
            extensions
                .bind(&root.0, &"after-unload".into())
                .unwrap()
                .values
                .is_empty()
        );
        let routers = ProjectRouters {
            extensions: extensions.clone(),
            project: root.0.clone(),
        };
        assert!(
            routers
                .lookup_router(&"custom".into(), &"old-session".into())
                .is_some()
        );
        assert!(
            routers
                .lookup_router(&"custom".into(), &"new-session".into())
                .is_some()
        );
        drop(old);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(
            routers
                .lookup_router(&"custom".into(), &"old-session".into())
                .is_none()
        );
        drop(current);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }
    #[test]
    fn project_binding_prevents_cross_project_router_and_prompt_access() {
        let first = Scratch::new();
        let second = Scratch::new();
        let extensions = NativeExtensions::default();
        let _registered = extensions
            .register(
                "private",
                Some(&first.0),
                extension("private", Arc::default()),
            )
            .unwrap();
        let _first_session = extensions.bind(&first.0, &"same-name".into()).unwrap();
        let second_session = extensions.bind(&second.0, &"same-name".into()).unwrap();
        assert!(second_session.values.is_empty());
        let routers = ProjectRouters {
            extensions,
            project: second.0.clone(),
        };
        assert!(
            routers
                .lookup_router(&"custom".into(), &"same-name".into())
                .is_none()
        );
    }
    #[test]
    fn invalid_registration_does_not_publish_or_replace() {
        let extensions = NativeExtensions::default();
        assert!(
            extensions
                .register("", None, NativeExtension::default())
                .is_err()
        );
        assert!(
            extensions
                .register("x", Some(Path::new("relative")), NativeExtension::default())
                .is_err()
        );
        assert!(
            extensions
                .update("x", None, NativeExtension::default())
                .is_err()
        );
        let registration = extensions
            .register("x", None, NativeExtension::default())
            .unwrap();
        assert!(
            extensions
                .register("x", None, NativeExtension::default())
                .is_err()
        );
        assert!(registration.unregister());
    }
    #[test]
    fn extension_descriptor_can_reenter_registry_without_deadlocking_admission() {
        use crate::host::{HostError, modules::capability::ToolDefinition};
        use crate::runtime::tools::{ToolFuture, ToolOutput, ToolRunContext};
        struct ReentrantTool {
            registry: NativeExtensions,
            fields: JsObject,
        }
        impl ToolDefinition for ReentrantTool {
            fn fields(&self) -> &JsObject {
                let registration = self
                    .registry
                    .register("descriptor-probe", None, NativeExtension::default())
                    .unwrap();
                drop(registration);
                &self.fields
            }
            fn is_read_only(&self, _: &JsValue) -> Result<JsValue, HostError> {
                Ok(JsValue::Bool(true))
            }
        }
        impl ExecutableTool for ReentrantTool {
            fn execute(&self, _: JsValue, _: ToolRunContext) -> ToolFuture<ToolOutput> {
                unreachable!("descriptor test does not execute tools")
            }
        }
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let root = Scratch::new();
            let registry = NativeExtensions::default();
            let mut fields = JsObject::new();
            fields.insert("name".into(), "reentrant".into());
            let registration = registry
                .register(
                    "extension",
                    None,
                    NativeExtension {
                        tools: vec![Arc::new(ReentrantTool {
                            registry: registry.clone(),
                            fields,
                        })],
                        ..Default::default()
                    },
                )
                .unwrap();
            let session = registry.bind(&root.0, &"s".into()).unwrap();
            drop(registration);
            drop(session);
            sender.send(()).unwrap();
        });
        receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("extension descriptor deadlocked admission");
        worker.join().unwrap();
    }
}
