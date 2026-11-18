//! `session.*` — session creation and lookup.

use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use serde_json::Value;

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.handler("session.create", |rt, input, _key, _rev| {
        Box::pin(session_create(rt, input))
    });
    reg.handler("session.get", |rt, input, _key, _rev| {
        Box::pin(session_get(rt, input))
    });
    reg.handler("session.list", |rt, input, _key, _rev| {
        Box::pin(session_list(rt, input))
    });
}

async fn session_create(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    Ok((
        ResponseOutcome::Succeeded,
        rt.store().create_session(&input)?,
    ))
}

async fn session_get(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    Ok((
        ResponseOutcome::Succeeded,
        rt.store()
            .get_session(input["sessionId"].as_str().unwrap_or_default())?,
    ))
}

async fn session_list(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    Ok((
        ResponseOutcome::Succeeded,
        rt.store().list_sessions(
            input["projectId"].as_str().unwrap_or_default(),
            input.get("sessionState").and_then(Value::as_str),
            input.get("page"),
        )?,
    ))
}
