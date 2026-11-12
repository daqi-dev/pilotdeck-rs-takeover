use super::*;
use crate::{
    channel::http_client::{HttpClient, UNDICI_DEFAULT_TIMEOUT},
    metadata::object,
    runtime::network::Cancellation,
};
use std::sync::Arc;

/// A client is retained by the session, so one verified manifest serves all
/// nodes. Failed checks are not cached, and cancellation drops the request.
#[derive(Clone)]
pub(super) struct Client {
    endpoint: String,
    profile: JsValue,
    http: HttpClient,
    manifest: Arc<tokio::sync::Mutex<bool>>,
}
impl Client {
    pub(super) fn new(profile: JsValue) -> Result<Self, SopError> {
        let endpoint = text(&profile, "endpoint").ok_or_else(|| {
            SopError::new(
                "SOP_RUNTIME_PROTOCOL",
                "StaffDeck SOP runtime endpoint is required.",
            )
        })?;
        Ok(Self {
            endpoint: endpoint.trim_end_matches('/').into(),
            profile,
            http: HttpClient::new(),
            manifest: Arc::default(),
        })
    }
    pub(super) async fn prepare(
        &self,
        bundle: JsValue,
        state: JsValue,
        context: JsObject,
        signal: &Cancellation,
    ) -> Result<JsValue, SopError> {
        let payload = self
            .post(
                "prepare",
                object([("bundle", bundle), ("state", state)]),
                context,
                signal,
            )
            .await?;
        if !record(&payload, "state") || !payload.get("step").is_some_and(valid_step) {
            return Err(
                protocol("StaffDeck SOP runtime returned an invalid prepare payload.")
                    .retry("unsafe"),
            );
        }
        Ok(payload)
    }
    pub(super) async fn submit(
        &self,
        payload: JsValue,
        context: JsObject,
        signal: &Cancellation,
    ) -> Result<JsValue, SopError> {
        let payload = self.post("submit", payload, context, signal).await?;
        if !record(&payload, "state") || !payload.get("result").is_some_and(valid_result) {
            return Err(
                protocol("StaffDeck SOP runtime returned an invalid submit payload.")
                    .retry("unsafe"),
            );
        }
        Ok(payload)
    }
    async fn post(
        &self,
        operation: &str,
        payload: JsValue,
        mut context: JsObject,
        signal: &Cancellation,
    ) -> Result<JsValue, SopError> {
        self.check_manifest(signal).await?;
        context.insert("protocolVersion".into(), "2.0".into());
        context.insert("payload".into(), payload);
        let request = JsValue::Object(context);
        let (status, response) = self
            .request(&format!("/v1/sop/{operation}"), Some(&request), signal)
            .await?;
        if response.as_object().is_none()
            || !is(&response, "protocolVersion", "2.0")
            || response.get("requestId") != request.get("requestId")
            || !matches!(response.get("ok"), Some(JsValue::Bool(_)))
            || !(is(&response, "outcome", "completed") || is(&response, "outcome", "failed"))
        {
            return Err(protocol(
                "StaffDeck SOP runtime returned an invalid protocol envelope.",
            ));
        }
        if response.get("ok") == Some(&JsValue::Bool(true)) {
            if !is(&response, "outcome", "completed") || !record(&response, "payload") {
                return Err(protocol(
                    "StaffDeck SOP runtime returned an invalid success envelope.",
                ));
            }
            if !(200..300).contains(&status) {
                return Err(protocol(format!(
                    "StaffDeck SOP runtime returned HTTP {status} with a successful envelope."
                )));
            }
            return Ok(response.get("payload").unwrap().clone());
        }
        let absent = JsValue::Undefined;
        let error = response.get("error").unwrap_or(&absent);
        if !is(&response, "outcome", "failed") || !valid_error(error) {
            return Err(protocol(
                "StaffDeck SOP runtime returned an invalid error envelope.",
            ));
        }
        Err(SopError {
            code: text(error, "code").unwrap(),
            message: text(error, "message").unwrap(),
            details: error.get("details").cloned().map(Box::new),
            retryability: text(error, "retryability"),
        })
    }
    async fn check_manifest(&self, signal: &Cancellation) -> Result<(), SopError> {
        let mut checked = tokio::select! { biased; _ = signal.cancelled() => return Err(cancelled()), guard = self.manifest.lock() => guard };
        if *checked {
            return Ok(());
        }
        let path = text(&self.profile, "manifestPath").unwrap_or_else(|| "/healthz".into());
        let (status, value) = self.request(&path, None, signal).await?;
        if !(200..300).contains(&status) {
            return Err(SopError::new(
                "SOP_RUNTIME_UNAVAILABLE",
                format!("StaffDeck SOP runtime health check returned HTTP {status}."),
            ));
        }
        validate_manifest(&value, &self.profile)?;
        *checked = true;
        Ok(())
    }
    async fn request(
        &self,
        path: &str,
        body: Option<&JsValue>,
        signal: &Cancellation,
    ) -> Result<(u16, JsValue), SopError> {
        let encoded = body
            .map(pilotdeck_protocol::stringify_json)
            .transpose()
            .map_err(|_| protocol("StaffDeck SOP request cannot be serialized."))?
            .flatten()
            .unwrap_or_default();
        let timeout = self
            .profile
            .get("timeoutMs")
            .and_then(JsValue::as_number)
            .map(crate::runtime::network::node_timer_duration)
            .unwrap_or(UNDICI_DEFAULT_TIMEOUT);
        let url = format!("{}{path}", self.endpoint);
        let request = self.http.request(
            if body.is_some() { "POST" } else { "GET" },
            &url,
            body.map(|_| "application/json"),
            &encoded,
            timeout,
        );
        let response = tokio::select! {
            biased;
            _ = signal.cancelled() => return Err(cancelled()),
            response = request => response.map_err(|error| {
                let reason = if error.contains("TimeoutError") { "StaffDeck SOP runtime request timed out." } else { error.strip_prefix("TypeError: ").unwrap_or(&error) };
                SopError::new("SOP_RUNTIME_UNAVAILABLE", format!("StaffDeck SOP runtime request failed: {reason}")).retry("safe")
            })?
        };
        let payload = pilotdeck_protocol::parse_json(&String::from_utf8_lossy(&response.body))
            .unwrap_or(JsValue::Undefined);
        Ok((response.status, payload))
    }
}
fn protocol(message: impl Into<String>) -> SopError {
    SopError::new("SOP_RUNTIME_PROTOCOL", message)
}
fn cancelled() -> SopError {
    SopError::new(
        "SOP_RUNTIME_CANCELLED",
        "StaffDeck SOP runtime request was cancelled: This operation was aborted",
    )
    .retry("unsafe")
}
fn validate_manifest(value: &JsValue, profile: &JsValue) -> Result<(), SopError> {
    let operations = value.get("operations");
    if !is(value, "status", "ok") || !is(value, "protocolVersion", "2.0") || !is(value, "moduleId", "sop.runtime") || !is(value, "contract", "sop.lifecycle/v2")
        || !["prepare", "submit"].iter().all(|op| matches!(operations, Some(JsValue::Array(values)) if values.contains(&(*op).into()))) {
        return Err(SopError::new("SOP_PROTOCOL_INCOMPATIBLE", "StaffDeck SOP runtime does not advertise sop.runtime sop.lifecycle/v2 protocol 2.0 support."));
    }
    if let Some(implementation) = text(profile, "implementationId") {
        let contract = text(profile, "contract").unwrap_or_default();
        let transport = text(profile, "transport").unwrap_or_default();
        if !is(value, "descriptorVersion", "1.0")
            || !is(value, "implementationId", &implementation)
            || !is(value, "contract", &contract)
            || !is(value, "transport", &transport)
        {
            return Err(SopError::new(
                "SOP_PROTOCOL_INCOMPATIBLE",
                format!(
                    "SOP runtime does not advertise the configured {implementation} {contract}/{transport} binding."
                ),
            ));
        }
    }
    Ok(())
}
pub(super) fn valid_step(value: &JsValue) -> bool {
    value.as_object().is_some()
        && ["skillId", "skillName", "version", "nodeId", "instruction"]
            .iter()
            .all(|k| string(value, k))
        && ["node", "knownSlots"].iter().all(|k| record(value, k))
        && [
            "expectedUserInfo",
            "allowedNextStepIds",
            "requiredToolNames",
            "allowedActions",
        ]
        .iter()
        .all(|k| array(value, k, |v| v.as_string().is_some()))
        && ["isTerminal", "declaresHandoff"]
            .iter()
            .all(|k| matches!(value.get(k), Some(JsValue::Bool(_))))
        && optional(value, "transitions", |v| {
            matches!(v, JsValue::Array(rows) if rows.iter().all(|v| {
                v.as_object().is_some() && string(v, "nextStepId") && string(v, "condition") && record(v, "targetStep")
                    && v.get("priority").and_then(JsValue::as_number).is_some_and(f64::is_finite)
                    && optional(v, "label", |v| v.as_string().is_some())
            }))
        })
        && optional(value, "contextMode", |v| {
            v == &JsValue::from("new_session") || v == &JsValue::from("inherit")
        })
        && optional(value, "subSopId", nullable_string)
}
pub(super) fn valid_result(value: &JsValue) -> bool {
    value.as_object().is_some()
        && string(value, "status")
        && string(value, "replyFragment")
        && record(value, "slotUpdates")
        && optional(value, "taskSummary", nullable_string)
        && optional(value, "nextStepId", nullable_string)
        && array(value, "events", |v| v.as_object().is_some())
}
fn nullable_string(value: &JsValue) -> bool {
    matches!(value, JsValue::Null | JsValue::String(_))
}
fn valid_error(value: &JsValue) -> bool {
    value.as_object().is_some()
        && string(value, "code")
        && string(value, "message")
        && ["safe", "unsafe", "retry_after_status"]
            .iter()
            .any(|v| is(value, "retryability", v))
        && optional(value, "details", |v| v.as_object().is_some())
}
