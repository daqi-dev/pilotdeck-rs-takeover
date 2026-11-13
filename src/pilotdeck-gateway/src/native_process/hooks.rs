//! The hooks of a native session: where their settings come from, how their
//! execution is projected onto the gateway stream, and the tool runtime's
//! lifecycle port.
//!
//! Baseline: `SessionInteractionBundle` (cli/SessionInteractionBundle.ts)
//! composes one hook runtime per session from the plugin contributions and
//! the SDK session config; `GatewaySessionLiveProjectionBundle` projects its
//! execution events; `ToolRuntime.dispatchLifecycle` dispatches the tool
//! events. A `hooks` section in `pilotdeck.yaml` is not a source: the config
//! loader reports it as an unknown top-level field (loadPilotConfig.ts:343-375).
use super::permission_prompt::GatewayEmitSlot;
use crate::{
    hooks::protocol::{HookCommand, HookCommon, HookMatcher, HooksSettings, is_hook_event},
    host::{HostFuture, modules::lifecycle::LifecyclePort},
    runtime::{
        hooks::{
            LifecycleDispatchInput, LifecycleDispatchResult,
            runtime::{
                AsyncHookRegistration, AsyncHookRegistrationHandler, HookExecutionEventHandler,
            },
        },
        tools::{ToolLifecycle, ToolRunContext},
    },
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::sync::{Arc, Mutex};

/// `createSdkHookSettings` + `mergeHookSettings`
/// (SessionInteractionBundle.ts:126-140, 325-351): every SDK matcher becomes
/// one HTTP hook posting to `hooks.url` with `event` and `matcher` (its index)
/// set on the query; the SDK `ConfigChange` / `FileChanged` matchers run in
/// their own runtimes and are left out here. SDK matchers follow the plugin
/// matchers of the same event.
pub(super) fn session_hook_settings(
    settings: HooksSettings,
    sdk_config: Option<&JsValue>,
) -> HooksSettings {
    sdk_hook_settings(settings, sdk_config, |event| {
        !event.equals_str("ConfigChange") && !event.equals_str("FileChanged")
    })
}

/// The SDK matchers of `ConfigChange` alone: the separate runtime of
/// SessionInteractionBundle.ts:170-180.
pub(super) fn sdk_config_change_settings(sdk_config: Option<&JsValue>) -> HooksSettings {
    sdk_hook_settings(Vec::new(), sdk_config, |event| {
        event.equals_str("ConfigChange")
    })
}

/// The SDK matchers of `FileChanged` alone: the separate runtime of
/// SessionInteractionBundle.ts:181-192.
pub(super) fn sdk_file_changed_settings(sdk_config: Option<&JsValue>) -> HooksSettings {
    sdk_hook_settings(Vec::new(), sdk_config, |event| {
        event.equals_str("FileChanged")
    })
}

/// `createSdkHookSettings` for the events `include` selects, appended to
/// `settings`.
fn sdk_hook_settings(
    mut settings: HooksSettings,
    sdk_config: Option<&JsValue>,
    include: impl Fn(&JsString) -> bool,
) -> HooksSettings {
    let Some(hooks) = sdk_config
        .and_then(|config| config.get("hooks"))
        .and_then(JsValue::as_object)
    else {
        return settings;
    };
    let Some(url) = hooks.get("url").and_then(JsValue::as_string) else {
        return settings;
    };
    // `{ ...config.headers }`.
    let headers = hooks
        .get("headers")
        .and_then(JsValue::as_object)
        .map(|headers| {
            let mut copy = JsObject::new();
            for (name, value) in headers.own_entries() {
                copy.insert(name.clone(), value.clone());
            }
            copy
        });
    let Some(events) = hooks.get("events").and_then(JsValue::as_object) else {
        return settings;
    };
    for (event, matchers) in events.own_entries() {
        if !is_hook_event(event) || !include(event) {
            continue;
        }
        let JsValue::Array(matchers) = matchers else {
            continue;
        };
        let converted: Vec<HookMatcher> = matchers
            .iter()
            .enumerate()
            .map(|(index, matcher)| HookMatcher {
                matcher: matcher
                    .get("matcher")
                    .and_then(JsValue::as_string)
                    .filter(|matcher| !matcher.is_empty())
                    .cloned(),
                hooks: vec![HookCommand::Http {
                    url: sdk_hook_url(url, event, index),
                    headers: headers.clone(),
                    allowed_env_vars: None,
                    common: HookCommon::default(),
                }],
                ..Default::default()
            })
            .collect();
        match settings.iter_mut().find(|(existing, _)| existing == event) {
            Some((_, existing)) => existing.extend(converted),
            None => settings.push((event.clone(), converted)),
        }
    }
    settings
}

/// `appendSdkHookSelector`: `URLSearchParams#set("event")`, then
/// `set("matcher")`, serialized the WHATWG way.
fn sdk_hook_url(url: &JsString, event: &JsString, matcher: usize) -> JsString {
    let text = String::from_utf16_lossy(url.units());
    let Ok(mut endpoint) = url::Url::parse(&text) else {
        return url.clone();
    };
    let mut pairs: Vec<(String, String)> = endpoint
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let mut set = |name: &str, value: String| match pairs.iter().position(|(key, _)| key == name) {
        Some(first) => {
            pairs[first].1 = value;
            let mut index = 0;
            pairs.retain(|(key, _)| {
                let keep = key != name || index == first;
                index += 1;
                keep
            });
        }
        None => pairs.push((name.to_string(), value)),
    };
    set("event", String::from_utf16_lossy(event.units()));
    set("matcher", matcher.to_string());
    endpoint.query_pairs_mut().clear().extend_pairs(pairs);
    endpoint.as_str().into()
}

/// `createGatewayHookExecutionProjection` / `createGatewaySdkHookExecutionProjection`
/// (gateway/hooks/GatewayHookExecutionProjection.ts): one session's hook
/// transitions on its active gateway stream. `sdk` is
/// `sdkSessionConfig.includeHookEvents === true`.
pub(super) struct GatewayHookProjection {
    pub session: JsString,
    pub emit: GatewayEmitSlot,
    /// The sink for events held until the active turn publishes its terminal
    /// (`TurnEventCoordinator::emit_before_terminal`).
    pub emit_before_terminal: GatewayEmitSlot,
    pub sdk: bool,
}

/// Whether a transition is the completion of an advisory hook
/// (`PreModelRequest` / `InstructionsLoaded`) that the turn's abort killed.
///
/// The baseline dispatches these two without awaiting them, under the turn's
/// abort signal (AgentLoop.ts:699-702, 2281-2283;
/// sidecarDefaultModuleDispatcher.ts:383-389), so a user abort kills the hook
/// while the loop is streaming. Nothing orders the hook's completion against
/// the aborted turn's own events there, but the order on the wire is stable:
/// the completion waits for the child's `close` (SubprocessPort.ts:85, 137)
/// while the truncated block comes back over the sidecar first, so the
/// baseline emits the truncated `assistant_block`, then this completion, then
/// `turn_completed`. Here the kill and the turn's abort handling race, so the
/// completion is held until the turn has published everything but its
/// terminal.
fn is_aborted_advisory_completion(event: &JsObject) -> bool {
    let is = |key: &str, value: &str| {
        event
            .get(key)
            .and_then(JsValue::as_string)
            .is_some_and(|field| field.equals_str(value))
    };
    is("type", "response")
        && is("outcome", "cancelled")
        && (is("hookEvent", "PreModelRequest") || is("hookEvent", "InstructionsLoaded"))
}
impl HookExecutionEventHandler for GatewayHookProjection {
    fn handle(&self, event: &JsObject) {
        if event.get("sessionId").and_then(JsValue::as_string) != Some(&self.session) {
            return;
        }
        let field = |key: &str| event.get(key).cloned().unwrap_or(JsValue::Undefined);
        let started = event
            .get("type")
            .and_then(JsValue::as_string)
            .is_some_and(|kind| kind.equals_str("started"));
        let mut projected = JsObject::new();
        if self.sdk {
            projected.insert(
                "type".into(),
                if started {
                    "hook_started"
                } else {
                    "hook_response"
                }
                .into(),
            );
            projected.insert("hookName".into(), field("hookName"));
            projected.insert("hookEvent".into(), field("hookEvent"));
            if !started {
                for key in ["stdout", "stderr", "exitCode", "outcome"] {
                    if let Some(value) = event.get(key) {
                        projected.insert(key.into(), value.clone());
                    }
                }
                if let Some(id) = event
                    .get("asyncInvocationId")
                    .and_then(JsValue::as_string)
                    .filter(|id| !id.is_empty())
                {
                    projected.insert("asyncInvocationId".into(), JsValue::String(id.clone()));
                }
                if let Some(timeout) = event.get("asyncTimeoutMs") {
                    projected.insert("asyncTimeoutMs".into(), timeout.clone());
                }
            }
        } else {
            let mut detail = JsObject::new();
            detail.insert("hookName".into(), field("hookName"));
            detail.insert("hookEvent".into(), field("hookEvent"));
            if !started {
                detail.insert("outcome".into(), field("outcome"));
                if let Some(code) = event.get("exitCode") {
                    detail.insert("exitCode".into(), code.clone());
                }
            }
            projected.insert("type".into(), "agent_status".into());
            projected.insert(
                "event".into(),
                if started {
                    "hook_execution_started"
                } else {
                    "hook_execution_completed"
                }
                .into(),
            );
            projected.insert("detail".into(), JsValue::Object(detail));
        }
        let emit = if is_aborted_advisory_completion(event) {
            &self.emit_before_terminal
        } else {
            &self.emit
        };
        if let Some(emit) = emit.get() {
            emit(&self.session, JsValue::Object(projected));
        }
    }
}

/// The gateway's deferred async-hook registry, published once the host that
/// owns it is built (after the session factory).
pub(super) type AsyncRegistrySlot =
    Arc<std::sync::OnceLock<Arc<crate::runtime::async_hooks::AsyncHookRegistry>>>;

/// `registerAsyncHook` bound to one session (SessionInteractionBundle.ts:146-157,
/// InProcessGateway.registerAsyncHook): the deferred result belongs to the
/// session's active turn.
pub(super) struct GatewayAsyncHooks {
    pub session: JsString,
    pub registry: AsyncRegistrySlot,
    pub include_hook_events: bool,
}
impl AsyncHookRegistrationHandler for GatewayAsyncHooks {
    fn handle(&self, registration: AsyncHookRegistration) -> Result<(), crate::host::HostError> {
        match self.registry.get() {
            Some(registry) => {
                registry.register(crate::runtime::async_hooks::AsyncHookRegistration {
                    session_key: self.session.clone(),
                    invocation_id: registration.invocation_id,
                    hook_name: registration.hook_name,
                    hook_event: registration.hook_event,
                    timeout_ms: registration.timeout_ms,
                    include_hook_events: self.include_hook_events,
                })
            }
            None => Ok(()),
        }
    }
}

/// The tool runtime's `lifecycle`: the current turn's bound hooks. A session
/// runs one turn at a time; the slot is replaced when the next turn binds.
#[derive(Default)]
pub(super) struct ToolHooks {
    port: Mutex<Option<Arc<dyn LifecyclePort>>>,
    /// The events the session has hooks for. Any other event's dispatch has
    /// no matcher to run, so its result is empty without a round trip.
    events: Mutex<Vec<JsString>>,
    /// The session-scoped runtime of the gateway-side observers (Elicitation,
    /// ElicitationResult): their hooks outlive the turn that asked, like the
    /// baseline's session `HookRuntime`, which a turn's end neither cancels
    /// nor waits for.
    session: Mutex<Option<crate::runtime::hooks::LifecycleRuntime>>,
    /// `configChangeLifecycle` (SessionInteractionBundle.ts:170-180): the
    /// SDK `ConfigChange` hooks in a runtime of their own, sharing the
    /// session's execution projection, without async registration.
    config_change: Mutex<Option<crate::runtime::hooks::LifecycleRuntime>>,
    /// `fileChangedLifecycle` (SessionInteractionBundle.ts:181-192): the SDK
    /// `FileChanged` hooks, which also hand async outputs to the gateway.
    file_changed: Mutex<Option<crate::runtime::hooks::LifecycleRuntime>>,
}
impl ToolHooks {
    /// Compose the SDK `FileChanged` runtime over `settings`.
    pub(super) fn configure_file_changed(
        &self,
        settings: &HooksSettings,
        observers: &crate::runtime::hooks::runtime::HookObservers,
    ) {
        let runtime = crate::runtime::hooks::HookRuntime::with_options(
            settings.clone(),
            crate::runtime::hooks::HookRuntimeOptions {
                shell: observers.shell.clone(),
                on_async_registered: observers.async_registered.clone(),
                observer: observers.execution.clone(),
                ..Default::default()
            },
        );
        *self.file_changed.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(crate::runtime::hooks::LifecycleRuntime::new(runtime));
    }
    /// `fileUpdateNotifier.didSave` (SessionInteractionBundle.ts:193-213): a
    /// committed save dispatches `FileChanged` on the SDK runtime, matched on
    /// the workspace-relative path, and waits for it; failures and results
    /// are dropped. `None` when the session has no SDK `FileChanged` hooks.
    pub(super) fn sdk_file_saved(
        &self,
        session: JsString,
        cwd: JsString,
    ) -> Option<crate::runtime::tools::FileUpdateNotifier> {
        let lifecycle = self
            .file_changed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()?;
        Some(Arc::new(
            move |update: crate::runtime::tools::FileUpdate, tool_cwd: JsString| {
                let (lifecycle, session, cwd) = (lifecycle.clone(), session.clone(), cwd.clone());
                Box::pin(async move {
                    let root = workspace_root(&update, &tool_cwd);
                    let mut base = JsObject::new();
                    base.insert("sessionId".into(), JsValue::String(session));
                    base.insert("transcriptPath".into(), "".into());
                    base.insert("cwd".into(), JsValue::String(cwd.clone()));
                    let mut payload = JsObject::new();
                    payload.insert(
                        "filePath".into(),
                        JsValue::String(update.relative_path.clone()),
                    );
                    payload.insert(
                        "absolutePath".into(),
                        JsValue::String(update.absolute_path.clone()),
                    );
                    payload.insert("root".into(), JsValue::String(root));
                    payload.insert(
                        "changeType".into(),
                        if update.created { "created" } else { "updated" }.into(),
                    );
                    let input = LifecycleDispatchInput {
                        event: "FileChanged".into(),
                        base_input: base,
                        payload: Some(payload),
                        match_query: Some(update.relative_path.clone()),
                        cwd,
                        env: None,
                        signal: None,
                    };
                    let _ = lifecycle.dispatch(input).await;
                    Ok(())
                })
            },
        ))
    }
    /// Compose the SDK `ConfigChange` runtime over `settings`.
    pub(super) fn configure_config_change(
        &self,
        settings: &HooksSettings,
        observers: &crate::runtime::hooks::runtime::HookObservers,
    ) {
        let runtime = crate::runtime::hooks::HookRuntime::with_options(
            settings.clone(),
            crate::runtime::hooks::HookRuntimeOptions {
                shell: observers.shell.clone(),
                observer: observers.execution.clone(),
                ..Default::default()
            },
        );
        *self.config_change.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(crate::runtime::hooks::LifecycleRuntime::new(runtime));
    }
    /// `void this.configChangeLifecycle.dispatch({ event: "ConfigChange",
    /// baseInput: { sessionId: sessionKey, transcriptPath: "", cwd:
    /// projectRoot }, payload, matchQuery: "ConfigChange" }).catch(() => {})`
    /// (SessionInteractionBundle.ts:286-299): nobody waits for the hooks and
    /// their result has no effect.
    pub(super) fn dispatch_config_change(
        &self,
        session: &JsString,
        cwd: &JsString,
        payload: &JsValue,
    ) {
        let lifecycle = self
            .config_change
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let (Some(lifecycle), Some(payload)) = (lifecycle, payload.as_object()) else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let mut base = JsObject::new();
        base.insert("sessionId".into(), JsValue::String(session.clone()));
        base.insert("transcriptPath".into(), "".into());
        base.insert("cwd".into(), JsValue::String(cwd.clone()));
        let input = LifecycleDispatchInput {
            event: "ConfigChange".into(),
            base_input: base,
            payload: Some(payload.clone()),
            match_query: Some("ConfigChange".into()),
            cwd: cwd.clone(),
            env: None,
            signal: None,
        };
        runtime.spawn(async move {
            let _ = lifecycle.dispatch(input).await;
        });
    }
    /// The session's own runtime over `settings`, with the session's
    /// observers (execution projection, async registration, shell).
    pub(super) fn configure_session(
        &self,
        settings: &HooksSettings,
        observers: &crate::runtime::hooks::runtime::HookObservers,
    ) {
        let runtime = crate::runtime::hooks::HookRuntime::with_options(
            settings.clone(),
            crate::runtime::hooks::HookRuntimeOptions {
                shell: observers.shell.clone(),
                on_async_registered: observers.async_registered.clone(),
                observer: observers.execution.clone(),
                ..Default::default()
            },
        );
        *self.session.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(crate::runtime::hooks::LifecycleRuntime::new(runtime));
    }
    pub(super) fn configure(&self, settings: &HooksSettings) {
        *self.events.lock().unwrap_or_else(|e| e.into_inner()) = settings
            .iter()
            .filter(|(_, matchers)| !matchers.is_empty())
            .map(|(event, _)| event.clone())
            .collect();
    }
    /// The session-scoped runtime as a lifecycle port, once configured.
    pub(super) fn session_port(&self) -> Option<Arc<dyn LifecyclePort>> {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .map(|runtime| Arc::new(runtime) as Arc<dyn LifecyclePort>)
    }
    pub(super) fn bind(&self, port: Option<Arc<dyn LifecyclePort>>) {
        *self.port.lock().unwrap_or_else(|e| e.into_inner()) = port;
    }
    /// `void this.lifecycle.dispatch({ event, baseInput: { sessionId:
    /// sessionKey, transcriptPath: "", cwd: projectRoot }, payload,
    /// matchQuery: event }).catch(() => {})` — the session-scoped observers
    /// of SessionInteractionBundle.ts:239-246 and 301-313 (Elicitation,
    /// ElicitationResult): no permission mode, no turn environment, nobody
    /// waits for the hooks or reads their result.
    pub(super) fn dispatch_detached(
        &self,
        event: &'static str,
        session: &JsString,
        cwd: &JsString,
        payload: JsObject,
    ) {
        let configured = self
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|configured| configured.equals_str(event));
        let lifecycle = self
            .session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let (Some(lifecycle), true) = (lifecycle, configured) else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let mut base = JsObject::new();
        base.insert("sessionId".into(), JsValue::String(session.clone()));
        base.insert("transcriptPath".into(), "".into());
        base.insert("cwd".into(), JsValue::String(cwd.clone()));
        let input = LifecycleDispatchInput {
            event: event.into(),
            base_input: base,
            payload: Some(payload),
            match_query: Some(event.into()),
            cwd: cwd.clone(),
            env: None,
            signal: None,
        };
        runtime.spawn(async move {
            let _ = lifecycle.dispatch(input).await;
        });
    }
}
/// `CompactionEngine`'s `lifecycle` (SessionContextRuntimeBundle.ts:104-119):
/// PreCompact / PostCompact through the session's hooks of the current turn,
/// with the base input `{ sessionId: sessionKey, transcriptPath: "", cwd:
/// projectRoot, permissionMode: "default" }` and the event name as the match
/// query. The dispatch result is not consulted.
pub(super) struct CompactionHooks {
    pub hooks: Arc<ToolHooks>,
    pub session: JsString,
    pub cwd: JsString,
}
impl crate::runtime::context::compaction::engine::CompactionLifecycle for CompactionHooks {
    fn dispatch(&self, event: &'static str, payload: JsValue) -> HostFuture<()> {
        let configured = self
            .hooks
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|configured| configured.equals_str(event));
        let port = self
            .hooks
            .port
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let (Some(port), true) = (port, configured) else {
            return Box::pin(async { Ok(()) });
        };
        let mut base = JsObject::new();
        base.insert("sessionId".into(), JsValue::String(self.session.clone()));
        base.insert("transcriptPath".into(), "".into());
        base.insert("cwd".into(), JsValue::String(self.cwd.clone()));
        base.insert("permissionMode".into(), "default".into());
        let future = port.dispatch(LifecycleDispatchInput {
            event: event.into(),
            base_input: base,
            payload: payload.as_object().cloned(),
            match_query: Some(event.into()),
            cwd: self.cwd.clone(),
            env: None,
            signal: None,
        });
        Box::pin(async move { future.await.map(|_| ()) })
    }
}

