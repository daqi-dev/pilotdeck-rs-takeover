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
            runtime::{AsyncHookRegistration, AsyncHookRegistrationHandler, HookExecutionEventHandler},
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
    mut settings: HooksSettings,
    sdk_config: Option<&JsValue>,
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
    let headers = hooks.get("headers").and_then(JsValue::as_object).map(|headers| {
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
        if !is_hook_event(event) || event.equals_str("ConfigChange") || event.equals_str("FileChanged")
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
    let mut set = |name: &str, value: String| {
        match pairs.iter().position(|(key, _)| key == name) {
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
        }
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
                if started { "hook_started" } else { "hook_response" }.into(),
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
            Some(registry) => registry.register(crate::runtime::async_hooks::AsyncHookRegistration {
                session_key: self.session.clone(),
                invocation_id: registration.invocation_id,
                hook_name: registration.hook_name,
                hook_event: registration.hook_event,
                timeout_ms: registration.timeout_ms,
                include_hook_events: self.include_hook_events,
            }),
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
}
impl ToolHooks {
    pub(super) fn configure(&self, settings: &HooksSettings) {
        *self.events.lock().unwrap_or_else(|e| e.into_inner()) = settings
            .iter()
            .filter(|(_, matchers)| !matchers.is_empty())
            .map(|(event, _)| event.clone())
            .collect();
    }
    pub(super) fn bind(&self, port: Option<Arc<dyn LifecyclePort>>) {
        *self.port.lock().unwrap_or_else(|e| e.into_inner()) = port;
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
        let field = |key: &str| context.fields.get(key).cloned().unwrap_or(JsValue::Undefined);
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
            env: context.fields.get("env").and_then(JsValue::as_object).cloned(),
            signal: None,
        };
        let future = port.dispatch(input);
        Box::pin(async move { future.await.map(Some) })
    }
}
