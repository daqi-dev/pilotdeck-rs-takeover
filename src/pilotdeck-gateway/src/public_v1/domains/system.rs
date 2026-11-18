//! `system.*`, `schema.*`, and `requests.lookup` — the introspection
//! operations — plus the availability-only registrations for the CLI-local
//! lifecycle operations owned by `bootstrap.rs`.

use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::registry::OperationRegistry;
use serde_json::{Value, json};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.handler("schema.list", |rt, input, _key, _rev| {
        Box::pin(schema_list(rt, input))
    });
    reg.handler("schema.get", |rt, input, _key, _rev| {
        Box::pin(schema_get(rt, input))
    });
    reg.handler("system.capabilities", |rt, _input, _key, _rev| {
        Box::pin(system_capabilities(rt))
    });
    reg.handler("system.health", |rt, _input, _key, _rev| {
        Box::pin(system_health(rt))
    });
    reg.handler("requests.lookup", |rt, input, _key, _rev| {
        Box::pin(requests_lookup(rt, input))
    });

    // CLI-local lifecycle operations (S1/S2): implemented in `bootstrap.rs`
    // and dispatched by the CLI's `local_ops` table, never over `/v1/ws`
    // (dispatch refuses `execution='local'` before reaching handlers).
    // Registered here so the implemented set and `system.capabilities`
    // report them truthfully.
    reg.available("instance.initialize");
    reg.available("service.start");
    reg.available("service.status");
    reg.available("service.stop");
}

async fn schema_list(
    _rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let reg = OperationRegistry::global();
    Ok((ResponseOutcome::Succeeded, reg.handle_schema_list(&input)?))
}

async fn schema_get(
    _rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let reg = OperationRegistry::global();
    Ok((ResponseOutcome::Succeeded, reg.handle_schema_get(&input)?))
}

async fn system_capabilities(
    rt: Arc<PublicV1Runtime>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    Ok((ResponseOutcome::Succeeded, rt.capabilities_json()))
}

async fn system_health(
    rt: Arc<PublicV1Runtime>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "alive": true,
            "ready": true,
            "diagnostics": rt.store().diagnostics(),
        }),
    ))
}

async fn requests_lookup(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let target_method = input["methodId"].as_str().unwrap_or_default();
    let key = input["idempotencyKey"].as_str().unwrap_or_default();
    Ok((
        ResponseOutcome::Succeeded,
        rt.store().lookup_request(target_method, key)?,
    ))
}
