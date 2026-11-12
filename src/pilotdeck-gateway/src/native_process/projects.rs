//! Project routing for the native host: which project a request is served
//! from, and which keys the dialog surface accepts.
//!
//! The baseline has no per-process project. `createLocalGateway` keeps a
//! lazily populated runtime map (`ProjectRuntimeRegistry.resolve`,
//! src/cli/ProjectRuntimeRegistry.ts:1540-1560) and every method reads
//! `input.projectKey ?? fallbackProjectRoot` itself. Two different rules sit
//! on top of that read, and each caller picks the one its baseline method
//! uses:
//!
//! - **Runtime routing** ([`Options::project_root_for`]): a plain lexical
//!   `path.resolve`. Nothing is validated; a key naming no directory still
//!   gets a runtime (`list_sessions` answers an empty list for it).
//! - **Dialog admission** ([`Options::dialog_project`]):
//!   `dialog.projects.resolveProjectKey` (src/gateway/dialog/
//!   projectRegistry.ts:42-59). The pilot home is the virtual General
//!   workspace and always resolves; every other key must be *registered* — a
//!   `<pilot home>/projects/<id>` directory whose `.cwd` marker (or legacy
//!   slug) names an existing directory equal to the key
//!   (src/web/server/listProjects.ts:26-63,199-213). The gateway never
//!   registers a project on its own; an unregistered key is
//!   `PROJECT_NOT_FOUND` / `Unknown projectKey: <as sent>`.
use super::{Options, requests::Requests};
use crate::{
    metadata::{Metadata, NativeFiles},
    runtime::{
        connection::RequestError,
        project::ProjectRuntimeRegistry,
        turn::{
            admission::{SubmitDialogRecovery, SubmitModelAssertion},
            submit::{SubmitError, SubmitFuture},
        },
    },
};
use pilotdeck_protocol::{JsString, JsValue};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// `input.projectKey` as the baseline's `??` reads it: `undefined` and `null`
/// are "not provided". A non-string is returned as `Err` so the caller can
/// answer the failure its own baseline method produces for it.
pub(crate) fn requested_key(params: &JsValue) -> Result<Option<&JsString>, &JsValue> {
    match params.get("projectKey") {
        None | Some(JsValue::Undefined | JsValue::Null) => Ok(None),
        Some(JsValue::String(key)) => Ok(Some(key)),
        Some(other) => Err(other),
    }
}

/// `Unknown projectKey: ${projectKey}` with the caller's own UTF-16.
pub(crate) fn unknown_project_message(key: &JsString) -> JsString {
    let mut units = "Unknown projectKey: ".encode_utf16().collect::<Vec<_>>();
    units.extend_from_slice(key.units());
    JsString::from_utf16(units)
}

