//! Native analytics.v2 collector, queue transport, and live Gateway observers.
//! Fixed source: PilotDeck cc7c4e6 `src/telemetry` and Gateway telemetry ports.
mod bundle;
mod collector;
mod context;
mod observer;
mod registry;
mod sender;

pub use bundle::{GatewayTelemetryBundle, GatewayTelemetryBundleOptions};
pub use collector::{TelemetryCollector, TelemetryCollectorOptions, sanitize_properties};
pub use context::{
    RuntimeContextOptions, TelemetryRuntimeContext, hash_telemetry_id, resolve_runtime_context,
};
pub use observer::{
    GatewayAgentEventTelemetryObserver, GatewayTelemetryContext, TurnTelemetryContext,
    resolve_turn_context,
};
pub use registry::{
    ObserverDisposer, ObserverFailureHandler, ObservingTelemetryClient, TelemetryObservation,
    TelemetryObserver, TelemetryObserverRegistry, TelemetryObserverResult, TelemetryRegistration,
    TelemetryReplacement,
};
pub use sender::{TelemetryConfig, TelemetrySender};

use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{Mutex, MutexGuard},
};

pub type TelemetryEnv = BTreeMap<String, String>;
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TelemetryFailure(pub String);
impl std::fmt::Display for TelemetryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for TelemetryFailure {}

#[derive(Clone, Debug)]
pub enum TelemetryErrorValue {
    /// JavaScript Error instances contribute only their name, never message.
    ErrorName(JsString),
    Value(JsValue),
}

pub trait TelemetryClient: Send + Sync {
    fn track(&self, event_name: &JsString, properties: &JsValue, context: &JsValue);
    fn track_feature_used(&self, input: &JsValue);
    fn track_feature_loop_stage(&self, input: &JsValue);
    fn track_error(&self, error: &TelemetryErrorValue, input: &JsValue);
    fn set_enabled(&self, enabled: bool);
    fn flush(&self) -> BoxFuture<()>;
    fn shutdown(&self) -> BoxFuture<()>;
    fn snapshot(&self) -> JsValue;
    fn get_config(&self) -> JsValue;
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}
static UNDEFINED: JsValue = JsValue::Undefined;
fn field<'a>(value: &'a JsValue, key: &str) -> &'a JsValue {
    value.get(key).unwrap_or(&UNDEFINED)
}
fn nullish(value: &JsValue) -> bool {
    matches!(value, JsValue::Undefined | JsValue::Null)
}
fn truthy(value: &JsValue) -> bool {
    match value {
        JsValue::Undefined | JsValue::Null => false,
        JsValue::Bool(b) => *b,
        JsValue::Number(n) => *n != 0.0 && !n.is_nan(),
        JsValue::String(s) => !s.is_empty(),
        _ => true,
    }
}
fn string(value: &str) -> JsValue {
    JsValue::String(value.into())
}
fn object<const N: usize>(fields: [(&str, JsValue); N]) -> JsValue {
    let mut out = JsObject::new();
    for (key, value) in fields {
        out.insert(key.into(), value);
    }
    JsValue::Object(out)
}
fn spread(target: &mut JsObject, value: &JsValue) {
    if let Some(source) = value.as_object() {
        for (key, value) in source.own_entries() {
            target.insert(key.clone(), value.clone());
        }
    }
}
fn defaulted(value: &JsValue, fallback: &str) -> JsValue {
    if nullish(value) {
        string(fallback)
    } else {
        value.clone()
    }
}
fn is_one(value: &JsValue, values: &[&str]) -> bool {
    value
        .as_string()
        .is_some_and(|s| values.iter().any(|v| s.equals_str(v)))
}
const MODULES: &[&str] = &["router", "always_on", "memory", "cron_job", "session"];
const KINDS: &[&str] = &[
    "user_session",
    "subagent",
    "always_on",
    "router_judge",
    "memory",
    "cron_job",
    "compaction",
    "tool_secondary",
];
const STAGES: &[&str] = &[
    "module_event",
    "loop_start",
    "model_request",
    "model_response",
    "tool_prepare",
    "tool_call",
    "permission_check",
    "loop_end",
];
const CATEGORIES: &[&str] = &[
    "model_request_error",
    "permission_error",
    "tool_param_error",
    "tool_runtime_error",
    "tool_result_parse_error",
    "loop_error",
    "runtime_error",
];

#[cfg(test)]
mod tests;

/// A cloneable explicit collector port carried through native assembly options.
#[derive(Clone)]
pub struct NativeTelemetry(pub std::sync::Arc<dyn TelemetryClient>);
impl std::fmt::Debug for NativeTelemetry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeTelemetry").finish_non_exhaustive()
    }
}

impl crate::runtime::memory::maintenance::MaintenanceTelemetry for NativeTelemetry {
    fn track_feature_loop_stage(&self, input: JsValue) {
        self.0.track_feature_loop_stage(&input);
    }
    fn track_error(&self, error: &crate::runtime::memory::MemoryProviderError, input: JsValue) {
        self.0.track_error(
            &TelemetryErrorValue::ErrorName(error.name.clone().into()),
            &input,
        );
    }
}

/// pilotdeck.ts:138-141: only telemetry path changes update the server base.
pub fn subscribe_enabled_reload(
    store: &crate::config::store::ConfigStore,
    client: std::sync::Arc<dyn TelemetryClient>,
) -> crate::config::store::Subscription {
    store.subscribe(std::sync::Arc::new(move |event| {
        let prefix: Vec<u16> = "telemetry.".encode_utf16().collect();
        if event
            .changed_paths
            .iter()
            .any(|path| path.equals_str("telemetry") || path.units().starts_with(&prefix))
        {
            let enabled = event
                .next_snapshot
                .get("config")
                .and_then(|config| config.get("telemetry"))
                .and_then(|telemetry| telemetry.get("enabled"))
                .and_then(JsValue::as_bool)
                == Some(true);
            client.set_enabled(enabled);
        }
    }))
}
