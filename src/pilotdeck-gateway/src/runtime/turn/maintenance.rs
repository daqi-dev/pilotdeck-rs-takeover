//! Session-owned manual compaction (`ManualCompactionController.ts`): one
//! maintenance turn outside the agent loop. It brackets the turn with
//! `turn_started` / `turn_result`, runs the session context's compaction with
//! `trigger: "manual", manualForce: true` through the same durable wrapper the
//! loop's compaction uses, and persists the replacement itself (there is no
//! loop callback to do it).
use super::host_factory::MaintenanceContext;
use crate::{
    host::{Cancellation, HostError, modules::context::ContextInput},
    metadata::object,
    runtime::project::RuntimeError,
    storage::recorder::Recorder,
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use pilotdeck_transport::{server::ExecutionClock, system_clock::SystemExecutionClock};
use std::sync::Arc;

static UNDEFINED: JsValue = JsValue::Undefined;
fn field<'a>(value: &'a JsValue, key: &str) -> &'a JsValue {
    value.get(key).unwrap_or(&UNDEFINED)
}
fn is_type(value: &JsValue, expected: &str) -> bool {
    field(value, "type")
        .as_string()
        .is_some_and(|kind| kind.equals_str(expected))
}
fn truthy(value: &JsValue) -> bool {
    crate::runtime::context::compaction::js_truthy(value)
}
fn host_message(error: &HostError) -> String {
    error.module_error.as_ref().map_or_else(
        || error.message.clone(),
        |module| String::from_utf16_lossy(module.message.units()),
    )
}
fn recorder_message(error: impl std::fmt::Display) -> String {
    error.to_string()
}

enum Outcome {
    Skipped,
    Compacted {
        compaction_id: JsValue,
        pre_tokens: JsValue,
        post_tokens: JsValue,
        messages_summarized: JsValue,
        usage: JsValue,
    },
}

/// `ManualCompactionController.compact`. `Err` only when the maintenance
/// turn could not even start; every later failure is a durable terminal
/// result and an `Ok` value with `type: "failed" | "aborted"`.
pub(super) async fn compact(
    recorder: Arc<Recorder>,
    session: JsString,
    turn: JsString,
    messages: Vec<JsValue>,
    context: MaintenanceContext,
    cancel: Cancellation,
) -> Result<JsValue, RuntimeError> {
    let clock = SystemExecutionClock;
    let started_at = clock.now_iso();
    recorder
        .start_turn(&session, &turn)
        .map_err(|error| RuntimeError::new(recorder_message(error)))?;
    let outcome = run(&recorder, &session, &turn, messages, &context, &cancel).await;
    let completed_at = clock.now_iso();
    let terminal = |kind: &str, stop: &str, usage: JsValue| {
        object([
            ("type", kind.into()),
            ("sessionId", JsValue::String(session.clone())),
            ("turnId", JsValue::String(turn.clone())),
            ("stopReason", stop.into()),
            ("usage", usage),
            ("permissionDenials", JsValue::Array(vec![])),
            ("turns", JsValue::Number(0.0)),
            ("startedAt", JsValue::String(started_at.clone())),
            ("completedAt", JsValue::String(completed_at.clone())),
        ])
    };
    let (result, reply) = match outcome {
        Ok(Outcome::Skipped) => (
            terminal("success", "completed", JsValue::Object(JsObject::new())),
            object([
                ("type", "skipped".into()),
                ("turnId", JsValue::String(turn.clone())),
                ("reason", "no_compactable_history".into()),
                ("usage", JsValue::Object(JsObject::new())),
            ]),
        ),
        Ok(Outcome::Compacted {
            compaction_id,
            pre_tokens,
            post_tokens,
            messages_summarized,
            usage,
        }) => (
            terminal("success", "completed", usage.clone()),
            object([
                ("type", "compacted".into()),
                ("turnId", JsValue::String(turn.clone())),
                ("compactionId", compaction_id),
                ("preTokens", pre_tokens),
                ("postTokens", post_tokens),
                ("messagesSummarized", messages_summarized),
                ("usage", usage),
            ]),
        ),
        Err(message) => {
            // `failDeferredCompaction(...).catch(() => {})`: closes a
            // compaction bracket left open by a late cancellation.
            let _ = recorder.fail_deferred_compaction(&session, &turn, &message);
            let aborted = cancel.is_cancelled();
            let empty = || JsValue::Object(JsObject::new());
            let result = if aborted {
                terminal("aborted", "aborted_streaming", empty())
            } else {
                let mut result = terminal("error", "model_error", empty());
                if let JsValue::Object(fields) = &mut result {
                    fields.insert(
                        "errors".into(),
                        JsValue::Array(vec![object([
                            ("code", "agent_invalid_state".into()),
                            ("message", JsValue::String(message.as_str().into())),
                        ])]),
                    );
                }
                result
            };
            (
                result,
                object([
                    ("type", if aborted { "aborted" } else { "failed" }.into()),
                    ("turnId", JsValue::String(turn.clone())),
                    ("error", JsValue::String(message.as_str().into())),
                    ("usage", empty()),
                ]),
            )
        }
    };
    recorder
        .complete_turn(&session, &turn, result)
        .map_err(|error| RuntimeError::new(recorder_message(error)))?;
    Ok(reply)
}

/// `throwIfAborted`: the abort reason becomes the error message.
fn aborted(cancel: &Cancellation) -> Result<(), String> {
    if cancel.is_cancelled() {
        return Err(cancel
            .reason()
            .map(|reason| String::from_utf16_lossy(reason.units()))
            .unwrap_or_else(|| "manual compaction aborted".into()));
    }
    Ok(())
}

