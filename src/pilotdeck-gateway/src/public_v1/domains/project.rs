//! `project.*` — project registration and lookup.

use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use serde_json::Value;

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.handler("project.register", |rt, input, _key, _rev| {
        Box::pin(project_register(rt, input))
    });
    reg.handler("project.get", |rt, input, _key, _rev| {
        Box::pin(project_get(rt, input))
    });
    reg.handler("project.list", |rt, input, _key, _rev| {
        Box::pin(project_list(rt, input))
    });
}

async fn project_register(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    Ok((
        ResponseOutcome::Succeeded,
        rt.store().register_project(&input)?,
    ))
}

async fn project_get(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    Ok((
        ResponseOutcome::Succeeded,
        rt.store()
            .get_project(input["projectId"].as_str().unwrap_or_default())?,
    ))
}

async fn project_list(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    Ok((
        ResponseOutcome::Succeeded,
        rt.store().list_projects(input.get("page"))?,
    ))
}
