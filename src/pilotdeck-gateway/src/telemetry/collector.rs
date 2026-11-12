use super::{
    BoxFuture, CATEGORIES, KINDS, MODULES, STAGES, TelemetryClient, TelemetryConfig, TelemetryEnv,
    TelemetryErrorValue, TelemetryRuntimeContext, TelemetrySender, defaulted, field,
    hash_telemetry_id, is_one, lock, object, spread, string, truthy,
};
use crate::runtime::model::http::ModelHttpTransport;
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub struct TelemetryCollectorOptions {
    pub env: TelemetryEnv,
    pub pilot_home: PathBuf,
    pub runtime_context: TelemetryRuntimeContext,
    pub enabled: Option<bool>,
    pub transport: Option<Arc<dyn ModelHttpTransport>>,
}
impl TelemetryCollectorOptions {
    pub fn new(
        env: TelemetryEnv,
        pilot_home: PathBuf,
        runtime_context: TelemetryRuntimeContext,
    ) -> Self {
        Self {
            env,
            pilot_home,
            runtime_context,
            enabled: None,
            transport: None,
        }
    }
}
pub struct TelemetryCollector {
    config: Arc<Mutex<TelemetryConfig>>,
    runtime_context: TelemetryRuntimeContext,
    sender: TelemetrySender,
}
impl TelemetryCollector {
    pub fn new(options: TelemetryCollectorOptions) -> Arc<Self> {
        let mut config = TelemetryConfig::from_env(&options.env, &options.pilot_home);
        if let Some(enabled) = options.enabled {
            config.enabled = enabled;
        }
        let config = Arc::new(Mutex::new(config));
        let sender = TelemetrySender::shared(config.clone(), options.transport);
        Arc::new(Self {
            config,
            runtime_context: options.runtime_context,
            sender,
        })
    }
    fn build_event(&self, name: &JsString, properties: &JsValue, context: &JsValue) -> JsValue {
        let runtime = &self.runtime_context;
        let session = field(context, "sessionId");
        object([
            ("schemaVersion", string("analytics.v2")),
            ("eventId", string(&uuid::Uuid::new_v4().to_string())),
            ("eventName", JsValue::String(name.clone())),
            (
                "occurredAt",
                string(&crate::runtime::context::prompt::iso_timestamp_now()),
            ),
            (
                "installationId",
                JsValue::String(runtime.installation_id.clone()),
            ),
            ("instanceId", JsValue::String(runtime.instance_id.clone())),
            (
                "deploymentMode",
                JsValue::String(runtime.deployment_mode.clone()),
            ),
            (
                "sessionId",
                if truthy(session) {
                    session
                        .as_string()
                        .map(|v| JsValue::String(hash_telemetry_id(v)))
                        .unwrap_or(JsValue::Undefined)
                } else {
                    JsValue::Undefined
                },
            ),
            ("commitHash", JsValue::String(runtime.commit_hash.clone())),
            ("appVersion", JsValue::String(runtime.app_version.clone())),
            ("platform", JsValue::String(runtime.platform.clone())),
            ("properties", sanitize_properties(properties)),
        ])
    }
}
impl TelemetryClient for TelemetryCollector {
    fn track(&self, event_name: &JsString, properties: &JsValue, context: &JsValue) {
        let enabled = lock(&self.config).enabled;
        if enabled {
            self.sender
                .enqueue(self.build_event(event_name, properties, context));
        }
    }
    fn track_feature_used(&self, input: &JsValue) {
        TelemetryClient::track_feature_loop_stage(self, input);
    }
    fn track_feature_loop_stage(&self, input: &JsValue) {
        let module = field(input, "module");
        let mut props = JsObject::new();
        props.insert("module".into(), module.clone());
        props.insert(
            "ownerModule".into(),
            if super::nullish(field(input, "ownerModule")) {
                module.clone()
            } else {
                field(input, "ownerModule").clone()
            },
        );
        for key in ["executionKind", "phase"] {
            if truthy(field(input, key)) {
                props.insert(key.into(), field(input, key).clone());
            }
        }
        props.insert("loopStage".into(), field(input, "loopStage").clone());
        props.insert(
            "outcome".into(),
            if field(input, "outcome").is_undefined() {
                string("success")
            } else {
                field(input, "outcome").clone()
            },
        );
        if truthy(field(input, "errorCategory")) {
            props.insert(
                "errorCategory".into(),
                field(input, "errorCategory").clone(),
            );
        }
        spread(&mut props, &sanitize_properties(field(input, "metadata")));
        self.track(&"feature_used".into(), &JsValue::Object(props), input);
    }
    fn track_error(&self, error: &TelemetryErrorValue, input: &JsValue) {
        let code = if super::nullish(field(input, "code")) {
            match error {
                TelemetryErrorValue::ErrorName(name) => {
                    if name.is_empty() {
                        string("Error")
                    } else {
                        JsValue::String(name.clone())
                    }
                }
                TelemetryErrorValue::Value(_) => string("UnknownError"),
            }
        } else {
            field(input, "code").clone()
        };
        let module = defaulted(field(input, "module"), "runtime");
        let owner = if super::nullish(field(input, "ownerModule")) {
            module.clone()
        } else {
            field(input, "ownerModule").clone()
        };
        let stage = defaulted(field(input, "loopStage"), "loop_end");
        let category = defaulted(field(input, "errorCategory"), "runtime_error");
        let mut props = JsObject::new();
        props.insert("module".into(), module.clone());
        props.insert("ownerModule".into(), owner.clone());
        for key in ["executionKind", "phase"] {
            if truthy(field(input, key)) {
                props.insert(key.into(), field(input, key).clone());
            }
        }
        props.insert("loopStage".into(), stage.clone());
        props.insert("errorCategory".into(), category.clone());
        props.insert("code".into(), code.clone());
        if truthy(field(input, "toolName")) {
            props.insert("toolName".into(), field(input, "toolName").clone());
        }
        self.track(&"error_occurred".into(), &JsValue::Object(props), input);
        if !is_one(&module, MODULES) || !is_one(&owner, MODULES) {
            return;
        }
        let mut metadata = JsObject::new();
        metadata.insert("code".into(), code);
        if truthy(field(input, "toolName")) {
            metadata.insert("toolName".into(), field(input, "toolName").clone());
        }
        for key in ["provider", "model"] {
            let v = field(field(input, "metadata"), key);
            if v.as_string().is_some() {
                metadata.insert(key.into(), v.clone());
            }
        }
        let url = field(field(input, "metadata"), "providerBaseUrl");
        if url.as_string().is_some() {
            let normalized = crate::runtime::model::providers::normalize_provider_base_url(url);
            if truthy(&normalized) {
                metadata.insert("providerBaseUrl".into(), normalized);
            }
        }
        TelemetryClient::track_feature_loop_stage(
            self,
            &object([
                ("module", module),
                ("ownerModule", owner),
                (
                    "executionKind",
                    if is_one(field(input, "executionKind"), KINDS) {
                        field(input, "executionKind").clone()
                    } else {
                        JsValue::Undefined
                    },
                ),
                ("phase", field(input, "phase").clone()),
                (
                    "loopStage",
                    if is_one(&stage, STAGES) {
                        stage
                    } else {
                        string("loop_end")
                    },
                ),
                ("outcome", string("failed")),
                (
                    "errorCategory",
                    if is_one(&category, CATEGORIES) {
                        category
                    } else {
                        string("runtime_error")
                    },
                ),
                ("sessionId", field(input, "sessionId").clone()),
                ("metadata", sanitize_properties(&JsValue::Object(metadata))),
            ]),
        );
    }
    fn set_enabled(&self, enabled: bool) {
        // Fixed collector and sender initially share this object. Preserve the
        // measured equality early-return (including its timer behavior).
        lock(&self.config).enabled = enabled;
        self.sender.set_enabled(enabled);
    }
    fn flush(&self) -> BoxFuture<()> {
        self.sender.flush()
    }
    fn shutdown(&self) -> BoxFuture<()> {
        self.sender.shutdown()
    }
    fn snapshot(&self) -> JsValue {
        self.sender.snapshot()
    }
    fn get_config(&self) -> JsValue {
        lock(&self.config).to_value()
    }
}
impl crate::runtime::memory::maintenance::MaintenanceTelemetry for TelemetryCollector {
    fn track_feature_loop_stage(&self, input: JsValue) {
        TelemetryClient::track_feature_loop_stage(self, &input);
    }
    fn track_error(&self, error: &crate::runtime::memory::MemoryProviderError, input: JsValue) {
        TelemetryClient::track_error(
            self,
            &TelemetryErrorValue::ErrorName(error.name.clone().into()),
            &input,
        );
    }
}

