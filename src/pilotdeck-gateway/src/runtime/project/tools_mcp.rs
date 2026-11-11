//! A6.9-pr3c1: MCP and execution-world families for ProjectResourcesFactory.
//! ProjectMcpRuntimeBundle.ts (mcp/runtime/ProjectMcpRuntimeProvider.ts:
//! options assembly) and ProjectExecutionWorldBundle.ts.
use super::*;
use crate::runtime::{
    mcp::{
        registry::{BuiltinMcpPaths, ProjectMcpRuntimeProvider, ProjectMcpRuntimeProviderOptions},
        sdk::SystemEnvironment,
    },
    model::http::ProxySettings,
};
use std::{any::Any, sync::Arc};

// ---------------------------------------------------------------------------
// MCP family
// ---------------------------------------------------------------------------

/// Wraps a `ProjectMcpRuntimeProvider` as a `FamilyResource` so the
/// production staging machinery can own and dispose it.
pub struct McpProviderResource {
    provider: Arc<ProjectMcpRuntimeProvider>,
}
impl McpProviderResource {
    /// #1433: the session prepare closure reads the provider to compose each
    /// session's MCP tools; the resource stays the staging owner.
    pub fn provider(&self) -> &Arc<ProjectMcpRuntimeProvider> {
        &self.provider
    }
}
impl FamilyResource for McpProviderResource {
    fn dispose(&self) -> DisposeFuture<'_> {
        Box::pin(self.provider.dispose())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Production MCP family source.  Constructs a `ProjectMcpRuntimeProvider`
/// from the staged config and registers it as the McpProvider owner.
#[derive(Default)]
pub struct McpFamilySource {
    /// What the builtin plugins' MCP placeholders resolve to; the default
    /// (all empty) is a host that ships no builtin plugins.
    pub builtin_paths: BuiltinMcpPaths,
}

impl ResourceFamilySource for McpFamilySource {
    fn family(&self) -> Family {
        Family::McpProvider
    }

    fn stage(&self, context: &StageContext, sink: &mut StageSink<'_>) -> RuntimeResult {
        let project_root = context.project_root();
        let pilot_home = context.pilot_home();
        let snapshot = context.snapshot();

        let proxy_settings = snapshot
            .get("config")
            .and_then(|c| c.get("proxy"))
            .and_then(|v| v.get("url"))
            .and_then(|u| u.as_string())
            .filter(|url| !url.is_empty())
            .map(|url| ProxySettings {
                url: String::from_utf16_lossy(url.units()),
                no_proxy: None,
                ..ProxySettings::default()
            });

        let options = ProjectMcpRuntimeProviderOptions {
            project_root: project_root
                .to_str()
                .ok_or_else(|| RuntimeError::new("MCP: project root is not valid UTF-8"))?
                .to_owned(),
            pilot_home: pilot_home
                .to_str()
                .ok_or_else(|| RuntimeError::new("MCP: pilot home is not valid UTF-8"))?
                .to_owned(),
            create_runtime: None,
            proxy: proxy_settings,
            on_diagnostic: None,
            environment: Arc::new(SystemEnvironment),
            builtin_paths: self.builtin_paths.clone(),
        };

        let provider = Arc::new(ProjectMcpRuntimeProvider::new(options));
        let resource = Arc::new(McpProviderResource {
            provider: provider.clone(),
        });

        sink.register_owner(self.family(), resource);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ExecutionWorld family: the project's background task runtime
// (`createNodeExecutionWorldBundle`, tool/execution-world/ExecutionWorldBundle.ts:
// 177-223). Sessions of the project share it through their `task_*` tools, and
// the gateway's `background_tasks` / `background_task_stop` read it.
// ---------------------------------------------------------------------------

/// `executionWorld.backgroundTasks` beside `runtime.backgroundSubagents`.
pub struct ExecutionWorldResource {
    background: crate::runtime::background::BackgroundResources,
}
impl ExecutionWorldResource {
    /// `backgroundTasks: new BackgroundTaskRuntime({ diskSpillDir:
    /// <state>/output, snapshotStore: <state>/state.json })` with the state
    /// directory `<projectRoot>/.pilotdeck/background-tasks`. Nothing is
    /// written until a task starts.
    fn new(project_root: &std::path::Path) -> std::io::Result<Self> {
        use crate::runtime::background::{
            BackgroundResources, BackgroundTaskRuntime, BackgroundTaskRuntimeOptions,
        };
        let state_dir = project_root.join(".pilotdeck").join("background-tasks");
        let snapshot_store =
            crate::runtime::background_snapshot::JsonFileBackgroundTaskSnapshotStore::new(
                state_dir.join("state.json"),
            )?;
        Ok(Self {
            background: BackgroundResources {
                tasks: BackgroundTaskRuntime::new(BackgroundTaskRuntimeOptions {
                    // This runtime appends `output/` itself.
                    disk_spill_dir: Some(state_dir.clone()),
                    snapshot_store: Some(Arc::new(snapshot_store)),
                    ..BackgroundTaskRuntimeOptions::default()
                }),
                subagents: Default::default(),
            },
        })
    }
    pub fn background(&self) -> &crate::runtime::background::BackgroundResources {
        &self.background
    }
}
impl FamilyResource for ExecutionWorldResource {
    fn dispose(&self) -> DisposeFuture<'_> {
        crate::runtime::project::RuntimeResources::dispose(&self.background)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Production execution-world family source.
pub struct ExecutionWorldFamilySource;

impl ResourceFamilySource for ExecutionWorldFamilySource {
    fn family(&self) -> Family {
        Family::ExecutionWorld
    }

    fn stage(&self, context: &StageContext, sink: &mut StageSink<'_>) -> RuntimeResult {
        let resource = Arc::new(
            ExecutionWorldResource::new(&context.project_root())
                .map_err(|error| RuntimeError::new(error.to_string()))?,
        );
        sink.register_owner(self.family(), resource.clone());
        sink.register_resource(Resource::ExecutionWorld, resource);
        Ok(())
    }
}
