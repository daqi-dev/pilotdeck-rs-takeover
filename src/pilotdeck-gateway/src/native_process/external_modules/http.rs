//! Shared outbound module-http-v2 client (fixed PilotDeck cc7c4e6).
//!
//! Slot adapters own operation declarations and result projection. This client
//! owns the manifest flight, transport envelope, and response validation.
use crate::runtime::network::{Cancellation, node_timer_duration};
use crate::skills::{
    ModuleEnvironment, ModuleHttpRequest, ModuleHttpTransport, ModuleTransportError,
};
use pilotdeck_protocol::{
    JsObject, JsString, JsValue, MODULE_PROTOCOL_VERSION, parse_json, stringify_json,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::watch;

#[derive(Clone, Debug)]
pub struct ExternalHttpBinding {
    pub implementation_id: JsString,
    pub contract: String,
    pub transport: String,
    pub endpoint: String,
    pub manifest_path: String,
    pub call_path: String,
    pub methods: Vec<JsString>,
    pub timeout_ms: f64,
    pub credential_env: Option<String>,
}

impl ExternalHttpBinding {
    /// Re-read an already validated config snapshot; do not choose a slot here.
    pub fn from_value(value: &JsValue) -> Result<Self, HttpModuleError> {
        let object = value.as_object().ok_or_else(|| {
            HttpModuleError::protocol("External module binding must be an object.")
        })?;
        let protocol_text = |key: &str| object.get(key).and_then(JsValue::as_string).cloned();
        let text = |key: &str| {
            object
                .get(key)
                .and_then(JsValue::as_string)
                .map(|v| String::from_utf16_lossy(v.units()))
        };
        let required = |key: &str| {
            text(key).ok_or_else(|| {
                HttpModuleError::protocol(format!("External module binding is missing {key}."))
            })
        };
        Ok(Self {
            implementation_id: protocol_text("implementationId").ok_or_else(|| {
                HttpModuleError::protocol("External module binding is missing implementationId.")
            })?,
            contract: required("contract")?,
            transport: required("transport")?,
            endpoint: required("endpoint")?,
            manifest_path: text("manifestPath").unwrap_or_else(|| "/module-manifest".into()),
            call_path: text("callPath").unwrap_or_else(|| "/v2/module/call".into()),
            methods: match object.get("methods") {
                Some(JsValue::Array(methods)) => methods
                    .iter()
                    .filter_map(JsValue::as_string)
                    .cloned()
                    .collect(),
                _ => Vec::new(),
            },
            timeout_ms: object
                .get("timeoutMs")
                .and_then(JsValue::as_number)
                .unwrap_or(10_000.0),
            credential_env: text("credentialEnv"),
        })
    }

    pub fn supports(&self, method: &str) -> bool {
        self.methods.iter().any(|value| value.equals_str(method))
    }
}

#[derive(Clone, Debug)]
pub struct ModuleCallInput {
    pub run_id: JsString,
    pub operation_id: JsString,
    pub request_id: JsString,
    pub module: JsString,
    pub payload: JsValue,
    pub idempotency_key: Option<JsString>,
    /// Transport-only: never serialized to the remote module.
    pub abort: Option<Cancellation>,
}

/// Preserve provider fields and the complete envelope, including unknown fields.
/// Box the potentially large values so ordinary Result errors remain small.
#[derive(Clone, Debug, PartialEq)]
pub struct HttpModuleError {
    pub code: JsString,
    pub message: JsString,
    pub outcome: Option<JsString>,
    pub retryability: Option<JsString>,
    pub details: Option<Box<JsValue>>,
    pub structure: Box<JsValue>,
}

impl HttpModuleError {
    pub fn protocol(message: impl Into<JsString>) -> Self {
        Self::local("MODULE_PROTOCOL_INCOMPATIBLE", message.into(), None)
    }

    pub fn transport(code: &str, message: impl Into<JsString>) -> Self {
        Self::local(code, message.into(), Some("result_unknown".into()))
    }

    fn local(code: &str, message: JsString, outcome: Option<JsString>) -> Self {
        let mut structure = object([
            ("code", code.into()),
            ("message", JsValue::String(message.clone())),
        ]);
        if let Some(outcome) = &outcome {
            structure.insert("outcome".into(), JsValue::String(outcome.clone()));
        }
        Self {
            code: code.into(),
            message,
            outcome,
            retryability: None,
            details: None,
            structure: Box::new(JsValue::Object(structure)),
        }
    }

    /// Project a validated ok:false response without reducing it to text.
    pub fn from_response(response: &JsValue) -> Self {
        let error = response.get("error").and_then(JsValue::as_object);
        let code =
            nonempty(response.get("code")).unwrap_or_else(|| "MODULE_PROTOCOL_INCOMPATIBLE".into());
        let message = error
            .and_then(|e| nonempty(e.get("message")))
            .unwrap_or_else(|| code.clone());
        Self {
            code,
            message,
            outcome: response
                .get("outcome")
                .and_then(JsValue::as_string)
                .cloned(),
            retryability: error
                .and_then(|e| e.get("retryability"))
                .and_then(JsValue::as_string)
                .cloned(),
            details: error
                .and_then(|e| e.get("details"))
                .filter(|v| !v.is_undefined())
                .cloned()
                .map(Box::new),
            structure: Box::new(response.clone()),
        }
    }
}

impl std::fmt::Display for HttpModuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", String::from_utf16_lossy(self.message.units()))
    }
}
impl std::error::Error for HttpModuleError {}