impl ToolLifecycle for ToolHooks {
    fn dispatch(
        &self,
        event: &'static str,
        match_query: &JsString,
        payload: JsObject,
        context: &ToolRunContext,
    ) -> HostFuture<Option<LifecycleDispatchResult>> {
        let configured = self
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|configured| configured.equals_str(event));
        let port = self.port.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let (Some(port), true) = (port, configured) else {
            return Box::pin(async { Ok(None) });
        };
        let field = |key: &str| {
            context
                .fields
                .get(key)
                .cloned()
                .unwrap_or(JsValue::Undefined)
        };
        let cwd = field("cwd").as_string().cloned().unwrap_or_default();
        let mut base = JsObject::new();
        base.insert("sessionId".into(), field("sessionId"));
        base.insert("transcriptPath".into(), "".into());
        base.insert("cwd".into(), JsValue::String(cwd.clone()));
        base.insert("permissionMode".into(), field("permissionMode"));
        let input = LifecycleDispatchInput {
            event: event.into(),
            base_input: base,
            payload: Some(payload),
            match_query: Some(match_query.clone()),
            cwd,
            env: context
                .fields
                .get("env")
                .and_then(JsValue::as_object)
                .cloned(),
            signal: None,
        };
        let future = port.dispatch(input);
        Box::pin(async move { future.await.map(Some) })
    }
}

/// The save's workspace root (`PilotDeckPathSafetyResult.root`,
/// pathSafety.ts:26-104): the root the relative path was taken against. A
/// path inside one of the roots ends with its relative path; every other
/// branch takes the path relative to the tool's working directory.
fn workspace_root(update: &crate::runtime::tools::FileUpdate, tool_cwd: &JsString) -> JsString {
    let absolute = String::from_utf16_lossy(update.absolute_path.units());
    let relative = String::from_utf16_lossy(update.relative_path.units());
    if relative == "." {
        return update.absolute_path.clone();
    }
    if relative.starts_with("..") || std::path::Path::new(&relative).is_absolute() {
        return tool_cwd.clone();
    }
    match absolute
        .strip_suffix(&relative)
        .and_then(|root| root.strip_suffix('/'))
    {
        Some(root) if !root.is_empty() => root.into(),
        Some(_) => "/".into(),
        None => tool_cwd.clone(),
    }
}
