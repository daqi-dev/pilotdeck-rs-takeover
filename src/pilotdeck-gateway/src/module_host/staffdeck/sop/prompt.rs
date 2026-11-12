use super::*;
pub(super) fn instruction(prepared: &JsValue, state: Option<&JsValue>) -> String {
    let step = prepared.get("step").unwrap();
    let text_of = |key| text(step, key).unwrap_or_default();
    let fields = strings(step, "expectedUserInfo");
    let required = strings(step, "requiredToolNames");
    let targets = strings(step, "allowedNextStepIds");
    let handoff = step.get("declaresHandoff") == Some(&JsValue::Bool(true));
    let terminal = step.get("isTerminal") == Some(&JsValue::Bool(true));
    let mut lines = vec![
        "<staffdeck-sop>".into(),
        format!(
            "Current SOP: {} ({}), step {}.",
            text_of("skillName"),
            text_of("skillId"),
            text_of("nodeId")
        ),
    ];
    if let Some(id) =
        state.and_then(|v| text(v, "active_skill_id").or_else(|| text(v, "selected_skill_id")))
    {
        lines.push(format!(
            "Authoritative SOP ID from persisted owner state: {id}."
        ));
    }
    if !text_of("instruction").is_empty() {
        lines.push(format!("Step instruction: {}", text_of("instruction")));
    }
    if !fields.is_empty() {
        lines.push(format!("Required user information: {}.", fields.join(", ")));
    }
    lines.push(format!(
        "Already persisted user fields: {}.",
        step.get("knownSlots")
            .map(json)
            .unwrap_or_else(|| "{}".into())
    ));
    if !required.is_empty() {
        lines.push(format!(
            "Required successful tools: {}.",
            required.join(", ")
        ));
    }
    if !targets.is_empty() {
        lines.push(format!("Allowed next steps: {}.", targets.join(", ")));
    }
    let transitions = match step.get("transitions") {
        Some(JsValue::Array(rows)) => rows.as_slice(),
        _ => &[],
    };
    if !transitions.is_empty() {
        let guidance = transitions
            .iter()
            .map(|v| {
                let target = v.get("targetStep").unwrap();
                let id = text(v, "nextStepId").unwrap_or_default();
                let target_name = text(target, "name")
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| id.clone());
                let target_parts = [
                    Some(target_name),
                    text(target, "type"),
                    text(target, "instruction"),
                ]
                .into_iter()
                .flatten()
                .filter(|s| !s.trim().is_empty())
                .collect::<Vec<_>>()
                .join("; ");
                let label = text(v, "label")
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| format!(", label={s}"))
                    .unwrap_or_default();
                let condition = text(v, "condition")
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "default".into());
                format!(
                    "- {id}: condition={condition}, priority={}{}; target={target_parts}",
                    v.get("priority").map(json).unwrap_or_default(),
                    label
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        lines.push(format!("Transition guidance:\n{guidance}"));
    }
    if !fields.is_empty() {
        lines.push("When required information is present in the user messages or tool results, include it in slotUpdates and submit completed to advance; use awaiting_user only when information is genuinely missing. If any required field is missing, your question MUST be the replyFragment of a submit_step_result call with status awaiting_user, known slotUpdates, and no nextStepId. Do not end with a plain-text question.".into());
        lines.push("Before submitting, extract each supplied value into slotUpdates under its exact required field key, including when other fields remain missing. Preserve explicit user facts instead of replacing them with inferred risks or future changes. Saying a value is recorded in replyFragment does not store it. Do not submit an empty slotUpdates when the user has supplied any required field.".into());
    }
    if !transitions.is_empty() {
        lines.push("Choose nextStepId from the transition whose condition matches the available facts. If the target is a handoff step, advance to it first and use status handoff there when approval is required.".into());
    }
    if handoff {
        lines.push("This step declares a human handoff. When approval is required, use status handoff to create the resumable approval wait; do not use awaiting_user. After an authorized human reply, persist the actual decision according to this node's instruction with submit_step_result and advance only to an allowed next step. A completed node means its decision was processed, not that the decision was approval. If this node instructs completed after either approval or rejection, submit completed for either decision and quote it accurately; do not substitute failed merely because the human rejected. Do not end with a plain-text approval summary.".into());
    }
    lines.push(if terminal {
        if handoff || !fields.is_empty() { "This is the active final SOP node. Being final does not remove its approval or required-field contract. Persist a required wait with submit_step_result before ending; complete only when this node's conditions are satisfied, preserving supplied fields in slotUpdates and omitting nextStepId. Do not claim completion before the tool succeeds." }
        else { "This is the active final SOP node, even if the previous node already submitted completed. Put the final user-facing answer in replyFragment and call submit_step_result with status completed, slotUpdates {}, and no nextStepId. Do not emit the answer as plain assistant text or claim the SOP is complete before this tool succeeds." }
    } else { "A completed submission closes only this node. Its replyFragment is this node's result, not a submission or final answer for a successor node; the successor must be prepared and submitted separately." }.into());
    if !handoff
        && fields.is_empty()
        && !targets.is_empty()
        && !strings(step, "allowedActions")
            .iter()
            .any(|v| v == "ask_user" || v == "ask_missing")
    {
        lines.push("This step has no missing user fields. After producing its required evidence or draft, submit completed and advance to an allowed next step. Approval belongs to the declared handoff step; do not pause this evidence step with awaiting_user.".into());
    }
    lines.extend(["A question asking for missing fields is also a step result: persist it with awaiting_user and no nextStepId before ending the turn. A plain-text answer without submit_step_result does not persist SOP progress.", "When this step has a result, call submit_step_result exactly once. Do not claim a tool succeeded before its result is in the conversation.", "Use status awaiting_user for missing user information and handoff only when this step explicitly permits it.", "</staffdeck-sop>"].into_iter().map(str::to_owned));
    lines.join("\n")
}