type ModuleResult = Result<JsValue, HttpModuleError>;

#[derive(Clone)]
enum FlightResult {
    Pending,
    Finished(ModuleResult),
    Abandoned,
}

struct ManifestState {
    value: Option<JsValue>,
    flight: Option<Arc<watch::Sender<FlightResult>>>,
}

/// An owner can disappear when its containing turn/task is dropped. Publish a
/// terminal state before releasing the cache so joined callers can take over.
struct ManifestOwner<'a> {
    state: &'a Mutex<ManifestState>,
    flight: Arc<watch::Sender<FlightResult>>,
    finished: bool,
}
impl Drop for ManifestOwner<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.flight.send_replace(FlightResult::Abandoned);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .flight
            .as_ref()
            .is_some_and(|f| Arc::ptr_eq(f, &self.flight))
        {
            state.flight = None;
        }
    }
}

pub struct HttpModuleClient {
    binding: ExternalHttpBinding,
    transport: Arc<dyn ModuleHttpTransport>,
    env: Arc<dyn ModuleEnvironment>,
    sequence: AtomicU64,
    manifest: Mutex<ManifestState>,
}

impl HttpModuleClient {
    pub fn new(
        binding: ExternalHttpBinding,
        transport: Arc<dyn ModuleHttpTransport>,
        env: Arc<dyn ModuleEnvironment>,
    ) -> Self {
        Self {
            binding,
            transport,
            env,
            sequence: AtomicU64::new(0),
            manifest: Mutex::new(ManifestState {
                value: None,
                flight: None,
            }),
        }
    }

    pub fn binding(&self) -> &ExternalHttpBinding {
        &self.binding
    }