async fn run(
    recorder: &Arc<Recorder>,
    session: &JsString,
    turn: &JsString,
    messages: Vec<JsValue>,
    context: &MaintenanceContext,
    cancel: &Cancellation,
) -> Result<Outcome, String> {
    aborted(cancel)?;
    let input = ContextInput {
        fields: {
            let mut fields = JsObject::new();
            fields.insert("sessionId".into(), JsValue::String(session.clone()));
            fields.insert("turnId".into(), JsValue::String(turn.clone()));
            fields.insert("trigger".into(), "manual".into());
            fields.insert("manualForce".into(), JsValue::Bool(true));
            fields.insert("messages".into(), JsValue::Array(messages));
            fields
        },
        abort_signal: Some(Arc::new(cancel.clone())),
        budget_evaluator: None,
    };
    let compact = crate::host::modules::context::compact_with_recorder(
        context.runtime.clone(),
        recorder.clone(),
        input,
    )
    .await
    .map_err(|error| host_message(&error))?;
    aborted(cancel)?;
    if is_type(&compact, "skipped") {
        return Ok(Outcome::Skipped);
    }
    let result = field(&compact, "result");
    if !truthy(result)
        || !field(result, "error").is_undefined()
        || field(result, "summaryMessage").is_undefined()
    {
        return Err("Manual compaction provider returned no durable replacement result.".into());
    }
    let id = field(result, "compactionId").clone();
    let boundary = compact_boundary(&compact, result);
    let replacement: Vec<JsValue> = match field(&compact, "messages") {
        JsValue::Array(messages) => messages.iter().map(|m| mark(m, &id)).collect(),
        _ => Vec::new(),
    };
    let persisted = recorder
        .record_compaction_replacement(session, turn, &boundary, &replacement)
        .and_then(|()| recorder.commit_deferred_compaction(session, turn));
    if let Err(error) = persisted {
        let message = recorder_message(error);
        let _ = recorder.fail_deferred_compaction(session, turn, &message);
        return Err(message);
    }
    let usage = match field(result, "summaryUsage") {
        JsValue::Undefined | JsValue::Null => JsValue::Object(JsObject::new()),
        usage => usage.clone(),
    };
    Ok(Outcome::Compacted {
        compaction_id: id,
        pre_tokens: field(result, "preTokens").clone(),
        post_tokens: field(field(&compact, "snapshot"), "tokens").clone(),
        messages_summarized: field(result, "messagesSummarized").clone(),
        usage,
    })
}

/// `compactBoundary` (ManualCompactionController.ts:168-201).
fn compact_boundary(compact: &JsValue, result: &JsValue) -> JsValue {
    let snapshot = field(compact, "snapshot");
    let mut metadata = JsObject::new();
    metadata.insert("compactionId".into(), field(result, "compactionId").clone());
    metadata.insert("trigger".into(), field(result, "trigger").clone());
    metadata.insert("preTokens".into(), field(result, "preTokens").clone());
    metadata.insert("postTokens".into(), field(snapshot, "tokens").clone());
    metadata.insert(
        "messagesSummarized".into(),
        field(result, "messagesSummarized").clone(),
    );
    let target = field(result, "targetPostTokens");
    if !target.is_undefined() {
        metadata.insert("targetTokens".into(), target.clone());
    }
    let summary_message = !field(result, "summaryMessage").is_undefined();
    let generated = match field(result, "summaryGenerated") {
        JsValue::Undefined | JsValue::Null => JsValue::Bool(summary_message),
        value => value.clone(),
    };
    metadata.insert("summaryGenerated".into(), generated);
    let cache_reset = field(result, "cacheReset") == &JsValue::Bool(true);
    let merged = match field(result, "checkpointMerged") {
        JsValue::Undefined | JsValue::Null => JsValue::Bool(cache_reset),
        value => value.clone(),
    };
    metadata.insert("checkpointMerged".into(), merged);
    metadata.insert("finalRatio".into(), field(snapshot, "ratio").clone());
    let mut extra = JsObject::new();
    extra.insert("tier".into(), field(compact, "tier").clone());
    extra.insert(
        "summarySucceeded".into(),
        JsValue::Bool(field(result, "error").is_undefined() && summary_message),
    );
    if truthy(field(result, "cacheReset")) {
        extra.insert("cacheReset".into(), JsValue::Bool(true));
    }
    let error = field(compact, "error");
    if truthy(error) {
        extra.insert(
            "finalBudgetTokens".into(),
            field(snapshot, "maxContextTokens").clone(),
        );
        extra.insert("finalUsedTokens".into(), field(snapshot, "tokens").clone());
        extra.insert("finalBudgetRatio".into(), field(snapshot, "ratio").clone());
        extra.insert("error".into(), error.clone());
    }
    metadata.insert("extra".into(), JsValue::Object(extra));
    object([
        ("kind", "compact".into()),
        ("subtype", "compact_boundary".into()),
        ("compactMetadata", JsValue::Object(metadata)),
    ])
}

/// `markCompactReplacementMessages`.
fn mark(message: &JsValue, id: &JsValue) -> JsValue {
    let mut marked = message.as_object().cloned().unwrap_or_default();
    let mut metadata = marked
        .get("metadata")
        .and_then(JsValue::as_object)
        .cloned()
        .unwrap_or_default();
    metadata.insert("compactReplacement".into(), JsValue::Bool(true));
    metadata.insert("compactSnapshotId".into(), id.clone());
    marked.insert("metadata".into(), JsValue::Object(metadata));
    JsValue::Object(marked)
}
