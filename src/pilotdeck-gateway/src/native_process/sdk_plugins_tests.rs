use super::*;
use crate::runtime::plugins::PluginRuntimeOptions;
use pilotdeck_protocol::{parse_json, stringify_json};
use std::{collections::HashMap, path::PathBuf};

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("pdrs-sdk-plugins-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn runtime(&self) -> PluginRuntime {
        PluginRuntime::new(PluginRuntimeOptions {
            project_root: self.0.join("project"),
            pilot_home: self.0.join("home"),
            builtin_skills_root: None,
            builtin_plugins: vec![],
            builtin_plugins_enabled: HashMap::new(),
        })
    }
    fn plugin(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::create_dir_all(path.join("skills/hello")).unwrap();
        std::fs::create_dir_all(path.join("commands")).unwrap();
        std::fs::create_dir_all(path.join("output-styles")).unwrap();
        for (relative, data) in [
            (
                "plugin.json",
                include_str!("../../tests/fixtures/sdk-session-plugins/local/plugin.json"),
            ),
            (
                "skills/hello/SKILL.md",
                include_str!(
                    "../../tests/fixtures/sdk-session-plugins/local/skills/hello/SKILL.md"
                ),
            ),
            (
                "commands/greet.md",
                include_str!("../../tests/fixtures/sdk-session-plugins/local/commands/greet.md"),
            ),
            (
                "output-styles/brief.md",
                include_str!(
                    "../../tests/fixtures/sdk-session-plugins/local/output-styles/brief.md"
                ),
            ),
        ] {
            std::fs::write(path.join(relative), data).unwrap();
        }
        path
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn requested(paths: &[PathBuf]) -> JsValue {
    JsValue::Array(
        paths
            .iter()
            .map(|path| {
                let mut value = pilotdeck_protocol::JsObject::new();
                value.insert("type".into(), "local".into());
                value.insert("path".into(), path.to_string_lossy().as_ref().into());
                JsValue::Object(value)
            })
            .collect(),
    )
}

#[tokio::test]
async fn data_directory_loads_skills_commands_style_hooks_and_mcp() {
    let root = Root::new();
    let runtime = root.runtime();
    let candidate = prepare(&runtime, Some(&requested(&[root.plugin("local")])))
        .await
        .unwrap();
    assert_eq!(candidate.plugins.len(), 1);
    assert_eq!(candidate.plugins[0].name, "session-data");
    assert!(!candidate.skill_names(&runtime).await.is_empty());
    assert_eq!(
        runtime
            .create_view(candidate.plugins.clone())
            .await
            .list_output_styles()
            .len(),
        1
    );
    let mut lease = acquire(&runtime, candidate.plugins).await.unwrap();
    assert_eq!(lease.contributions.commands.len(), 1);
    assert_eq!(lease.contributions.skills.len(), 1);
    assert!(
        lease
            .contributions
            .mcp_servers
            .get("session-local")
            .is_some()
    );
    assert!(!lease.contributions.hooks.is_empty());
    lease.release().await.unwrap();
    runtime.dispose().await.unwrap();
}

#[tokio::test]
async fn preparation_errors_preserve_exact_sdk_plugin_codes() {
    let root = Root::new();
    let runtime = root.runtime();
    let local = root.plugin("local");
    let file = root.0.join("file");
    std::fs::write(&file, "plain").unwrap();
    let bad = root.0.join("bad");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(bad.join("plugin.json"), "not json").unwrap();
    for (paths, code) in [
        (vec![root.0.join("missing")], "SDK_PLUGIN_NOT_FOUND"),
        (vec![file], "SDK_PLUGIN_NOT_DIRECTORY"),
        (vec![bad], "SDK_PLUGIN_LOAD_FAILED"),
        (
            vec![local.clone(), local.clone()],
            "SDK_PLUGIN_DUPLICATE_PATH",
        ),
        (
            vec![local, root.plugin("same-name")],
            "SDK_PLUGIN_NAME_CONFLICT",
        ),
    ] {
        let failure = prepare(&runtime, Some(&requested(&paths)))
            .await
            .err()
            .unwrap();
        assert_eq!(failure.code, code);
    }
    runtime.dispose().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn canonical_alias_duplicate_is_rejected() {
    let root = Root::new();
    let runtime = root.runtime();
    let local = root.plugin("local");
    let alias = root.0.join("alias");
    std::os::unix::fs::symlink(&local, &alias).unwrap();
    let error = prepare(&runtime, Some(&requested(&[local, alias])))
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "SDK_PLUGIN_DUPLICATE_PATH");
    runtime.dispose().await.unwrap();
}

#[tokio::test]
async fn session_mcp_source_controls_status_and_reserved_names() {
    use crate::config::permission::GatewaySessionPermissionRuleSetRegistry;
    use crate::runtime::mcp::sdk::*;
    use crate::runtime::sdk_session::SdkSessionProjects;
    struct Resolver(Arc<PluginRuntime>);
    impl SdkMcpProjectResolver for Resolver {
        fn resolve<'a>(
            &'a self,
            _: Option<&'a JsString>,
        ) -> SdkMcpFuture<'a, Result<Arc<dyn SdkMcpProject>, SdkMcpError>> {
            Box::pin(async move {
                Ok(
                    Arc::new(PluginProjectRuntime::new("project", self.0.clone()))
                        as Arc<dyn SdkMcpProject>,
                )
            })
        }
    }
    let root = Root::new();
    let runtime = Arc::new(root.runtime());
    let candidate = prepare(&runtime, Some(&requested(&[root.plugin("local")])))
        .await
        .unwrap();
    let committed = Arc::new(super::super::session_overrides::SessionOverrides::default());
    committed.commit_sdk_config_with_plugins(
        &"one".into(),
        parse_json("{}").unwrap(),
        candidate.plugins,
        root.0.clone(),
    );
    let source = SessionPluginSource({
        let committed = committed.clone();
        Arc::new(move |key| committed.sdk_plugins(key))
    });
    let config = SdkMcpConfig::new(SdkMcpConfigOptions {
        resolver: Arc::new(Resolver(runtime.clone())),
        close_session: Arc::new(|_, _| Box::pin(async { Ok(()) })),
        session_plugins: Arc::new(source),
        permission_rule_sets: Arc::new(GatewaySessionPermissionRuleSetRegistry::new()),
        environment: Arc::new(SystemEnvironment),
        projects: SdkSessionProjects::shared(),
    });
    let one = config
        .mcp_server_status(&parse_json(r#"{"sessionKey":"one"}"#).unwrap())
        .await
        .unwrap();
    assert!(
        stringify_json(&one)
            .unwrap()
            .unwrap()
            .contains("session-local")
    );
    let two = config
        .mcp_server_status(&parse_json(r#"{"sessionKey":"two"}"#).unwrap())
        .await
        .unwrap();
    assert!(
        !stringify_json(&two)
            .unwrap()
            .unwrap()
            .contains("session-local")
    );
    let conflict = config
        .set_mcp_servers(
            &parse_json(r#"{"sessionKey":"one","servers":{"session-local":{"command":"unused"}}}"#)
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        stringify_json(&conflict)
            .unwrap()
            .unwrap()
            .contains("owned by project configuration or a plugin")
    );
    committed.clear_sdk_config(&"one".into());
    let cleared = config
        .mcp_server_status(&parse_json(r#"{"sessionKey":"one"}"#).unwrap())
        .await
        .unwrap();
    assert!(
        !stringify_json(&cleared)
            .unwrap()
            .unwrap()
            .contains("session-local")
    );
    runtime.dispose().await.unwrap();
}

#[tokio::test]
async fn contribution_lease_remains_stable_after_candidate_replacement_and_releases() {
    let root = Root::new();
    let runtime = Arc::new(root.runtime());
    let local = root.plugin("local");
    let base = root.plugin("project/.pilotdeck/plugins/base");
    let manifest = std::fs::read_to_string(base.join("plugin.json"))
        .unwrap()
        .replace("session-data", "base-data");
    std::fs::write(base.join("plugin.json"), manifest).unwrap();
    let first = prepare(&runtime, Some(&requested(std::slice::from_ref(&local))))
        .await
        .unwrap();
    let mut held = acquire(&runtime, first.plugins).await.unwrap();
    std::fs::write(
        local.join("skills/hello/SKILL.md"),
        "---\nname: hello\n---\nRECONFIGURED_SKILL\n",
    )
    .unwrap();
    let second = prepare(&runtime, Some(&requested(&[local]))).await.unwrap();
    let mut newer = acquire(&runtime, second.plugins).await.unwrap();
    assert!(
        held.contributions
            .skills
            .iter()
            .any(|skill| skill.namespace == "session-data"
                && skill.content.contains("SESSION_PLUGIN_SKILL"))
    );
    assert!(
        newer
            .contributions
            .skills
            .iter()
            .any(|skill| skill.namespace == "session-data"
                && skill.content.contains("RECONFIGURED_SKILL"))
    );
    let disposing = tokio::spawn({
        let runtime = runtime.clone();
        async move { runtime.dispose().await }
    });
    tokio::task::yield_now().await;
    assert!(
        !disposing.is_finished(),
        "live contribution leases keep the runtime open"
    );
    held.release().await.unwrap();
    newer.release().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), disposing)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn failed_reconfiguration_keeps_committed_config_and_session_isolation() {
    let root = Root::new();
    let runtime = root.runtime();
    let store = super::super::session_overrides::SessionOverrides::default();
    let first = prepare(&runtime, Some(&requested(&[root.plugin("local")])))
        .await
        .unwrap();
    let config = parse_json(r#"{"plugins":[{"type":"local","path":"first"}]}"#).unwrap();
    store.commit_sdk_config_with_plugins(
        &"one".into(),
        config.clone(),
        first.plugins,
        root.0.clone(),
    );
    assert!(
        prepare(&runtime, Some(&requested(&[root.0.join("missing")])))
            .await
            .is_err()
    );
    let (still_config, still_plugins) = store.sdk_snapshot(&"one".into());
    assert_eq!(still_config, Some(config));
    assert_eq!(still_plugins.len(), 1);
    assert!(store.sdk_snapshot(&"two".into()).0.is_none());
    assert!(store.sdk_plugins(&"two".into()).is_empty());
    let empty = prepare(&runtime, None).await.unwrap();
    store.commit_sdk_config_with_plugins(
        &"one".into(),
        parse_json("{}").unwrap(),
        empty.plugins,
        root.0.clone(),
    );
    assert!(store.sdk_plugins(&"one".into()).is_empty());
    assert_eq!(
        still_plugins[0].name, "session-data",
        "a live snapshot retains its old plugin values"
    );
    store.clear_sdk_config(&"one".into());
    assert!(store.sdk_snapshot(&"one".into()).0.is_none());
    assert!(store.sdk_plugins(&"one".into()).is_empty());
    runtime.dispose().await.unwrap();
}

#[tokio::test]
async fn read_skill_executes_against_the_frozen_session_directory_view() {
    use crate::{
        host::modules::tool_context::{ToolFileState, ToolRuntimeContext},
        runtime::tools::{ExecutableTool, ToolRunContext, builtin::read_skill::ReadSkillTool},
    };
    let root = Root::new();
    let runtime = Arc::new(root.runtime());
    let prepared = prepare(&runtime, Some(&requested(&[root.plugin("local")])))
        .await
        .unwrap();
    let mut lease = acquire(&runtime, prepared.plugins).await.unwrap();
    let name = lease.contributions.skills[0].name.clone();
    let tool =
        ReadSkillTool::new(runtime.clone()).with_contributions(lease.contributions.skills.clone());
    let host = ToolRuntimeContext {
        fields: Default::default(),
        files: ToolFileState {
            read_file_state: Arc::default(),
            write_snapshots: Arc::default(),
            allowed_read_files: vec![],
        },
        abort_signal: None,
        model: None,
        elicitation: None,
        user_dialog: None,
        audit_recorder: None,
        file_history: None,
        file_update_notifier: None,
        now: None,
        progress: None,
        subagent: None,
        plan_todo: None,
        goal: None,
        plan_directory: None,
    };
    let input = parse_json(&format!(
        "{{\"skillName\":{}}}",
        stringify_json(&JsValue::String(name.into()))
            .unwrap()
            .unwrap()
    ))
    .unwrap();
    let output = tool
        .execute(input, ToolRunContext::new(Arc::new(host)))
        .await
        .unwrap();
    assert!(
        stringify_json(&JsValue::Array(output.content))
            .unwrap()
            .unwrap()
            .contains("SESSION_PLUGIN_SKILL")
    );
    assert!(
        runtime.all_skills().await.is_empty(),
        "session plugin must not leak into the project catalog"
    );
    lease.release().await.unwrap();
    runtime.dispose().await.unwrap();
}
