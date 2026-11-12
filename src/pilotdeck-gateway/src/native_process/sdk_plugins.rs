//! Native SDK directory plugins. The disk loader reads data manifests,
//! Markdown, hooks and server definitions; no JS function loader is involved.
use crate::runtime::{
    plugins::{LoadedPlugin, PluginRuntime, PluginRuntimeError, PluginSessionContributionLease},
    sdk_session::{DiskSdkPluginLoader, SdkSessionError, load_sdk_session_plugins},
};
use pilotdeck_protocol::{JsString, JsValue};
use std::sync::Arc;

/// A validated candidate, committed together with the SDK config only after
/// all model/skill/settings checks succeeded. Preparing never mutates a session.
pub(super) struct PreparedPlugins {
    pub plugins: Vec<Arc<LoadedPlugin>>,
}
impl PreparedPlugins {
    pub async fn skill_names(&self, runtime: &PluginRuntime) -> Vec<String> {
        runtime
            .create_view(self.plugins.clone())
            .await
            .all_skills()
            .into_iter()
            .map(|skill| skill.name)
            .collect()
    }
}

pub(super) async fn prepare(
    runtime: &PluginRuntime,
    requested: Option<&JsValue>,
) -> Result<PreparedPlugins, SdkSessionError> {
    runtime
        .refresh()
        .await
        .map_err(|error| SdkSessionError::raw("Error", error.to_string()))?;
    let plugins = load_sdk_session_plugins(requested, runtime, Some(&DiskSdkPluginLoader)).await?;
    Ok(PreparedPlugins { plugins })
}

/// Acquire exactly one generation snapshot for a live session. Its release
/// belongs in the session's retained cleanup, after consumers such as MCP close.
pub(super) async fn acquire(
    runtime: &PluginRuntime,
    plugins: Vec<Arc<LoadedPlugin>>,
) -> Result<PluginSessionContributionLease, PluginRuntimeError> {
    runtime.refresh().await?;
    runtime.acquire_session_contributions(plugins).await
}

/// MCP reads the same committed plugin list as session composition. The closure
/// is supplied by SessionOverrides, whose one mutex commits config and plugins.
type PluginLookup = dyn Fn(&JsString) -> Vec<Arc<LoadedPlugin>> + Send + Sync;
pub(super) struct SessionPluginSource(pub Arc<PluginLookup>);
impl crate::runtime::mcp::sdk::SdkMcpSessionPlugins for SessionPluginSource {
    fn plugins(&self, session_key: &JsValue) -> Vec<Arc<LoadedPlugin>> {
        session_key
            .as_string()
            .map(|key| (self.0)(key))
            .unwrap_or_default()
    }
}

#[cfg(test)]
#[path = "sdk_plugins_tests.rs"]
mod tests;
