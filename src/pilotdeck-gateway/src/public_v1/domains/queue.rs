//! `queue.*` — per-session run queue inspection and resumption.

use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use serde_json::Value;

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.handler("queue.get", |rt, input, _key, _rev| {
        Box::pin(queue_get(rt, input))
    });
    reg.handler("queue.resume", |rt, input, _key, rev| {
        Box::pin(queue_resume(rt, input, rev))
    });
}

async fn queue_get(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    Ok((
        ResponseOutcome::Succeeded,
        rt.store().get_queue(
            input["sessionId"].as_str().unwrap_or_default(),
            input.get("page"),
        )?,
    ))
}

async fn queue_resume(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    expected_revision: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let rev = expected_revision
        .as_deref()
        .or_else(|| input.get("expectedRevision").and_then(Value::as_str))
        .unwrap_or_default();
    let (queue_json, next_run_id) = rt.store().resume_queue(
        input["sessionId"].as_str().unwrap_or_default(),
        rev,
        input["acknowledgedPauseReason"]
            .as_str()
            .unwrap_or_default(),
    )?;
    if let Some(run_id) = next_run_id {
        rt.start_session_worker(run_id);
    }
    Ok((ResponseOutcome::Succeeded, queue_json))
}
