//! U7 global settings methods wired into the native request router (#1320).
//! TS baseline: `GatewayWsConnection.ts:405-425, 523-543` (the WS dispatch
//! of the six methods), `InProcessGateway.ts:2337-2362, 2387-2390,
//! 2905-2928` (the wrapper contracts).
//!
//! The `SettingsGateway` component (#1318) owns the method-level contracts:
//! the per-method capability gates (the four rejecting methods throw
//! `CAPABILITY_UNAVAILABLE` `"<method> is unavailable."`; the two reloads
//! project `{reloaded:false, reason:"unsupported"}`), the field validation,
//! the `hasActiveTurn` SESSION_BUSY gate, the post-success session close and
//! the createLocalGateway `updateSettings` closure. This family is the thin
//! adapter that routes the six wire names into that component and maps
//! `SettingsError` to the WS error frame's `RequestError` projection
//! (`code: None` is a raw JS throw, which the WS layer answers
//! `gateway_request_failed` for, like the MCP slice's adapter).
//!
//! The native composition (`services::native_settings`) binds
//! `resolve_settings` over the real config store, the two reloads over the
//! real refresh bundle and `apply_flag_settings` over the session selection
//! store. `reload_output_styles` needs the session registry and the project
//! runtimes the request router owns, so this family answers it directly
//! (`Requests::reload_output_styles`).
use super::super::requests::Requests;
use crate::runtime::{
    connection::{Call, RequestError},
    settings::SettingsError,
};
use pilotdeck_protocol::{JsString, JsValue};

// The single source is the component's list (`runtime/settings/gateway.rs`);
// #1320 fix-r2 F10: re-export instead of hand-copying, so the registry table
// and the dispatch arms cannot drift apart.
pub(crate) const METHODS: &[&str] = crate::runtime::settings::METHODS;

pub(super) const HANDLER: super::Handler =
    super::Handler::AfterAdmission(Requests::settings_request);

fn settings_error(error: SettingsError) -> RequestError {
    RequestError {
        code: error
            .code()
            .map(|code| String::from_utf16_lossy(code.units()))
            .unwrap_or_else(|| "gateway_request_failed".into()),
        message: error.message().clone(),
        details: None,
        validation: None,
    }
}

impl Requests {
    pub(in crate::native_process) fn settings_request(
        &self,
        method: String,
        params: JsValue,
    ) -> Call<'_, Result<JsValue, RequestError>> {
        Box::pin(async move {
            if method == "reload_output_styles" {
                return self.reload_output_styles(&params).await;
            }
            let gateway = &self.settings;
            match gateway.dispatch(&method, &params).await {
                Some(result) => result.map_err(settings_error),
                // The registry selected this family, so the wire name is one
                // of the six the component dispatches; if the two lists ever
                // drift apart, answer the caller with the WS layer's own
                // unknown-method projection instead of panicking the process
                // (#1320 fix-r1 F5).
                None => Err(RequestError {
                    code: "gateway_request_failed".into(),
                    message: JsString::from(format!("unknown settings method: {method}")),
                    details: None,
                    validation: None,
                }),
            }
        })
    }
}

impl Requests {
    /// `registry.reloadOutputStylesForSdk(input)`
    /// (ProjectRuntimeRegistry.ts:1111-1121; `input = {}` when the frame has
    /// no params, InProcessGateway.ts:2387): resolve the runtime, reload its
    /// output-style registry, then close every session holding an SDK
    /// session config in that project (all of them without a `projectKey`)
    /// with `sdk_output_styles_reloaded`.
    async fn reload_output_styles(&self, params: &JsValue) -> Result<JsValue, RequestError> {
        let empty = JsValue::Object(Default::default());
        let params = if params.is_undefined() { &empty } else { params };
        let project_root = self.route_project(params)?;
        let project = self.project.clone();
        let acquire_root = project_root.clone();
        let lease = tokio::task::spawn_blocking(move || project.acquire(Some(&acquire_root)))
            .await
            .map_err(|e| RequestError::new(e.to_string()))?
            .map_err(|e| RequestError::new(e.to_string()))?;
        let resources = lease
            .runtime()
            .resources()
            .get::<crate::runtime::project::ProjectResources>()
            .ok_or_else(|| RequestError::new("Native project resources are missing."))?;
        let plugin_resource = resources
            .context()
            .resource(crate::runtime::project::Resource::PluginRuntime)
            .ok_or_else(|| RequestError::new("Native plugin runtime resource is missing."))?;
        let plugin_runtime = plugin_resource
            .as_any()
            .downcast_ref::<crate::runtime::project::plugins::PluginResources>()
            .ok_or_else(|| RequestError::new("Native plugin runtime has the wrong type."))?
            .runtime
            .clone();
        let changed = plugin_runtime.reload_output_styles().await;
        drop(lease);
        let scoped = matches!(params.get("projectKey"), Some(JsValue::String(key)) if !key.is_empty());
        let reason: JsString = "sdk_output_styles_reloaded".into();
        for (session, root) in self.session_overrides.sdk_config_sessions() {
            if scoped && root != project_root {
                continue;
            }
            // `this.gateway?.closeSession(...)` (InProcessGateway.ts:2049-2054).
            self.owner
                .close(&session)
                .await
                .map_err(|e| RequestError::new(e.to_string()))?;
            self.interactions.close_session(&session, &reason);
            self.session_overrides.clear_live_mode(&session);
        }
        let mut result = pilotdeck_protocol::JsObject::new();
        result.insert("reloaded".into(), JsValue::Bool(true));
        result.insert(
            "changed".into(),
            JsValue::Array(
                changed
                    .into_iter()
                    .map(|name| JsValue::String(name.into()))
                    .collect(),
            ),
        );
        Ok(JsValue::Object(result))
    }
}