pub fn sanitize_properties(value: &JsValue) -> JsValue {
    let mut out = JsObject::new();
    for (key, value) in entries(value) {
        let key_text = String::from_utf16_lossy(key.units()).to_ascii_lowercase();
        if ["path", "cwd", "root", "dir", "file"]
            .iter()
            .any(|part| key_text.contains(part))
        {
            continue;
        }
        let value = sanitize_value(&value);
        if !value.is_undefined() {
            if key.equals_str("__proto__") {
                // TS assigns into {}, invoking Object.prototype's setter.
                // Object spread elsewhere still defines ordinary own keys.
                if matches!(
                    value,
                    JsValue::Null | JsValue::Object(_) | JsValue::Array(_)
                ) {
                    out.set_prototype(value);
                }
            } else {
                out.insert(key, value);
            }
        }
    }
    JsValue::Object(out)
}
fn entries(value: &JsValue) -> Vec<(JsString, JsValue)> {
    match value {
        JsValue::Object(o) => o
            .own_entries()
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        JsValue::Array(a) => a
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string().into(), v.clone()))
            .collect(),
        JsValue::String(s) => s
            .units()
            .iter()
            .enumerate()
            .map(|(i, u)| {
                (
                    i.to_string().into(),
                    JsValue::String(JsString::from_utf16(vec![*u])),
                )
            })
            .collect(),
        _ => Vec::new(),
    }
}
fn sanitize_value(value: &JsValue) -> JsValue {
    match value {
        JsValue::String(s) => {
            let text = String::from_utf16_lossy(s.units());
            let t = crate::metadata::js_trim(&text).as_bytes();
            let absolute = t.first().is_some_and(|c| matches!(c, b'/' | b'\\'))
                || (t.len() >= 3
                    && t[0].is_ascii_alphabetic()
                    && t[1] == b':'
                    && matches!(t[2], b'/' | b'\\'));
            if absolute {
                JsValue::Undefined
            } else {
                value.clone()
            }
        }
        JsValue::Array(items) => {
            let values: Vec<_> = items
                .iter()
                .map(sanitize_value)
                .filter(|v| !v.is_undefined())
                .collect();
            if values.is_empty() {
                JsValue::Undefined
            } else {
                JsValue::Array(values)
            }
        }
        JsValue::Object(_) => {
            let nested = sanitize_properties(value);
            if nested.as_object().is_some_and(|o| o.is_empty()) {
                JsValue::Undefined
            } else {
                nested
            }
        }
        _ => value.clone(),
    }
}
