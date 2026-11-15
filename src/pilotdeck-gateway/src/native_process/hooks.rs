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
mod sdk_events;
mod session;
pub(super) use sdk_events::{SdkEventBus, SdkSessionEvents};
mod sop;
use super::permission_prompt::GatewayEmitSlot;
#[cfg(feature = "kernel-managed-model")]
use crate::host::HostError;
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
pub(super) use session::SessionHookCleanup;
pub(super) use sop::SopNodeLifecycle;
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
    merge_sdk_hooks(settings, sdk_config, false)
}
fn merge_sdk_hooks(
    mut settings: HooksSettings,
    sdk_config: Option<&JsValue>,
    separate: bool,
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
        if !is_hook_event(event)
            || (!separate && (event.equals_str("ConfigChange") || event.equals_str("FileChanged")))
        {
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
    pub sdk: bool,
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
        if let Some(emit) = self.emit.get() {
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
    // Serializes managed snapshot checks with changes to the live turn hook
    // binding. No lock is held over a hook future or a product callback.
    #[cfg(feature = "kernel-managed-model")]
    managed_epoch: Arc<Mutex<u64>>,
    port: Mutex<Option<Arc<dyn LifecyclePort>>>,
    /// The events the session has hooks for. Any other event's dispatch has
    /// no matcher to run, so its result is empty without a round trip.
    events: Mutex<Vec<JsString>>,
    /// The session-scoped runtime of the gateway-side observers (Elicitation,
    /// ElicitationResult): their hooks outlive the turn that asked, like the
    /// baseline's session `HookRuntime`, which a turn's end neither cancels
    /// nor waits for.
    session: Mutex<Option<session::SessionLifecycle>>,
    sdk_events: Mutex<Option<(Arc<SdkSessionEvents>, bool)>>,
}

impl ToolHooks {
    #[cfg(feature = "kernel-managed-model")]
    fn check_managed_empty(&self) -> Result<(), HostError> {
        let configured = self.events.lock().unwrap_or_else(|e| e.into_inner());
        let has_effect = configured.iter().any(|event| {
            [
                "PreToolUse",
                "PermissionRequest",
                "PostToolUse",
                "PostToolUseFailure",
                "FileChanged",
            ]
            .iter()
            .any(|name| event.equals_str(name))
        });
        let has_sdk = self
            .sdk_events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some();
        if has_effect || has_sdk {
            return Err(HostError::new(
                crate::host::HostErrorKind::Protocol,
                "Managed tools require hooks without external effects.",
            ));
        }
        Ok(())
    }

    pub(super) fn configure_sdk_events(
        &self,
        config: Option<&JsValue>,
        bus: Arc<SdkEventBus>,
        session: JsString,
        cwd: JsString,
        observers: &crate::runtime::hooks::runtime::HookObservers,
    ) {
        if let Some(events) = SdkSessionEvents::new(config, bus, session, cwd, observers) {
            #[cfg(feature = "kernel-managed-model")]
            let mut epoch = self.managed_epoch.lock().unwrap_or_else(|e| e.into_inner());
            #[cfg(feature = "kernel-managed-model")]
            {
                *epoch = epoch.saturating_add(1);
            }
            *self.sdk_events.lock().unwrap_or_else(|e| e.into_inner()) =
                Some((Arc::new(events), true));
        }
    }
    pub(super) fn sdk_events(&self) -> Option<Arc<SdkSessionEvents>> {
        self.sdk_events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|(events, _)| events.clone())
    }
    pub(super) fn inherit_sdk_events(&self, events: Option<Arc<SdkSessionEvents>>) {
        #[cfg(feature = "kernel-managed-model")]
        let mut epoch = self.managed_epoch.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(feature = "kernel-managed-model")]
        {
            *epoch = epoch.saturating_add(1);
        }
        *self.sdk_events.lock().unwrap_or_else(|e| e.into_inner()) =
            events.map(|events| (events, false));
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
            Some(session::SessionLifecycle::new(runtime));
    }
    pub(super) fn configure(&self, settings: &HooksSettings) {
        #[cfg(feature = "kernel-managed-model")]
        let mut epoch = self.managed_epoch.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(feature = "kernel-managed-model")]
        {
            *epoch = epoch.saturating_add(1);
        }
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
        #[cfg(feature = "kernel-managed-model")]
        let mut epoch = self.managed_epoch.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(feature = "kernel-managed-model")]
        {
            *epoch = epoch.saturating_add(1);
        }
        *self.port.lock().unwrap_or_else(|e| e.into_inner()) = port;
    }
    fn close_session(&self) -> HostFuture<()> {
        let session = self
            .session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.bind(None);
        let sdk = self
            .sdk_events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .filter(|(_, owner)| *owner)
            .map(|(events, _)| events.close());
        let session = session.map(|session| session.dispose());
        Box::pin(async move {
            let first = match sdk {
                Some(future) => future.await,
                None => Ok(()),
            };
            let second = match session {
                Some(future) => future.await,
                None => Ok(()),
            };
            first.and(second)
        })
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
impl Drop for ToolHooks {
    fn drop(&mut self) {
        // Explicit prepared-resource release awaits disposal. This fallback
        // also cancels commands when a partially constructed session is dropped.
        if let Some(session) = self.session.get_mut().unwrap_or_else(|e| e.into_inner()) {
            session.close();
        }
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
    #[cfg(feature = "kernel-managed-model")]
    fn managed_empty_hooks(&self) -> Result<crate::runtime::tools::ManagedHookGuard, HostError> {
        let epoch = self.managed_epoch.lock().unwrap_or_else(|e| e.into_inner());
        if *epoch == u64::MAX {
            return Err(HostError::new(
                crate::host::HostErrorKind::Protocol,
                "Managed hook configuration is unavailable.",
            ));
        }
        self.check_managed_empty()?;
        Ok(crate::runtime::tools::ManagedHookGuard {
            source: self.managed_epoch.clone(),
            epoch: *epoch,
        })
    }

    fn dispatch(
        &self,
        event: &'static str,
        match_query: &JsString,
        payload: JsObject,
        context: &ToolRunContext,
    ) -> HostFuture<Option<LifecycleDispatchResult>> {
        #[cfg(feature = "kernel-managed-model")]
        if let Some(guard) = &context.managed_hooks {
            let epoch = self.managed_epoch.lock().unwrap_or_else(|e| e.into_inner());
            let valid = Arc::ptr_eq(&self.managed_epoch, &guard.source)
                && *epoch == guard.epoch
                && *epoch != u64::MAX
                && self.check_managed_empty().is_ok();
            return Box::pin(async move {
                if valid {
                    Ok(None)
                } else {
                    Err(HostError::new(
                        crate::host::HostErrorKind::Protocol,
                        "Managed hook configuration changed.",
                    ))
                }
            });
        }
        let configured = self
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|configured| configured.equals_str(event));
        let port = self.port.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let sdk = if event == "FileChanged" {
            self.sdk_events()
                .map(|events| events.file_changed(&payload))
        } else {
            None
        };
        let (Some(port), true) = (port, configured) else {
            return Box::pin(async move {
                if let Some(future) = sdk {
                    future.await?;
                }
                Ok(None)
            });
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
        Box::pin(async move {
            let result = future.await;
            if let Some(sdk) = sdk {
                let _ = sdk.await;
            }
            result.map(Some)
        })
    }
}

#[cfg(all(test, feature = "kernel-managed-model"))]
mod managed_empty_tests {
    use super::*;
    use crate::host::{
        Cancellation,
        modules::{
            capability::{
                RuntimeContext, ToolCall, ToolDefinition, ToolExecution, ToolInvocationContext,
            },
            permission::PermissionDecision,
            tool_context::{ExecutionContext, HostToolCheckpoint, ToolRuntimeContext},
        },
    };
    use crate::runtime::tools::{
        ToolRegistry, builtin,
        scheduler::{
            FinalToolAdmission, FinalToolContinuation, FinalToolPermit, FinalToolRequest,
            SchedulerMode, ToolRuntime,
        },
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Permission(Arc<AtomicUsize>);
    impl PermissionDecision for Permission {
        fn decide(
            &self,
            _: Arc<dyn ToolDefinition>,
            _: JsValue,
            _: RuntimeContext,
            _: JsString,
        ) -> HostFuture<JsValue> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                let mut value = JsObject::new();
                value.insert("type".into(), "allow".into());
                Ok(JsValue::Object(value))
            })
        }
    }
    struct Permit;
    impl FinalToolPermit for Permit {
        fn revalidate(&self) -> Result<(), HostError> {
            Ok(())
        }
    }
    #[derive(Clone)]
    struct Gate(Cancellation);
    impl FinalToolAdmission for Gate {
        fn local_cancellation(&self) -> Option<Cancellation> {
            Some(self.0.clone())
        }
        fn child_producer(&self) -> Result<Arc<dyn FinalToolAdmission>, HostError> {
            Ok(Arc::new(self.clone()))
        }
        fn execute(
            &self,
            _: FinalToolRequest,
            continuation: FinalToolContinuation,
        ) -> HostFuture<JsObject> {
            continuation(Arc::new(Permit))
        }
        fn spawn_detached(&self, _: HostFuture<JsObject>) -> Result<(), HostError> {
            panic!("unexpected detached fixture")
        }
    }

    #[tokio::test]
    async fn real_empty_hooks_admit_read_and_reject_write_before_permission() {
        let root = std::env::temp_dir().join(format!(
            "pd-managed-read-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        struct Scratch(std::path::PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let scratch = Scratch(root.clone());
        std::fs::write(root.join("sample.txt"), "hello\n").unwrap();
        let registry = ToolRegistry::default();
        let _read = registry
            .register(Arc::new(builtin::read_file::ReadFileTool::new()))
            .unwrap();
        let _write = registry
            .register(Arc::new(builtin::write_file::WriteFileTool::default()))
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let runtime = ToolRuntime::new(
            registry,
            Arc::new(Permission(calls.clone())),
            SchedulerMode::Sequential,
        )
        .with_lifecycle(Arc::new(ToolHooks::default()));
        let execution = runtime
            .for_managed_batch(Arc::new(Gate(Cancellation::default())))
            .unwrap();
        let mut fields = JsObject::new();
        fields.insert(
            "cwd".into(),
            JsValue::String(root.to_string_lossy().to_string().into()),
        );
        fields.insert("sessionId".into(), "session".into());
        fields.insert("turnId".into(), "turn".into());
        fields.insert("permissionMode".into(), "default".into());
        let host = Arc::new(ToolRuntimeContext {
            fields,
            files: HostToolCheckpoint::default().tool_context_state(),
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
        });
        let context = || ToolInvocationContext {
            runtime: host.clone(),
            execution: Arc::new(ExecutionContext {
                fields: JsObject::new(),
                abort_signal: None,
            }),
        };
        let write = ToolCall {
            id: "write".into(),
            name: "write_file".into(),
            input: JsValue::Object(JsObject::new()),
        };
        assert!(execution.execute_all(vec![write], context()).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let mut input = JsObject::new();
        input.insert("file_path".into(), "sample.txt".into());
        let read = ToolCall {
            id: "read".into(),
            name: "read_file".into(),
            input: JsValue::Object(input),
        };
        let result = execution.execute_all(vec![read], context()).await.unwrap();
        assert_eq!(result[0].get("type"), Some(&JsValue::from("success")));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        drop(scratch);
    }

    #[test]
    fn same_owner_mutation_invalidates_snapshot() {
        let hooks = Arc::new(ToolHooks::default());
        let snapshot = hooks.managed_empty_hooks().unwrap();
        assert!(snapshot.revalidate().is_ok());
        hooks.bind(None);
        assert!(snapshot.revalidate().is_err());
        assert!(hooks.managed_empty_hooks().unwrap().revalidate().is_ok());
    }

    #[test]
    fn configured_effect_fails_before_port_is_called() {
        let hooks = Arc::new(ToolHooks::default());
        hooks.events.lock().unwrap().push("PreToolUse".into());
        assert!(hooks.managed_empty_hooks().is_err());
    }
}
