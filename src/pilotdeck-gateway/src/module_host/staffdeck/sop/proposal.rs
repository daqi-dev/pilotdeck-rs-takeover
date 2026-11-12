use super::*;
use crate::metadata::object;
pub(super) fn parse(value: &JsValue) -> Option<JsValue> {
    let status = text(value, "status")?;
    if ![
        "completed",
        "awaiting_user",
        "handoff",
        "failed",
        "blocked",
        "waiting_external_task",
    ]
    .contains(&status.as_str())
    {
        return None;
    }
    let reply = text(value, "replyFragment")?;
    let reply = crate::metadata::js_trim(&reply);
    if reply.is_empty()
        || !optional(value, "slotUpdates", |v| v.as_object().is_some())
        || !["nextStepId", "taskSummary"].iter().all(|k| {
            optional(value, k, |v| {
                matches!(v, JsValue::Null | JsValue::String(_))
            })
        })
    {
        return None;
    }
    let mut proposal = object([
        ("status", status.as_str().into()),
        ("replyFragment", reply.into()),
    ]);
    for key in ["slotUpdates", "structuredResult"] {
        if let Some(value) = value.get(key) {
            set(&mut proposal, key, value.clone());
        }
    }
    for key in ["taskSummary", "nextStepId"] {
        if let Some(value) = text(value, key)
            .map(|v| crate::metadata::js_trim(&v).to_owned())
            .filter(|v| !v.is_empty())
        {
            set(&mut proposal, key, value.as_str().into());
        }
    }
    Some(proposal)
}
pub(super) fn validate(
    proposal: &mut JsValue,
    step: &JsValue,
    state: &JsValue,
    user: &str,
    checked: &mut bool,
) -> Result<(), SopError> {
    if step.get("nodeId") != state.get("active_step_id") {
        return Ok(());
    }
    let fields = strings(step, "expectedUserInfo");
    let prior = state.get("slots_json");
    let updates = proposal.get("slotUpdates");
    for field in &fields {
        if let Some(update) = updates.and_then(|v| v.get(field)) {
            if filled(prior.and_then(|v| v.get(field))) && !filled(Some(update)) {
                return Err(invalid(format!(
                    "REQUIRED_SLOT_ERASURE: {field} is already filled; an empty update cannot erase it."
                )));
            }
        }
    }
    if is(proposal, "status", "handoff")
        && step.get("declaresHandoff") != Some(&JsValue::Bool(true))
    {
        return Err(invalid(
            "HANDOFF_NOT_DECLARED: complete the current evidence step and advance to the declared approval node before creating handoff.",
        ));
    }
    if is(proposal, "status", "awaiting_user")
        && step.get("declaresHandoff") == Some(&JsValue::Bool(true))
    {
        return Err(invalid(
            "HANDOFF_REQUIRED: this node declares a resumable human approval handoff. Submit handoff to wait for the responsible person's reply; awaiting_user cannot create that approval wait.",
        ));
    }
    let allowed = strings(step, "allowedNextStepIds");
    if let Some(next) = text(proposal, "nextStepId") {
        if !allowed.contains(&next) {
            let node = text(step, "nodeId").unwrap_or_default();
            let targets = if allowed.is_empty() {
                "none (this node is terminal or must remain active)".into()
            } else {
                allowed.join(", ")
            };
            let mut error = invalid(format!(
                "SOP_INVALID_TRANSITION: nextStepId '{next}' is not declared from '{node}'. Allowed nextStepId values: {targets}. Do not invent a transition; preserve the current state and submit the declared result or ask for the missing information."
            ));
            error.details = Some(object([
                ("activeStepId", node.as_str().into()),
                ("requestedNextStepId", next.as_str().into()),
                (
                    "allowedNextStepIds",
                    step.get("allowedNextStepIds").unwrap().clone(),
                ),
            ]));
            return Err(error);
        }
    }
    let asks = strings(step, "allowedActions")
        .iter()
        .any(|v| v == "ask_user" || v == "ask_missing");
    if is(proposal, "status", "awaiting_user") {
        if text(proposal, "nextStepId").is_some() {
            return Err(invalid(
                "AWAITING_USER_CANNOT_ADVANCE: remove nextStepId and retain the current step.",
            ));
        }
        let missing = fields
            .iter()
            .filter(|field| {
                !filled(
                    updates
                        .and_then(|v| v.get(field))
                        .or_else(|| prior.and_then(|v| v.get(field))),
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        let collect = step
            .get("node")
            .is_some_and(|v| is(v, "type", "collect_info"));
        if !missing.is_empty()
            && collect
            && (is(state, "status", "awaiting_user")
                || updates
                    .and_then(JsValue::as_object)
                    .is_none_or(JsObject::is_empty))
            && !crate::metadata::js_trim(user).is_empty()
            && !*checked
        {
            *checked = true;
            return Err(invalid(format!(
                "COLLECT_FIELDS_CHECK: Before waiting, re-read this turn's user message: {}. Persisted slots: {}. Your proposal still leaves these fields missing: {}. Extract every explicitly supplied value into slotUpdates using these exact keys: {}. If a field is genuinely absent, resubmit awaiting_user for only that field; do not invent a value or repeat fields already supplied.",
                json(&user.into()),
                prior.map(json).unwrap_or_else(|| "{}".into()),
                missing.join(", "),
                fields.join(", ")
            )));
        }
        if missing.is_empty() && (!fields.is_empty() || !asks) {
            return Err(invalid(
                "STEP_MUST_ADVANCE: required fields are complete. Submit completed to an allowed next step; approval belongs to the declared handoff node.",
            ));
        }
        if collect && !missing.is_empty() {
            set(
                proposal,
                "replyFragment",
                format!(
                    "请补充以下信息，以继续当前步骤：\n{}",
                    missing
                        .iter()
                        .map(|v| format!("- {v}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
                .as_str()
                .into(),
            );
        }
    }
    if is(proposal, "status", "completed")
        && step.get("isTerminal") == Some(&JsValue::Bool(true))
        && fields.is_empty()
        && !asks
    {
        static WAIT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"(?i)(?:^|[。！？.!?\n])\s*(?:(?:由于)?(?:我|目前|当前|本步骤|此步骤|SOP).{0,80}(?:卡在|无法(?:继续|推进|完成|生成)|缺少.{0,30}(?:信息|资料|上下文))|请(?:您|你)?(?:提供|补充)|(?:I|we|this step|the SOP)\s+(?:(?:am|are|is)\s+)?(?:stuck|blocked|cannot|can't|lack|need more information)|(?:please|could you)\s+(?:provide|supply))").unwrap()
        });
        if WAIT.is_match(&text(proposal, "replyFragment").unwrap_or_default()) {
            return Err(invalid(
                "FINAL_REPLY_CONTRADICTS_COMPLETION: this terminal step has no missing user fields, but replyFragment says progress is blocked or asks the user for missing information. Rewrite the final answer from the current step instruction and known facts, then submit completed without a nextStepId. Do not reuse the rejected waiting reply.",
            ));
        }
    }
    if is(proposal, "status", "completed")
        && step
            .get("node")
            .is_some_and(|v| is(v, "type", "knowledge_query"))
        && !strings(state, "successful_tool_names")
            .iter()
            .any(|v| v == "knowledge_query")
    {
        return Err(invalid(
            "REQUIRED_CAPABILITY_NOT_INVOKED: the current knowledge node requires a successful knowledge_query receipt before completion.",
        ));
    }
    Ok(())
}
fn invalid(message: impl Into<String>) -> SopError {
    SopError::new("invalid_tool_input", message)
}
fn filled(value: Option<&JsValue>) -> bool {
    match value {
        None | Some(JsValue::Null | JsValue::Undefined) => false,
        Some(JsValue::String(v)) => {
            !crate::metadata::js_trim(&String::from_utf16_lossy(v.units())).is_empty()
        }
        Some(JsValue::Array(v)) => !v.is_empty(),
        Some(JsValue::Object(v)) => !v.is_empty(),
        _ => true,
    }
}
