use super::*;
use crate::{
    metadata::object,
    module_host::approvals::{StoreStatusInput, pending::DiskSopStore},
};

#[derive(Clone)]
pub(super) struct Store {
    disk: DiskSopStore,
    input: StoreStatusInput,
}
impl Store {
    pub(super) fn new(root: String, session: JsString) -> Self {
        Self {
            disk: DiskSopStore::new(std::sync::Arc::new(move |_| Some(root.clone()))),
            input: StoreStatusInput {
                project_key: None,
                session_key: session,
            },
        }
    }
    pub(super) async fn read(&self) -> Result<Option<JsValue>, SopError> {
        self.disk
            .transact(self.input.clone(), |current| Ok((None, current)))
            .await
    }
    pub(super) async fn load_or_create(
        &self,
        bundle: JsValue,
        selected: JsString,
    ) -> Result<JsValue, SopError> {
        let session = self.input.session_key.clone();
        self.disk
            .transact(self.input.clone(), move |current| {
                if let Some(current) = current {
                    return Ok((None, current));
                }
                let created = object([
                    ("schemaVersion", JsValue::Number(3.0)),
                    ("sessionId", JsValue::String(session)),
                    ("bundle", bundle),
                    (
                        "state",
                        object([
                            ("version", JsValue::Number(1.0)),
                            ("selected_skill_id", JsValue::String(selected)),
                            ("slots_json", object([])),
                            ("skill_stack_json", JsValue::Array(vec![])),
                            ("successful_tool_names", JsValue::Array(vec![])),
                        ]),
                    ),
                    ("revision", JsValue::Number(1.0)),
                    ("resumeRequests", object([])),
                    ("updatedAt", now()),
                ]);
                Ok((Some(created.clone()), created))
            })
            .await
    }
    pub(super) async fn replace(
        &self,
        bundle: JsValue,
        state: JsValue,
        expected: Option<f64>,
    ) -> Result<JsValue, SopError> {
        let session = self.input.session_key.clone();
        self.disk
            .transact(self.input.clone(), move |current| {
                if expected.is_some_and(|e| {
                    current
                        .as_ref()
                        .and_then(|v| v.get("revision"))
                        .and_then(JsValue::as_number)
                        != Some(e)
                }) {
                    return Err(SopError::new(
                        "SOP_REVISION_CONFLICT",
                        "The SOP changed during preparation.",
                    ));
                }
                let next = replacement(&session, bundle, state, current.as_ref());
                Ok((Some(next.clone()), next))
            })
            .await
    }
    pub(super) async fn commit(
        &self,
        bundle: JsValue,
        state: JsValue,
        expected: f64,
        turn: JsString,
        result: JsValue,
    ) -> Result<JsValue, SopError> {
        let session = self.input.session_key.clone();
        self.disk
            .transact(self.input.clone(), move |current| {
                let revision = current
                    .as_ref()
                    .and_then(|v| v.get("revision"))
                    .and_then(JsValue::as_number)
                    .unwrap_or(0.0);
                if revision != expected {
                    return Err(SopError::new(
                        "SOP_REVISION_CONFLICT",
                        format!("Expected SOP revision {expected}, found {revision}."),
                    ));
                }
                let mut next = replacement(&session, bundle, state, current.as_ref());
                set(
                    &mut next,
                    "replyDelivery",
                    object([
                        ("turnId", JsValue::String(turn)),
                        ("phase", "pending".into()),
                        ("result", result),
                        ("committedAt", now()),
                    ]),
                );
                Ok((Some(next.clone()), next))
            })
            .await
    }
    pub(super) async fn mark_reply_durable(
        &self,
        expected: JsString,
        delivered: JsString,
    ) -> Result<JsValue, SopError> {
        self.disk
            .transact(self.input.clone(), move |current| {
                let mut current = current.ok_or_else(|| delivery_missing(&expected))?;
                let mut delivery = current
                    .get("replyDelivery")
                    .filter(|v| v.get("turnId").and_then(JsValue::as_string) == Some(&expected))
                    .cloned()
                    .ok_or_else(|| delivery_missing(&expected))?;
                if is(&delivery, "phase", "durable") && expected == delivered {
                    return Ok((None, delivery));
                }
                let timestamp = now();
                set(&mut delivery, "turnId", JsValue::String(delivered));
                set(&mut delivery, "phase", "durable".into());
                set(&mut delivery, "durableAt", timestamp.clone());
                set(&mut current, "replyDelivery", delivery.clone());
                set(&mut current, "updatedAt", timestamp);
                Ok((Some(current), delivery))
            })
            .await
    }
    pub(super) async fn clear_reply(&self, turn: JsString) -> Result<(), SopError> {
        self.disk
            .transact(self.input.clone(), move |current| {
                let Some(mut current) = current.filter(|v| {
                    v.get("replyDelivery")
                        .and_then(|v| v.get("turnId"))
                        .and_then(JsValue::as_string)
                        == Some(&turn)
                }) else {
                    return Ok((None, ()));
                };
                remove(&mut current, "replyDelivery");
                set(&mut current, "updatedAt", now());
                Ok((Some(current), ()))
            })
            .await
    }
    pub(super) async fn record_successful_tools(
        &self,
        names: Vec<JsString>,
    ) -> Result<(), SopError> {
        self.disk
            .transact(self.input.clone(), move |current| {
                let Some(mut current) = current else {
                    return Ok((None, ()));
                };
                let mut state = current.get("state").unwrap().clone();
                let existing = match state.get("successful_tool_names") {
                    Some(JsValue::Array(values)) => values.as_slice(),
                    _ => &[],
                };
                let mut tools = Vec::new();
                for name in existing
                    .iter()
                    .filter_map(JsValue::as_string)
                    .chain(names.iter())
                    .filter(|name| name.len_utf16() > 0)
                {
                    let value = JsValue::String(name.clone());
                    if !tools.contains(&value) {
                        tools.push(value);
                    }
                }
                set(&mut state, "successful_tool_names", JsValue::Array(tools));
                // Baseline records successful tools by replacing the state, so the
                // revision fences concurrent approval and subsequent submission.
                let session = current
                    .get("sessionId")
                    .and_then(JsValue::as_string)
                    .unwrap()
                    .clone();
                let bundle = current.get("bundle").unwrap().clone();
                current = replacement(&session, bundle, state, Some(&current));
                Ok((Some(current), ()))
            })
            .await
    }
}
fn delivery_missing(turn: &JsString) -> SopError {
    SopError::new(
        "SOP_REPLY_DELIVERY_NOT_FOUND",
        format!(
            "No StaffDeck SOP reply delivery exists for turn '{}'.",
            String::from_utf16_lossy(turn.units())
        ),
    )
}
fn replacement(
    session: &JsString,
    bundle: JsValue,
    state: JsValue,
    current: Option<&JsValue>,
) -> JsValue {
    let revision = current
        .and_then(|v| v.get("revision"))
        .and_then(JsValue::as_number)
        .unwrap_or(0.0)
        + 1.0;
    let wait = wait(&state, current.and_then(|v| v.get("wait")));
    let mut next = object([
        ("schemaVersion", JsValue::Number(3.0)),
        ("sessionId", JsValue::String(session.clone())),
        ("bundle", bundle),
        ("state", state),
        ("revision", JsValue::Number(revision)),
        (
            "resumeRequests",
            current
                .and_then(|v| v.get("resumeRequests"))
                .cloned()
                .unwrap_or_else(|| object([])),
        ),
        ("updatedAt", now()),
    ]);
    if let Some(wait) = wait {
        set(&mut next, "wait", wait);
    }
    if let Some(delivery) = current.and_then(|v| v.get("replyDelivery")) {
        set(&mut next, "replyDelivery", delivery.clone());
    }
    next
}
fn wait(state: &JsValue, previous: Option<&JsValue>) -> Option<JsValue> {
    let kind = if is(state, "status", "handoff") {
        "handoff"
    } else if is(state, "status", "waiting_external_task") {
        "external_task"
    } else {
        return None;
    };
    if let Some(previous) = previous.filter(|v| {
        is(v, "kind", kind)
            && v.get("skillId") == state.get("active_skill_id")
            && v.get("stepId") == state.get("active_step_id")
    }) {
        return Some(previous.clone());
    }
    let id = state
        .get("awaiting_input_json")
        .and_then(|v| v.get("wait_id"))
        .filter(|v| v.as_string().is_some())
        .cloned()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string().as_str().into());
    let mut wait = object([("id", id), ("kind", kind.into()), ("createdAt", now())]);
    for (target, source) in [("skillId", "active_skill_id"), ("stepId", "active_step_id")] {
        if let Some(value) = state.get(source).filter(|v| v.as_string().is_some()) {
            set(&mut wait, target, value.clone());
        }
    }
    Some(wait)
}