impl Options {
    /// The project catalog over this process's pilot home. Relative keys and
    /// markers resolve against the process working directory, like
    /// `path.resolve` in the baseline.
    pub(crate) fn project_catalog(&self) -> Metadata {
        Metadata {
            home: self.home.clone(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            files: NativeFiles,
        }
    }

    /// `resolve(projectKey ?? fallbackProjectRoot)`: the root whose runtime,
    /// storage directory and working directory serve this key.
    pub(crate) fn project_root_for(&self, key: Option<&JsString>) -> PathBuf {
        match key {
            None => self.project.clone(),
            Some(key) => {
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
                let resolved = crate::metadata::resolve_units(&cwd.to_string_lossy(), key);
                PathBuf::from(String::from_utf16_lossy(resolved.units()))
            }
        }
    }

    /// `dialog.projects.resolveProjectKey(projectKey ?? fallbackProjectRoot)`:
    /// the registered spelling of the project, or the message of the
    /// `PROJECT_NOT_FOUND` rejection.
    pub(crate) fn dialog_project(&self, key: Option<&JsString>) -> Result<PathBuf, JsString> {
        let fallback;
        let key = match key {
            Some(key) => key,
            None => {
                fallback = JsString::from(self.project.to_string_lossy().as_ref());
                &fallback
            }
        };
        self.project_catalog()
            .resolve_dialog_project(key)
            .map(PathBuf::from)
            .ok_or_else(|| unknown_project_message(key))
    }

    /// Whether the default project is the General workspace (the pilot home),
    /// which the dialog registry accepts without a registration.
    pub(crate) fn default_project_is_general(&self) -> bool {
        self.project == self.home
    }
}

/// The failure `registry.resolve(projectKey)` reports for a root no runtime
/// can be built on. The baseline builds each project runtime around a
/// background-task snapshot store that reads
/// `<root>/.pilotdeck/background-tasks/state.json` in its constructor
/// (src/task/storage/BackgroundTaskSnapshotStore.ts:92-101): a missing file is
/// an empty store, every other read failure — a `projectKey` naming a file
/// answers ENOTDIR — throws this message, and the method that asked for the
/// runtime fails with it.
fn runtime_root_failure(root: &Path) -> Option<String> {
    let state = root.join(".pilotdeck/background-tasks/state.json");
    match std::fs::read(&state) {
        Ok(_) => None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => Some(format!(
            "Could not read background task snapshot {}.",
            state.display()
        )),
    }
}

/// `registry.resolve(projectKey)`: the root, with its runtime staged on first
/// use and kept for later requests (ProjectRuntimeRegistry.ts:1540-1560).
pub(crate) fn stage_runtime(
    options: &Options,
    registry: &ProjectRuntimeRegistry,
    key: Option<&JsString>,
) -> Result<PathBuf, String> {
    let root = options.project_root_for(key);
    if let Some(message) = runtime_root_failure(&root) {
        return Err(message);
    }
    registry
        .resolve(Some(&root))
        .map_err(|error| error.message)?;
    Ok(root)
}

/// Node's `path.resolve(<non-string>)` rejection, the failure every baseline
/// method that resolves `input.projectKey` answers for a non-string key.
fn invalid_key(key: &JsValue) -> RequestError {
    match crate::metadata::argument_received(key) {
        crate::metadata::ArgumentReceived::Text(received) => RequestError {
            code: "ERR_INVALID_ARG_TYPE".into(),
            message: crate::metadata::invalid_arg_type_message("paths[0]", &received),
            details: None,
            validation: None,
        },
        crate::metadata::ArgumentReceived::Throws { message, .. } => RequestError::new(message),
    }
}

/// `registry.resolve(input.projectKey)`: runtime routing, no registration
/// check.
pub(crate) fn route_project_with(
    options: &Options,
    registry: &ProjectRuntimeRegistry,
    params: &JsValue,
) -> Result<PathBuf, RequestError> {
    let key = requested_key(params).map_err(invalid_key)?;
    stage_runtime(options, registry, key).map_err(RequestError::new)
}

impl Requests {
    /// `registry.resolve(input.projectKey)` for a request: runtime routing,
    /// no registration check.
    pub(crate) fn route_project(&self, params: &JsValue) -> Result<PathBuf, RequestError> {
        route_project_with(&self.options, &self.project, params)
    }

