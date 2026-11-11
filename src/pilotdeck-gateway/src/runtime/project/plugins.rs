//! A6.9-pr3e: PluginRuntime family for ProjectRuntimeFactory.
//! ProjectRuntimeResourcesBundle.ts:218-225.
use super::*;
use crate::runtime::plugins::{PluginRuntime, PluginRuntimeOptions};
use pilotdeck_protocol::JsValue;
use std::{any::Any, sync::Arc};

pub struct PluginFamilySource {
    pub options: PluginRuntimeOptions,
}

pub struct PluginResources {
    pub runtime: Arc<PluginRuntime>,
}

impl FamilyResource for PluginResources {
    fn dispose(&self) -> DisposeFuture<'_> {
        Box::pin(async {
            self.runtime
                .dispose()
                .await
                .map_err(|error| RuntimeError::new(error.to_string()))
        })
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl ResourceFamilySource for PluginFamilySource {
    fn family(&self) -> Family {
        Family::PluginRuntime
    }

    fn stage(&self, context: &StageContext, sink: &mut StageSink<'_>) -> RuntimeResult {
        // `builtinPluginsEnabled: snapshot.config.extension.builtinPluginsEnabled`
        // (ProjectRuntimeResourcesBundle.ts:223): each project generation reads
        // its own config. The factory-level map only seeds entries the config
        // does not name (production passes it empty).
        let mut builtin_plugins_enabled = self.options.builtin_plugins_enabled.clone();
        let snapshot = context.snapshot();
        if let Some(enabled) = snapshot
            .get("config")
            .and_then(|config| config.get("extension"))
            .and_then(|extension| extension.get("builtinPluginsEnabled"))
            .and_then(JsValue::as_object)
        {
            for (name, value) in enabled.own_entries() {
                if let JsValue::Bool(value) = value {
                    builtin_plugins_enabled.insert(String::from_utf16_lossy(name.units()), *value);
                }
            }
        }
        let runtime = Arc::new(PluginRuntime::new(PluginRuntimeOptions {
            // One factory stages every project of the registry, so the plugin
            // runtime follows the generation's own root
            // (ProjectRuntimeResourcesBundle.ts:218-225 passes `projectRoot`).
            project_root: context.project_root(),
            pilot_home: self.options.pilot_home.clone(),
            builtin_skills_root: self.options.builtin_skills_root.clone(),
            builtin_plugins: self.options.builtin_plugins.clone(),
            builtin_plugins_enabled,
        }));
        let resource = Arc::new(PluginResources { runtime });
        sink.register_owner(self.family(), resource.clone());
        sink.register_resource(Resource::PluginRuntime, resource);
        Ok(())
    }
}
