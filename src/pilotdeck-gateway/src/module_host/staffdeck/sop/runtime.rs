use super::*;
use crate::{
    metadata::object,
    runtime::{network::Cancellation, turn::host_factory::HostPortDecorators},
};
use pilotdeck_protocol::admission::ExecutionIdentity;
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

#[derive(Default)]
pub(super) struct TurnState {
    pub selected: Option<String>,
    pub prepared: Option<JsValue>,
    pub preflight: bool,
    pub correction: bool,
    pub collect_checked: bool,
    pub user_input: String,
    pub submission: Option<JsValue>,
}
pub(crate) struct SopRuntime {
    pub(super) profile: JsValue,
    pub(super) bundle: JsValue,
    pub(super) session: JsString,
    pub(super) client: client::Client,
    pub(super) store: store::Store,
    pub(super) turn: Mutex<TurnState>,
    pub(super) signal: Mutex<Cancellation>,
}
impl SopRuntime {
    pub(crate) fn new(profile: JsValue, session: JsString) -> Result<Arc<Self>, SopError> {
        let path = text(&profile, "definitionsPath").ok_or_else(|| {
            SopError::new(
                "SOP_DEFINITION_INVALID",
                "StaffDeck SOP definitionsPath is required.",
            )
        })?;
        let bundle = definitions::load(Path::new(&path))?;
        let selected = text(&profile, "defaultSopId").unwrap_or_default();
        if !contains_sop(&bundle, &selected) {
            return Err(SopError::new(
                "SOP_DEFINITION_INVALID",
                format!("StaffDeck SOP defaultSopId '{selected}' is not present in {path}."),
            ));
        }
        let root = text(&profile, "stateRoot").ok_or_else(|| {
            SopError::new(
                "SOP_DEFINITION_INVALID",
                "StaffDeck SOP stateRoot is required.",
            )
        })?;
        Ok(Arc::new(Self {
            client: client::Client::new(profile.clone())?,
            store: store::Store::new(root, session.clone()),
            profile,
            bundle,
            session,
            turn: Mutex::new(TurnState {
                selected: Some(selected),
                ..Default::default()
            }),
            signal: Mutex::default(),
        }))
    }
    pub(crate) fn bind_signal(&self, signal: Cancellation) {
        *self.signal.lock().unwrap_or_else(|e| e.into_inner()) = signal;
    }
    pub(super) fn signal(&self) -> Cancellation {
        self.signal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub(crate) fn decorators(self: &Arc<Self>) -> HostPortDecorators {
        let runtime = self.clone();
        let model = self.clone();
        let tool = self.clone();
        HostPortDecorators {
            context: Some(Arc::new(move |identity, inner| {
                Arc::new(ports::Context {
                    runtime: runtime.clone(),
                    identity: identity.clone(),
                    inner,
                })
            })),
            model: Some(Arc::new(move |_, inner| {
                Arc::new(ports::Model {
                    runtime: model.clone(),
                    inner,
                })
            })),
            tool_execution: Some(Arc::new(move |identity, inner| {
                Arc::new(ports::Tools {
                    runtime: tool.clone(),
                    identity: identity.clone(),
                    inner,
                })
            })),
        }
    }
    pub(super) fn step(&self) -> Option<JsValue> {
        self.turn
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .prepared
            .as_ref()
            .and_then(|v| v.get("step"))
            .cloned()
    }
    pub(super) async fn start(&self, messages: &JsValue) -> Result<(), SopError> {
        let user_input = latest_user(messages);
        let current = self.store.read().await?;
        let persisted = current
            .as_ref()
            .and_then(|v| v.get("state"))
            .and_then(|v| text(v, "selected_skill_id").or_else(|| text(v, "active_skill_id")));
        let selected = if let Some(selected) = persisted {
            Some(selected)
        } else if ["discoveryEndpoint", "discoveryAgentId", "discoveryApiKey"]
            .iter()
            .all(|k| text(&self.profile, k).is_some_and(|v| !v.is_empty()))
        {
            self.discover(&user_input).await?
        } else {
            text(&self.profile, "defaultSopId")
        };
        if let Some(selected) = &selected {
            if !contains_sop(&self.bundle, selected) {
                return Err(SopError::new(
                    "SOP_DISCOVERY_DEFINITION_MISSING",
                    format!(
                        "StaffDeck SOP discovery selected '{selected}', but the PilotDeck definition bundle does not contain it."
                    ),
                ));
            }
        }
        *self.turn.lock().unwrap_or_else(|e| e.into_inner()) = TurnState {
            selected,
            user_input,
            ..Default::default()
        };
        Ok(())
    }
    async fn discover(&self, message: &str) -> Result<Option<String>, SopError> {
        use crate::channel::http_client::{
            HttpClient, UNDICI_DEFAULT_TIMEOUT, encode_uri_component,
        };
        let endpoint = text(&self.profile, "discoveryEndpoint").unwrap();
        let agent = text(&self.profile, "discoveryAgentId").unwrap();
        let key = text(&self.profile, "discoveryApiKey").unwrap();
        let path = text(&self.profile, "discoveryPath")
            .unwrap_or_else(|| format!("/agents/{}/sops:route", encode_uri_component(&agent)));
        let url = format!("{}{path}", endpoint.trim_end_matches('/'));
        let timeout = self
            .profile
            .get("discoveryTimeoutMs")
            .or_else(|| self.profile.get("timeoutMs"))
            .and_then(JsValue::as_number)
            .map(crate::runtime::network::node_timer_duration)
            .unwrap_or(UNDICI_DEFAULT_TIMEOUT);
        let body = json(&object([
            ("message", message.into()),
            ("model_source", "pilotdeck_host".into()),
            ("session_id", JsValue::String(self.session.clone())),
        ]));
        let client = HttpClient::new();
        let signal = self.signal();
        let response = tokio::select! { biased;
            _ = signal.cancelled() => return Err(SopError::new("SOP_DISCOVERY_UNAVAILABLE", "StaffDeck SOP discovery request failed: This operation was aborted")),
            response = client.request_bearer("POST", &url, Some("application/json"), &key, &body, timeout) => response.map_err(|_| SopError::new("SOP_DISCOVERY_UNAVAILABLE", "StaffDeck SOP discovery request failed: fetch failed"))?
        };
        let value = pilotdeck_protocol::parse_json(&String::from_utf8_lossy(&response.body))
            .unwrap_or(JsValue::Undefined);
        if !response.is_ok() {
            return Err(SopError::new(
                &text(&value, "code").unwrap_or_else(|| "SOP_DISCOVERY_FAILED".into()),
                format!("StaffDeck SOP discovery returned HTTP {}.", response.status),
            ));
        }
        if !string(&value, "decision") {
            return Err(SopError::new(
                "SOP_DISCOVERY_PROTOCOL",
                "StaffDeck SOP discovery returned an invalid route response.",
            ));
        }
        let selected = text(&value, "selected_sop_id").filter(|v| !v.is_empty());
        if let Some(selected) = &selected {
            if matches!(value.get("candidate_sop_ids"), Some(JsValue::Array(_)))
                && !strings(&value, "candidate_sop_ids").contains(selected)
            {
                return Err(SopError::new(
                    "SOP_DISCOVERY_SELECTION_NOT_VISIBLE",
                    format!(
                        "StaffDeck SOP discovery selected '{selected}', but StaffDeck did not expose it as a candidate."
                    ),
                ));
            }
        }
        Ok(selected)
    }
    pub(super) async fn prepare(
        &self,
        identity: &ExecutionIdentity,
        step_id: Option<JsValue>,
    ) -> Result<Option<JsValue>, SopError> {
        let selected = {
            let mut turn = self.turn.lock().unwrap_or_else(|e| e.into_inner());
            turn.prepared = None;
            turn.selected.clone()
        };
        let Some(selected) = selected else {
            return Ok(None);
        };
        let persisted = self
            .store
            .load_or_create(self.bundle.clone(), selected.as_str().into())
            .await?;
        let state = persisted.get("state").unwrap();
        if ["completed", "handoff", "blocked", "waiting_external_task"]
            .iter()
            .any(|v| is(state, "status", v))
        {
            return Ok(None);
        }
        let session = String::from_utf16_lossy(self.session.units());
        let turn = String::from_utf16_lossy(identity.turn_id.units());
        let request = format!(
            "sop.prepare:{session}:{turn}:{}",
            step_id.as_ref().map(json).unwrap_or_else(|| "0".into())
        );
        let context = object([
            ("runId", format!("sop:{session}:{turn}").as_str().into()),
            (
                "operationId",
                format!("sop.prepare:{session}:{turn}").as_str().into(),
            ),
            ("requestId", request.as_str().into()),
            ("sessionId", JsValue::String(self.session.clone())),
            ("turnId", JsValue::String(identity.turn_id.clone())),
            ("idempotencyKey", request.as_str().into()),
            (
                "expectedRevision",
                persisted.get("revision").unwrap().clone(),
            ),
        ])
        .as_object()
        .unwrap()
        .clone();
        let mut prepared = self
            .client
            .prepare(
                persisted.get("bundle").unwrap().clone(),
                state.clone(),
                context,
                &self.signal(),
            )
            .await?;
        let mut step = prepared.get("step").unwrap().clone();
        remove(&mut step, "optionalKnowledge");
        remove(&mut step, "optionalKnowledgeUnavailable");
        if step
            .get("node")
            .is_some_and(|v| is(v, "type", "knowledge_query"))
        {
            let mut required = strings(&step, "requiredToolNames");
            if !required.iter().any(|v| v == "knowledge_query") {
                required.push("knowledge_query".into());
            }
            set(
                &mut step,
                "requiredToolNames",
                JsValue::Array(required.iter().map(|v| v.as_str().into()).collect()),
            );
        }
        let mode = definitions::context_mode(
            &step,
            &self.profile,
            persisted.get("bundle").unwrap(),
            &text(&step, "skillId").unwrap(),
        );
        set(&mut step, "contextMode", mode.as_str().into());
        set(&mut prepared, "step", step);
        self.store
            .replace(
                persisted.get("bundle").unwrap().clone(),
                prepared.get("state").unwrap().clone(),
                persisted.get("revision").and_then(JsValue::as_number),
            )
            .await?;
        self.turn.lock().unwrap_or_else(|e| e.into_inner()).prepared = Some(prepared.clone());
        Ok(Some(prepared))
    }
}
fn contains_sop(bundle: &JsValue, id: &str) -> bool {
    matches!(bundle.get("sops"), Some(JsValue::Array(rows)) if rows.iter().any(|v| text(v, "id").or_else(|| text(v, "skill_id")).as_deref() == Some(id)))
}
fn latest_user(messages: &JsValue) -> String {
    let JsValue::Array(rows) = messages else {
        return String::new();
    };
    rows.iter()
        .rev()
        .find(|v| is(v, "role", "user"))
        .map(|v| match v.get("content") {
            Some(JsValue::Array(parts)) => parts
                .iter()
                .filter(|v| is(v, "type", "text"))
                .filter_map(|v| text(v, "text"))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        })
        .unwrap_or_default()
}
