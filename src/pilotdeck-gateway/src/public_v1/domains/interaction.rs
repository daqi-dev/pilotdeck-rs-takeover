//! `interaction.*` and `approval.respond` — interaction listing, lookup, and
//! resolution (waking the run blocked on the answer).

use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use serde_json::{Value, json};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.handler("interaction.list", |rt, input, _key, _rev| {
        Box::pin(interaction_list(rt, input))
    });
    reg.handler("interaction.get", |rt, input, _key, _rev| {
        Box::pin(interaction_get(rt, input))
    });
    reg.handler("approval.respond", |rt, input, _key, rev| {
        Box::pin(approval_respond(rt, input, rev))
    });
    reg.handler("interaction.respond", |rt, input, _key, rev| {
        Box::pin(interaction_respond(rt, input, rev))
    });
}

async fn interaction_list(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    Ok((
        ResponseOutcome::Succeeded,
        // P2 (wp4 review): the operation schema defaults an omitted
        // `state` to `pending`; without this the store listed every
        // state on omission.
        rt.store().list_interactions(
            input.get("scope"),
            input.get("kind").and_then(Value::as_str),
            Some(
                input
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or("pending"),
            ),
            input.get("page"),
        )?,
    ))
}

async fn interaction_get(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let ia = rt
        .store()
        .get_interaction(input["interactionId"].as_str().unwrap_or_default())?;
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "interaction": ia.to_contract_json(),
            "responseSchema": ia.response_schema,
        }),
    ))
}

async fn approval_respond(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    expected_revision: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let rev = expected_revision
        .as_deref()
        .or_else(|| input.get("expectedRevision").and_then(Value::as_str))
        .unwrap_or_default();
    let decision = input["decision"].as_str().unwrap_or("deny");
    let remember = input
        .get("rememberScope")
        .and_then(Value::as_str)
        .unwrap_or("none");
    let res = rt.store().respond_interaction(
        input["interactionId"].as_str().unwrap_or_default(),
        Some("approval"),
        rev,
        json!({"decision": decision, "rememberScope": remember}),
        Some(remember),
    )?;
    if res.should_wake_run {
        rt.notify_interaction_resolved(
            &res.interaction.interaction_id,
            res.decision_or_answer.clone(),
        );
    }
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "interaction": res.interaction.to_contract_json(),
            "policyRevision": res.policy_revision,
        }),
    ))
}

async fn interaction_respond(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    expected_revision: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let rev = expected_revision
        .as_deref()
        .or_else(|| input.get("expectedRevision").and_then(Value::as_str))
        .unwrap_or_default();
    let answer = input.get("answer").cloned().unwrap_or(Value::Null);
    let res = rt.store().respond_interaction(
        input["interactionId"].as_str().unwrap_or_default(),
        None,
        rev,
        answer,
        None,
    )?;
    if res.should_wake_run {
        rt.notify_interaction_resolved(
            &res.interaction.interaction_id,
            res.decision_or_answer.clone(),
        );
    }
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "interaction": res.interaction.to_contract_json(),
        }),
    ))
}