    /// `dialog.projects.resolveProjectKey(input.projectKey ??
    /// fallbackProjectRoot)` for a request.
    pub(crate) fn dialog_project(&self, params: &JsValue) -> Result<PathBuf, RequestError> {
        let key = requested_key(params).map_err(invalid_key)?;
        self.options
            .dialog_project(key)
            .map_err(|message| RequestError {
                code: "PROJECT_NOT_FOUND".into(),
                message,
                details: None,
                validation: None,
            })
    }
}

/// The two project checks of the baseline's submit preflight, in its order
/// (src/gateway/client/InProcessGateway.ts:1115-1202). `assertSdkModelAllowed`
/// resolves the runtime the turn will run in
/// (`registry.resolve(projectKey ?? fallbackProjectRoot)`,
/// ProjectRuntimeRegistry.ts:830-838) and any failure there is reported as
/// `INVALID_SDK_SESSION_CONFIG`; then `clearRecoveredUserDialogs` asks the
/// dialog registry for the project (createLocalGateway.ts:1446-1449) and its
/// rejection is reported as `gateway_dialog_recovery_cleanup_failed`; the
/// cleanup then deletes the session's restart-recovered dialog journal
/// (`super::dialog_recovery::clear`). The model step checks the
/// session's SDK `managedModels` (no organization policy exists here).
pub(super) struct SubmitProjectAdmission {
    pub options: Options,
    pub registry: Arc<ProjectRuntimeRegistry>,
    /// The stored `sdkSessionConfig`s whose `managedModels` the selected
    /// model must pass.
    pub overrides: Arc<super::SessionOverrides>,
}

impl SubmitModelAssertion for SubmitProjectAdmission {
    fn assert(
        &self,
        session: &JsString,
        selection: Option<&JsValue>,
        project: Option<&JsString>,
    ) -> Result<(), SubmitError> {
        let root = stage_runtime(&self.options, &self.registry, project)
            .map_err(|message| SubmitError::new("INVALID_SDK_SESSION_CONFIG", message))?;
        // `assertSdkManagedModelAllowed(sdkSessionConfigs.get(sessionKey)
        // ?.managedModels, model ?? runtime.snapshot.config.agent.model)`
        // (ProjectRuntimeRegistry.ts:830-850).
        let Some(policy) = self
            .overrides
            .sdk_config(session)
            .and_then(|config| config.get("managedModels").cloned())
        else {
            return Ok(());
        };
        let model = match selection {
            Some(selection) => selection.clone(),
            None => self
                .registry
                .resolve(Some(&root))
                .ok()
                .and_then(|runtime| {
                    runtime
                        .resources()
                        .get::<crate::runtime::project::ProjectResources>()
                        .and_then(|resources| {
                            resources
                                .context()
                                .snapshot()
                                .get("config")
                                .and_then(|config| config.get("agent"))
                                .and_then(|agent| agent.get("model"))
                                .cloned()
                        })
                })
                .unwrap_or(JsValue::Undefined),
        };
        let text = |key: &str| {
            model
                .get(key)
                .and_then(JsValue::as_string)
                .cloned()
                .unwrap_or_default()
        };
        super::sdk_config::assert_managed_model_allowed(
            Some(&policy),
            &text("provider"),
            &text("model"),
        )
    }
}

impl SubmitDialogRecovery for SubmitProjectAdmission {
    fn clear_recovered<'a>(
        &'a self,
        session: &'a JsString,
        project: Option<&'a JsString>,
    ) -> SubmitFuture<'a, Result<(), SubmitError>> {
        Box::pin(async move {
            let root = self.options.dialog_project(project).map_err(|message| {
                SubmitError::new(
                    "PROJECT_NOT_FOUND",
                    String::from_utf16_lossy(message.units()),
                )
            })?;
            // `clearRecoveredUserDialogsForSdk` (ProjectRuntimeRegistry.ts:724-730).
            super::dialog_recovery::clear(&self.options.home, &root, &self.overrides, session)
                .map_err(|error| {
                    SubmitError::new(
                        "gateway_dialog_recovery_cleanup_failed",
                        String::from_utf16_lossy(error.message.units()),
                    )
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn options(home: PathBuf, project: PathBuf) -> Options {
        Options {
            home,
            project_root: project.clone(),
            project,
            listen: "127.0.0.1:0".parse().expect("listen"),
            token_file: PathBuf::from("/unused/token"),
            sidecar: PathBuf::from("/unused/sidecar"),
            sidecar_args: Vec::new(),
            drain_budget: Duration::from_secs(1),
            storage_root: None,
            os_home: None,
            builtin_skills_root: None,
            builtin_plugins_root: None,
            in_process_cli: false,
        }
    }

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let root = std::env::temp_dir()
                .join(format!("pdrs-projects-{}", uuid::Uuid::new_v4()))
                .canonicalize()
                .unwrap_or_else(|_| {
                    let root = std::env::temp_dir()
                        .join(format!("pdrs-projects-{}", uuid::Uuid::new_v4()));
                    std::fs::create_dir_all(&root).expect("scratch");
                    root.canonicalize().expect("canonical scratch")
                });
            std::fs::create_dir_all(root.join("home")).expect("home");
            Self(root)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn key(path: &std::path::Path) -> JsString {
        JsString::from(path.to_string_lossy().as_ref())
    }

    /// projectRegistry.ts:42-59 over listProjects.ts:199-213, as the baseline
    /// answered in `scripts/bbx/scenarios/project-keys.json`: the pilot home
    /// always resolves, a marker-registered directory resolves, an existing
    /// but unregistered directory, a missing one and a stale marker do not.
    #[test]
    fn dialog_admission_follows_registration_not_existence() {
        let scratch = Scratch::new();
        let home = scratch.0.join("home");
        let options = options(home.clone(), home.clone());
        let registered = scratch.0.join("work-a");
        let unregistered = scratch.0.join("work-b");
        let stale = scratch.0.join("gone");
        for dir in [&registered, &unregistered] {
            std::fs::create_dir_all(dir).expect("project dir");
        }
        for (id, target) in [("any-id", &registered), ("stale-id", &stale)] {
            let dir = home.join("projects").join(id);
            std::fs::create_dir_all(&dir).expect("registration dir");
            std::fs::write(dir.join(".cwd"), format!("{}\n", target.display())).expect("marker");
        }

        assert_eq!(options.dialog_project(None), Ok(home.clone()));
        assert_eq!(options.dialog_project(Some(&key(&home))), Ok(home.clone()));
        assert_eq!(
            options.dialog_project(Some(&key(&registered))),
            Ok(registered.clone())
        );
        for refused in [&unregistered, &stale] {
            assert_eq!(
                options
                    .dialog_project(Some(&key(refused)))
                    .map_err(|message| String::from_utf16_lossy(message.units())),
                Err(format!("Unknown projectKey: {}", refused.display()))
            );
        }
        // The rejection echoes the key as sent, not its resolved form.
        assert_eq!(
            options
                .dialog_project(Some(&JsString::from("work/alpha")))
                .map_err(|message| String::from_utf16_lossy(message.units())),
            Err("Unknown projectKey: work/alpha".to_owned())
        );
    }

    /// LocalGatewayBootConfig.ts:52 default (`fallbackProjectRoot ??
    /// projectRoot`), the `--native` entry's shape: the default project is an
    /// ordinary project and must itself be registered.
    #[test]
    fn a_non_general_default_project_needs_its_registration() {
        let scratch = Scratch::new();
        let home = scratch.0.join("home");
        let project = scratch.0.join("project");
        std::fs::create_dir_all(&project).expect("project");
        let options = options(home.clone(), project.clone());
        assert!(!options.default_project_is_general());
        assert!(options.dialog_project(None).is_err());
        let dir = home.join("projects/registered");
        std::fs::create_dir_all(&dir).expect("registration dir");
        std::fs::write(dir.join(".cwd"), project.to_string_lossy().as_bytes()).expect("marker");
        assert_eq!(options.dialog_project(None), Ok(project));
    }

    /// ProjectRuntimeRegistry.ts:1542: runtime routing is `path.resolve` and
    /// nothing else.
    #[test]
    fn runtime_routing_is_lexical() {
        let options = options(PathBuf::from("/h"), PathBuf::from("/h"));
        assert_eq!(options.project_root_for(None), PathBuf::from("/h"));
        assert_eq!(
            options.project_root_for(Some(&JsString::from("/a/b/../c/./d/"))),
            PathBuf::from("/a/c/d")
        );
        let relative = options.project_root_for(Some(&JsString::from("rel/x")));
        assert_eq!(
            relative,
            std::env::current_dir().expect("cwd").join("rel/x")
        );
    }

    #[test]
    fn nullish_project_key_is_not_provided() {
        use crate::metadata::object;
        assert_eq!(requested_key(&object([])), Ok(None));
        assert_eq!(
            requested_key(&object([("projectKey", JsValue::Null)])),
            Ok(None)
        );
        assert_eq!(
            requested_key(&object([("projectKey", "/p".into())])),
            Ok(Some(&JsString::from("/p")))
        );
        assert!(requested_key(&object([("projectKey", JsValue::Number(7.0))])).is_err());
    }
}