    pub async fn call(&self, input: ModuleCallInput) -> ModuleResult {
        // The baseline does not pass caller cancellation to the shared manifest.
        self.manifest().await?;
        let message_id = format!(
            "module-http-{}",
            self.sequence.fetch_add(1, Ordering::SeqCst) + 1
        );
        let authorization = match self
            .binding
            .credential_env
            .as_deref()
            .filter(|key| !key.is_empty())
        {
            Some(key) => {
                let value = self
                    .env
                    .var(key)
                    .map(|v| crate::metadata::js_trim(&v).to_owned())
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| {
                        HttpModuleError::protocol(
                            "Knowledge module account credential is not configured.",
                        )
                    })?;
                Some(format!("Bearer {value}"))
            }
            None => None,
        };
        let mut request = object([
            ("kind", "request".into()),
            ("method", "module_call".into()),
            ("messageId", message_id.as_str().into()),
            ("runId", JsValue::String(input.run_id)),
            ("operationId", JsValue::String(input.operation_id)),
            ("requestId", JsValue::String(input.request_id.clone())),
            ("module", JsValue::String(input.module)),
            ("payload", input.payload),
        ]);
        if let Some(key) = input.idempotency_key {
            request.insert("idempotencyKey".into(), JsValue::String(key));
        }
        let body = stringify_json(&JsValue::Object(request))
            .ok()
            .flatten()
            .ok_or_else(|| HttpModuleError::protocol("Module call request is not serializable."))?;
        let response = self
            .request_json(
                &self.binding.call_path,
                "POST",
                body.into_bytes(),
                authorization,
                true,
                input.abort.as_ref(),
            )
            .await?;
        validate_response(&response, &message_id, &input.request_id)?;
        Ok(response)
    }

    pub async fn manifest(&self) -> ModuleResult {
        loop {
            let (flight, owner) = {
                let mut state = self.manifest.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(value) = &state.value {
                    return Ok(value.clone());
                }
                if let Some(flight) = &state.flight {
                    (flight.clone(), false)
                } else {
                    let flight = Arc::new(watch::channel(FlightResult::Pending).0);
                    state.flight = Some(flight.clone());
                    (flight, true)
                }
            };
            if owner {
                let mut guard = ManifestOwner {
                    state: &self.manifest,
                    flight: flight.clone(),
                    finished: false,
                };
                let result = self.load_manifest().await;
                {
                    let mut state = self.manifest.lock().unwrap_or_else(|e| e.into_inner());
                    if let Ok(value) = &result {
                        state.value = Some(value.clone());
                    }
                    state.flight = None;
                    flight.send_replace(FlightResult::Finished(result.clone()));
                    guard.finished = true;
                }
                return result;
            }
            let mut receiver = flight.subscribe();
            loop {
                let outcome = receiver.borrow_and_update().clone();
                match outcome {
                    FlightResult::Finished(result) => return result,
                    FlightResult::Abandoned => break,
                    FlightResult::Pending => {
                        if receiver.changed().await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    }

    async fn load_manifest(&self) -> ModuleResult {
        let value = self
            .request_json(
                &self.binding.manifest_path,
                "GET",
                Vec::new(),
                None,
                false,
                None,
            )
            .await?;
        let manifest = value
            .as_object()
            .ok_or_else(|| HttpModuleError::protocol("Module manifest must be an object."))?;
        let text_is = |key: &str, expected: &str| {
            manifest
                .get(key)
                .and_then(JsValue::as_string)
                .is_some_and(|v| v.equals_str(expected))
        };
        let methods = match manifest.get("methods") {
            Some(JsValue::Array(methods)) => Some(methods),
            _ => None,
        };
        if !text_is("protocolVersion", MODULE_PROTOCOL_VERSION)
            || manifest
                .get("implementationId")
                .and_then(JsValue::as_string)
                != Some(&self.binding.implementation_id)
            || !text_is("contract", &self.binding.contract)
            || !text_is("transport", &self.binding.transport)
            || !methods.is_some_and(|m| m.iter().all(|v| v.as_string().is_some()))
        {
            return Err(HttpModuleError::protocol(
                "Module manifest identity, protocol, contract, transport, or methods are incompatible.",
            ));
        }
        let methods = methods.expect("validated manifest methods");
        if let Some(missing) = self
            .binding
            .methods
            .iter()
            .find(|method| !methods.iter().any(|v| v.as_string() == Some(method)))
        {
            let mut message: Vec<u16> = "Module manifest does not provide required method '"
                .encode_utf16()
                .collect();
            message.extend_from_slice(missing.units());
            message.extend("'.".encode_utf16());
            return Err(HttpModuleError::protocol(JsString::from_utf16(message)));
        }
        // Return the same normalized manifest shape as TS, without extra fields.
        Ok(JsValue::Object(object([
            ("protocolVersion", MODULE_PROTOCOL_VERSION.into()),
            (
                "implementationId",
                JsValue::String(self.binding.implementation_id.clone()),
            ),
            ("contract", self.binding.contract.as_str().into()),
            ("transport", self.binding.transport.as_str().into()),
            ("methods", JsValue::Array(methods.clone())),
        ])))
    }

    async fn request_json(
        &self,
        path: &str,
        method: &'static str,
        body: Vec<u8>,
        authorization: Option<String>,
        accept_failure: bool,
        abort: Option<&Cancellation>,
    ) -> ModuleResult {
        if abort.is_some_and(Cancellation::is_aborted) {
            return Err(HttpModuleError::transport(
                "MODULE_ABORTED",
                "Module request was cancelled before dispatch.",
            ));
        }
        let base = if self.binding.endpoint.ends_with('/') {
            self.binding.endpoint.clone()
        } else {
            format!("{}/", self.binding.endpoint)
        };
        let url = url::Url::parse(&base)
            .and_then(|base| base.join(path))
            .map_err(|_| {
                HttpModuleError::transport(
                    "MODULE_TRANSPORT_UNAVAILABLE",
                    "Module HTTP request could not be completed.",
                )
            })?;
        let timeout = node_timer_duration(self.binding.timeout_ms);
        let request = ModuleHttpRequest {
            method,
            url: url.to_string(),
            content_type: (method == "POST").then_some("application/json"),
            authorization,
            body,
            timeout,
        };
        let deadline = tokio::time::Instant::now() + timeout;
        let transport = self.transport.clone();
        let send =
            crate::runtime::network::ExchangeFuture::io(
                async move { transport.send(request).await },
            );
        let response = match crate::runtime::network::exchange(
            send,
            async {
                match abort {
                    Some(abort) => abort.cancelled().await,
                    None => std::future::pending().await,
                }
            },
            || abort.and_then(Cancellation::abort_event_at),
            Some(deadline),
        )
        .await
        {
            crate::runtime::network::Exchange::Response(response, _) => {
                response.map_err(|e| self.transport_error(e))?
            }
            crate::runtime::network::Exchange::Aborted => {
                return Err(HttpModuleError::transport(
                    "MODULE_ABORTED",
                    "Module request was cancelled.",
                ));
            }
            crate::runtime::network::Exchange::Timeout { .. } => {
                return Err(self.transport_error(ModuleTransportError::Timeout));
            }
        };
        let text = String::from_utf8_lossy(&response.body);
        let parsed = parse_json(text.strip_prefix('\u{feff}').unwrap_or(&text)).ok();
        let structured_failure = accept_failure
            && parsed.as_ref().is_some_and(|v| {
                v.get("kind")
                    .and_then(JsValue::as_string)
                    .is_some_and(|k| k.equals_str("response"))
                    && v.get("ok") == Some(&JsValue::Bool(false))
            });
        if !(200..300).contains(&response.status) && !structured_failure {
            return Err(self.transport_error(ModuleTransportError::Unavailable));
        }
        parsed.ok_or_else(|| HttpModuleError::protocol("Module HTTP response must contain JSON."))
    }

    fn transport_error(&self, error: ModuleTransportError) -> HttpModuleError {
        match error {
            ModuleTransportError::Timeout => HttpModuleError::transport(
                "MODULE_TIMEOUT",
                format!(
                    "Module HTTP request exceeded {}ms.",
                    crate::js_value::number_string(self.binding.timeout_ms)
                ),
            ),
            ModuleTransportError::Unavailable => HttpModuleError::transport(
                "MODULE_TRANSPORT_UNAVAILABLE",
                "Module HTTP request could not be completed.",
            ),
        }
    }
}

fn validate_response(
    value: &JsValue,
    message_id: &str,
    request_id: &JsString,
) -> Result<(), HttpModuleError> {
    pilotdeck_protocol::module::validate_module_message(value)
        .map_err(|e| HttpModuleError::protocol(e.message))?;
    if !value
        .get("kind")
        .and_then(JsValue::as_string)
        .is_some_and(|v| v.equals_str("response"))
    {
        return Err(HttpModuleError::protocol(
            "Module call did not return a response envelope.",
        ));
    }
    if value.get("ok") == Some(&JsValue::Bool(true)) {
        if value.get("code").is_some_and(|v| !v.is_undefined())
            || value.get("error").is_some_and(|v| !v.is_undefined())
            || value
                .get("outcome")
                .and_then(JsValue::as_string)
                .is_some_and(|v| v.equals_str("failed"))
        {
            return Err(HttpModuleError::protocol(
                "Successful module response contains failure fields.",
            ));
        }
    } else {
        let code = nonempty(value.get("code"));
        let error = value.get("error").and_then(JsValue::as_object);
        let (Some(code), Some(error)) = (code, error) else {
            return Err(HttpModuleError::protocol(
                "Failed module response requires code and error details.",
            ));
        };
        let retryability = error
            .get("retryability")
            .and_then(|v| crate::js_value::to_string_units_checked(v).ok())
            .map(JsString::from_utf16);
        if error.get("code").and_then(JsValue::as_string) != Some(&code)
            || nonempty(error.get("message")).is_none()
            || !retryability.is_some_and(|v| {
                ["safe", "unsafe", "retry_after_status"]
                    .iter()
                    .any(|s| v.equals_str(s))
            })
        {
            return Err(HttpModuleError::protocol(
                "Failed module response error fields are inconsistent.",
            ));
        }
    }
    if !value
        .get("inReplyTo")
        .and_then(JsValue::as_string)
        .is_some_and(|v| v.equals_str(message_id))
        || value
            .get("requestId")
            .is_some_and(|v| !v.is_undefined() && v.as_string() != Some(request_id))
    {
        return Err(HttpModuleError::protocol(
            "Module response correlation does not match the request.",
        ));
    }
    Ok(())
}

fn nonempty(value: Option<&JsValue>) -> Option<JsString> {
    value
        .and_then(JsValue::as_string)
        .filter(|v| !v.is_empty())
        .cloned()
}
fn object<const N: usize>(fields: [(&str, JsValue); N]) -> JsObject {
    let mut object = JsObject::new();
    for (key, value) in fields {
        object.insert(key.into(), value);
    }
    object
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod tests;
