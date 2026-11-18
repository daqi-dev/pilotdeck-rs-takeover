//! Stationary verification and lossless message projection for PilotDeck `.jsonl` transcripts.
//!
//! Ensures an existing session is genuinely stationary (no active writer lease,
//! no pending permissions or questions, no unclosed turn/step, no unpaired or
//! in-flight tool calls, no unresolved `result_unknown` operations, and zero
//! JSONL syntax/envelope corruption) before any cross-backend import occurs.

use std::collections::{BTreeMap, HashMap, HashSet};

use pilotdeck_context_core::wire::{
    block_type, content, field, is_block, is_js_whitespace, is_role, string_field,
};
use pilotdeck_context_iface::{LedgerError, validate_tool_pair_integrity};
use pilotdeck_protocol::{JsObject, JsString, JsValue, parse_json};
use serde::{Deserialize, Serialize};

use crate::error::{MigrationError, sha256_hex, validate_session_id};

/// Maximum accepted `.jsonl` transcript size (50 MiB, matching `pilotdeck-gateway` storage).
pub const MAX_TRANSCRIPT_BYTES: usize = 50 * 1024 * 1024;

const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// Live runtime activity flags observed by the host for a session.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveSessionActivity {
    pub has_active_writer_lease: bool,
    pub pending_permission_count: usize,
    pub pending_question_count: usize,
}

/// Summary of one turn observed during transcript stationary verification.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptTurnSummary {
    pub turn_id: String,
    pub closed: bool,
    pub terminal_type: Option<String>,
    pub open_step: Option<u64>,
    pub step_count: usize,
}

/// Verified stationary transcript audit output carrying lossless `JsValue` messages.
#[derive(Clone, Debug, PartialEq)]
pub struct StationaryAudit {
    pub source_session_id: String,
    pub source_sha256: String,
    pub entry_count: usize,
    pub message_count: usize,
    pub last_sequence: Option<u64>,
    pub turn_count: usize,
    pub turns: Vec<TranscriptTurnSummary>,
    pub has_compaction_boundary: bool,
    pub messages: Vec<JsValue>,
    pub raw_entries: Vec<JsValue>,
}

#[derive(Default)]
struct TurnTracker {
    started: bool,
    requires_closure: bool,
    closed: bool,
    terminal_type: Option<String>,
    last_step: u64,
    open_step: Option<u64>,
    step_count: usize,
    open_tool_calls: Vec<String>,
    open_permissions: Vec<String>,
    open_questions: Vec<String>,
    open_compactions: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OperationState {
    Open,
    TerminalKnown,
    TerminalUnknown,
}

/// Audit a `.jsonl` transcript and external [`LiveSessionActivity`] for stationary import readiness.
///
/// Fails closed with [`MigrationError::SessionNotStationary`] or
/// [`MigrationError::CorruptedTranscript`] on any live activity, open turn/step,
/// pending approval/question, unpaired/in-flight tool call, or corrupt JSONL record.
pub fn audit_jsonl_transcript(
    raw_jsonl: &[u8],
    activity: &LiveSessionActivity,
) -> Result<StationaryAudit, MigrationError> {
    audit_inner(raw_jsonl, None, activity)
}

/// Audit a `.jsonl` transcript while also enforcing an expected `sessionId`.
pub fn audit_jsonl_transcript_for_session(
    raw_jsonl: &[u8],
    expected_session_id: &str,
    activity: &LiveSessionActivity,
) -> Result<StationaryAudit, MigrationError> {
    validate_session_id(expected_session_id)?;
    audit_inner(raw_jsonl, Some(expected_session_id), activity)
}

fn audit_inner(
    raw_jsonl: &[u8],
    expected_session_id: Option<&str>,
    activity: &LiveSessionActivity,
) -> Result<StationaryAudit, MigrationError> {
    check_live_activity(activity)?;

    if raw_jsonl.len() > MAX_TRANSCRIPT_BYTES {
        return Err(MigrationError::CorruptedTranscript(format!(
            "transcript size {} exceeds {} bytes",
            raw_jsonl.len(),
            MAX_TRANSCRIPT_BYTES
        )));
    }

    let text = std::str::from_utf8(raw_jsonl).map_err(|err| {
        MigrationError::CorruptedTranscript(format!("transcript is not valid UTF-8: {err}"))
    })?;
    let source_sha256 = sha256_hex(raw_jsonl);

    let raw_entries = parse_jsonl_entries(text)?;
    let (source_session_id, last_sequence) = validate_envelopes(&raw_entries, expected_session_id)?;

    let turns = verify_stationary_domain_state(&raw_entries)?;
    let (messages, has_compaction_boundary) = reconstruct_messages(&raw_entries)?;
    verify_message_tool_pairs(&messages)?;

    let turn_summaries: Vec<TranscriptTurnSummary> = turns
        .into_iter()
        .map(|(turn_id, tracker)| TranscriptTurnSummary {
            turn_id,
            closed: tracker.closed,
            terminal_type: tracker.terminal_type,
            open_step: tracker.open_step,
            step_count: tracker.step_count,
        })
        .collect();

    Ok(StationaryAudit {
        source_session_id,
        source_sha256,
        entry_count: raw_entries.len(),
        message_count: messages.len(),
        last_sequence,
        turn_count: turn_summaries.len(),
        turns: turn_summaries,
        has_compaction_boundary,
        messages,
        raw_entries,
    })
}

fn check_live_activity(activity: &LiveSessionActivity) -> Result<(), MigrationError> {
    if activity.has_active_writer_lease {
        return Err(MigrationError::SessionNotStationary(
            "session holds an active writer lease".into(),
        ));
    }
    if activity.pending_permission_count > 0 {
        return Err(MigrationError::SessionNotStationary(format!(
            "session has {} pending permission approval(s)",
            activity.pending_permission_count
        )));
    }
    if activity.pending_question_count > 0 {
        return Err(MigrationError::SessionNotStationary(format!(
            "session has {} pending user question(s)",
            activity.pending_question_count
        )));
    }
    Ok(())
}

fn parse_jsonl_entries(text: &str) -> Result<Vec<JsValue>, MigrationError> {
    let mut entries = Vec::new();
    for (idx, raw_line) in text.split('\n').enumerate() {
        let line_no = idx + 1;
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if is_js_blank(line) {
            continue;
        }
        let parsed = parse_json(line).map_err(|err| {
            MigrationError::CorruptedTranscript(format!("line {line_no}: invalid JSON: {err}"))
        })?;
        if parsed.as_object().is_none() {
            return Err(MigrationError::CorruptedTranscript(format!(
                "line {line_no}: transcript entry must be a JSON object"
            )));
        }
        entries.push(parsed);
    }
    if entries.is_empty() {
        return Err(MigrationError::CorruptedTranscript(
            "transcript contains no entries; missing sessionId".into(),
        ));
    }
    Ok(entries)
}

fn validate_envelopes(
    entries: &[JsValue],
    expected_session_id: Option<&str>,
) -> Result<(String, Option<u64>), MigrationError> {
    let mut resolved_session: Option<String> = None;
    let mut saw_sequence: Option<bool> = None;
    let mut prev_sequence: u64 = 0;
    let mut seen_entry_ids: HashSet<JsString> = HashSet::new();

    for (idx, entry) in entries.iter().enumerate() {
        let line_no = idx + 1;

        let kind = require_nonblank_utf8(entry, "type", line_no)?;
        let _ = kind;

        let sid = require_nonblank_utf8(entry, "sessionId", line_no)?;
        validate_session_id(&sid).map_err(|err| {
            MigrationError::CorruptedTranscript(format!("line {line_no}: invalid sessionId: {err}"))
        })?;
        if let Some(expected) = expected_session_id
            && sid != expected
        {
            return Err(MigrationError::CorruptedTranscript(format!(
                "line {line_no}: sessionId {sid:?} != expected {expected:?}"
            )));
        }
        match &resolved_session {
            Some(first) if first != &sid => {
                return Err(MigrationError::CorruptedTranscript(format!(
                    "line {line_no}: mismatched sessionId {sid:?} != {first:?}"
                )));
            }
            None => resolved_session = Some(sid),
            _ => {}
        }

        let seq_val = field(entry, "sequence");
        let has_seq = !seq_val.is_undefined();
        match saw_sequence {
            None => saw_sequence = Some(has_seq),
            Some(prev) if prev != has_seq => {
                return Err(MigrationError::CorruptedTranscript(format!(
                    "line {line_no}: inconsistent presence of sequence field across transcript"
                )));
            }
            _ => {}
        }
        if has_seq {
            let seq = parse_positive_safe_u64(seq_val).ok_or_else(|| {
                MigrationError::CorruptedTranscript(format!(
                    "line {line_no}: sequence must be a positive safe integer"
                ))
            })?;
            if seq <= prev_sequence {
                return Err(MigrationError::CorruptedTranscript(format!(
                    "line {line_no}: sequence {seq} did not increase strictly after {prev_sequence}"
                )));
            }
            prev_sequence = seq;
        }

        let entry_id = field(entry, "entryId");
        if !entry_id.is_undefined() {
            let Some(id_str) = entry_id.as_string().filter(|s| !is_blank_js(s)) else {
                return Err(MigrationError::CorruptedTranscript(format!(
                    "line {line_no}: entryId must be a non-empty string when present"
                )));
            };
            if !seen_entry_ids.insert(id_str.clone()) {
                return Err(MigrationError::CorruptedTranscript(format!(
                    "line {line_no}: duplicate entryId {}",
                    String::from_utf16_lossy(id_str.units())
                )));
            }
            let parent_id = field(entry, "parentEntryId");
            if !parent_id.is_undefined() && !matches!(parent_id, JsValue::Null) {
                let Some(parent_str) = parent_id.as_string().filter(|s| !is_blank_js(s)) else {
                    return Err(MigrationError::CorruptedTranscript(format!(
                        "line {line_no}: parentEntryId must be null or a non-empty string"
                    )));
                };
                if parent_str == id_str {
                    return Err(MigrationError::CorruptedTranscript(format!(
                        "line {line_no}: entryId references itself as parentEntryId"
                    )));
                }
            }
        }
    }

    let session_id = resolved_session.ok_or_else(|| {
        MigrationError::CorruptedTranscript("transcript missing sessionId".into())
    })?;
    let last_sequence = (saw_sequence == Some(true)).then_some(prev_sequence);
    Ok((session_id, last_sequence))
}

fn verify_stationary_domain_state(
    entries: &[JsValue],
) -> Result<Vec<(String, TurnTracker)>, MigrationError> {
    let mut turns: Vec<(String, TurnTracker)> = Vec::new();
    let mut turn_index: HashMap<String, usize> = HashMap::new();
    let mut pending_queue: Vec<(String, String)> = Vec::new();
    let mut op_states: BTreeMap<String, OperationState> = BTreeMap::new();

    for (idx, entry) in entries.iter().enumerate() {
        let line_no = idx + 1;
        let kind = require_nonblank_utf8(entry, "type", line_no)?;

        if kind == "agent_turn_enqueued" {
            let item_id = require_nonblank_utf8(entry, "itemId", line_no)?;
            let turn_id = require_nonblank_utf8(entry, "turnId", line_no)?;
            pending_queue.push((item_id, turn_id));
            continue;
        }
        if kind == "agent_turn_discarded" {
            let item_id = require_nonblank_utf8(entry, "itemId", line_no)?;
            let turn_id = require_nonblank_utf8(entry, "turnId", line_no)?;
            let Some(pos) = pending_queue
                .iter()
                .position(|(item, turn)| item == &item_id && turn == &turn_id)
            else {
                return Err(MigrationError::CorruptedTranscript(format!(
                    "line {line_no}: agent_turn_discarded for unknown queued item {item_id:?}"
                )));
            };
            pending_queue.remove(pos);
            continue;
        }

        if matches!(
            kind.as_str(),
            "agent_loop_operation_started"
                | "agent_loop_operation_accepted"
                | "agent_loop_operation_terminal"
        ) {
            let op_id = require_nonblank_utf8(entry, "operationId", line_no)?;
            let turn_id = require_nonblank_utf8(entry, "turnId", line_no)?;
            let _ = get_or_insert_turn(&mut turns, &mut turn_index, &turn_id);
            match kind.as_str() {
                "agent_loop_operation_started" => {
                    op_states.entry(op_id).or_insert(OperationState::Open);
                }
                "agent_loop_operation_accepted" => {
                    if !op_states.contains_key(&op_id) {
                        return Err(MigrationError::CorruptedTranscript(format!(
                            "line {line_no}: agent_loop_operation_accepted before started for {op_id:?}"
                        )));
                    }
                }
                "agent_loop_operation_terminal" => {
                    if !op_states.contains_key(&op_id) {
                        return Err(MigrationError::CorruptedTranscript(format!(
                            "line {line_no}: agent_loop_operation_terminal before started for {op_id:?}"
                        )));
                    }
                    let outcome = require_nonblank_utf8(entry, "outcome", line_no)?;
                    let next_state = if outcome == "result_unknown" {
                        OperationState::TerminalUnknown
                    } else {
                        OperationState::TerminalKnown
                    };
                    op_states.insert(op_id, next_state);
                }
                _ => {}
            }
            continue;
        }

        if !is_turn_scoped_entry(&kind) {
            continue;
        }

        let turn_id = require_nonblank_utf8(entry, "turnId", line_no)?;

        if kind == "turn_started" {
            let slot = get_or_insert_turn(&mut turns, &mut turn_index, &turn_id);
            let tracker = &mut turns[slot].1;
            if tracker.started {
                return Err(MigrationError::CorruptedTranscript(format!(
                    "line {line_no}: duplicate turn_started for turn {turn_id:?}"
                )));
            }
            tracker.started = true;
            tracker.requires_closure = true;

            if let Some(claim) = field(entry, "inboxItemId")
                .as_string()
                .and_then(|s| s.to_utf8().ok())
                .filter(|s| !s.trim().is_empty())
            {
                if let Some(pos) = pending_queue
                    .iter()
                    .position(|(item, turn)| item == &claim && turn == &turn_id)
                {
                    pending_queue.remove(pos);
                }
            } else if let Some(pos) = pending_queue.iter().position(|(_, turn)| turn == &turn_id) {
                pending_queue.remove(pos);
            }
            continue;
        }

        let slot = get_or_insert_turn(&mut turns, &mut turn_index, &turn_id);
        let tracker = &mut turns[slot].1;

        if tracker.closed
            && !matches!(
                kind.as_str(),
                "turn_result"
                    | "assistant_message"
                    | "tool_result_message"
                    | "durable_message"
                    | "context_snapshot"
                    | "session_metadata"
                    | "agent_status_message"
                    | "file_artifacts"
                    | "control_boundary"
            )
        {
            return Err(MigrationError::CorruptedTranscript(format!(
                "line {line_no}: {kind} recorded after turn_result for closed turn {turn_id:?}"
            )));
        }

        match kind.as_str() {
            "accepted_input" | "assistant_message" | "tool_result_message" | "durable_message" => {
                tracker.requires_closure = true;
            }
            "step_started" => {
                let step = parse_positive_safe_u64(field(entry, "step")).ok_or_else(|| {
                    MigrationError::CorruptedTranscript(format!(
                        "line {line_no}: step_started missing positive safe integer step"
                    ))
                })?;
                if step <= tracker.last_step {
                    return Err(MigrationError::CorruptedTranscript(format!(
                        "line {line_no}: step {step} did not increase after {} in turn {turn_id:?}",
                        tracker.last_step
                    )));
                }
                if let Some(open) = tracker.open_step {
                    return Err(MigrationError::SessionNotStationary(format!(
                        "line {line_no}: turn {turn_id:?} opened step {step} while step {open} was still open"
                    )));
                }
                tracker.last_step = step;
                tracker.open_step = Some(step);
                tracker.step_count += 1;
                tracker.requires_closure = true;
            }
            "step_completed" => {
                let step = parse_positive_safe_u64(field(entry, "step")).ok_or_else(|| {
                    MigrationError::CorruptedTranscript(format!(
                        "line {line_no}: step_completed missing positive safe integer step"
                    ))
                })?;
                if tracker.open_step != Some(step) {
                    return Err(MigrationError::CorruptedTranscript(format!(
                        "line {line_no}: step_completed step {step} does not match open step {:?} in turn {turn_id:?}",
                        tracker.open_step
                    )));
                }
                let outcome = field(entry, "outcome")
                    .as_string()
                    .and_then(|s| s.to_utf8().ok())
                    .unwrap_or_default();
                if outcome == "completed" && !tracker.open_tool_calls.is_empty() {
                    return Err(MigrationError::SessionNotStationary(format!(
                        "line {line_no}: step {step} completed with {} unresolved tool call(s)",
                        tracker.open_tool_calls.len()
                    )));
                }
                tracker.open_step = None;
            }
            "tool_call" => {
                let call_id = extract_domain_tool_call_id(entry, line_no)?;
                if tracker.open_tool_calls.iter().any(|id| id == &call_id) {
                    return Err(MigrationError::CorruptedTranscript(format!(
                        "line {line_no}: duplicate open tool_call id {call_id:?}"
                    )));
                }
                tracker.open_tool_calls.push(call_id);
                tracker.requires_closure = true;
            }
            "tool_result" => {
                let call_id = extract_domain_tool_result_id(entry, line_no)?;
                let Some(pos) = tracker.open_tool_calls.iter().position(|id| id == &call_id) else {
                    return Err(MigrationError::CorruptedTranscript(format!(
                        "line {line_no}: tool_result {call_id:?} has no matching open tool_call"
                    )));
                };
                tracker.open_tool_calls.remove(pos);
                tracker.requires_closure = true;
            }
            "permission_started" | "permission_completed" | "permission_failed" => {
                let op_id = require_nonblank_utf8(entry, "operationId", line_no)?;
                update_bracket(
                    &mut tracker.open_permissions,
                    &kind,
                    "permission",
                    &op_id,
                    line_no,
                )?;
                tracker.requires_closure = true;
            }
            "question_started" | "question_completed" | "question_failed" => {
                let op_id = require_nonblank_utf8(entry, "operationId", line_no)?;
                update_bracket(
                    &mut tracker.open_questions,
                    &kind,
                    "question",
                    &op_id,
                    line_no,
                )?;
                tracker.requires_closure = true;
            }
            "compaction_started" | "compaction_completed" | "compaction_failed" => {
                let op_id = require_nonblank_utf8(entry, "operationId", line_no)?;
                update_bracket(
                    &mut tracker.open_compactions,
                    &kind,
                    "compaction",
                    &op_id,
                    line_no,
                )?;
                tracker.requires_closure = true;
            }
            "turn_result" => {
                let terminal_type = validate_turn_result_payload(entry, line_no)?;
                if let Some(open) = tracker.open_step {
                    return Err(MigrationError::SessionNotStationary(format!(
                        "turn {turn_id:?} reached turn_result with step {open} still open"
                    )));
                }
                if !tracker.open_tool_calls.is_empty() {
                    return Err(pending_bracket_error(
                        &turn_id,
                        &tracker.open_tool_calls,
                        "closed with unresolved tool call(s)",
                    ));
                }
                if !tracker.open_permissions.is_empty() {
                    return Err(pending_bracket_error(
                        &turn_id,
                        &tracker.open_permissions,
                        "closed with pending permission(s)",
                    ));
                }
                if !tracker.open_questions.is_empty() {
                    return Err(pending_bracket_error(
                        &turn_id,
                        &tracker.open_questions,
                        "closed with pending question(s)",
                    ));
                }
                if !tracker.open_compactions.is_empty() {
                    return Err(pending_bracket_error(
                        &turn_id,
                        &tracker.open_compactions,
                        "closed with pending compaction(s)",
                    ));
                }
                tracker.closed = true;
                tracker.terminal_type = Some(terminal_type);
            }
            _ => {}
        }
    }

    for (turn_id, tracker) in &turns {
        if !tracker.open_permissions.is_empty() {
            return Err(pending_bracket_error(
                turn_id,
                &tracker.open_permissions,
                "has pending permission approval(s)",
            ));
        }
        if !tracker.open_questions.is_empty() {
            return Err(pending_bracket_error(
                turn_id,
                &tracker.open_questions,
                "has pending question(s)",
            ));
        }
        if !tracker.open_compactions.is_empty() {
            return Err(pending_bracket_error(
                turn_id,
                &tracker.open_compactions,
                "has pending compaction(s)",
            ));
        }
        if !tracker.open_tool_calls.is_empty() {
            return Err(MigrationError::SessionNotStationary(format!(
                "turn {turn_id:?} has in-flight unpaired tool call(s): {:?}",
                tracker.open_tool_calls
            )));
        }
        if let Some(open_step) = tracker.open_step {
            return Err(MigrationError::SessionNotStationary(format!(
                "turn {turn_id:?} has unclosed step {open_step}"
            )));
        }
        if tracker.requires_closure && !tracker.closed {
            return Err(MigrationError::SessionNotStationary(format!(
                "turn {turn_id:?} opened without terminal completion/settlement/abort closure"
            )));
        }
    }

    if !pending_queue.is_empty() {
        return Err(MigrationError::SessionNotStationary(format!(
            "session has {} pending queued turn(s)",
            pending_queue.len()
        )));
    }
    for (op_id, state) in &op_states {
        match state {
            OperationState::Open => {
                return Err(MigrationError::SessionNotStationary(format!(
                    "agent loop operation {op_id:?} is still open without terminal closure"
                )));
            }
            OperationState::TerminalUnknown => {
                return Err(MigrationError::SessionNotStationary(format!(
                    "agent loop operation {op_id:?} ended with result_unknown; reconciliation required"
                )));
            }
            OperationState::TerminalKnown => {}
        }
    }

    Ok(turns)
}

fn validate_turn_result_payload(entry: &JsValue, line_no: usize) -> Result<String, MigrationError> {
    let result = field(entry, "result");
    if result.is_undefined() {
        return Ok("settled".to_string());
    }
    if matches!(result, JsValue::Null) || result.as_object().is_none() {
        return Err(MigrationError::CorruptedTranscript(format!(
            "line {line_no}: turn_result.result must be an object"
        )));
    }
    let kind_str = field(result, "type")
        .as_string()
        .or_else(|| field(result, "outcome").as_string())
        .or_else(|| field(result, "status").as_string())
        .and_then(|s| s.to_utf8().ok())
        .unwrap_or_else(|| "settled".to_string());

    if matches!(
        kind_str.as_str(),
        "result_unknown" | "unknown" | "pending" | "in_progress"
    ) {
        return Err(MigrationError::SessionNotStationary(format!(
            "line {line_no}: turn_result has non-stationary outcome {kind_str:?}"
        )));
    }
    Ok(kind_str)
}

/// Fail closed with the pending-bracket detail for a turn holding open operations.
fn pending_bracket_error(turn_id: &str, pending: &[String], detail: &str) -> MigrationError {
    MigrationError::SessionNotStationary(format!("turn {turn_id:?} {detail}: {pending:?}"))
}

/// Decode a JS string value, rejecting blank or whitespace-only text.
fn nonblank_js_string(raw: &JsValue) -> Option<String> {
    raw.as_string()
        .and_then(|s| s.to_utf8().ok())
        .filter(|s| !s.trim().is_empty())
}

/// Require a non-empty transcript identifier from a raw JSON value.
fn require_nonblank_id(
    raw: &JsValue,
    line_no: usize,
    missing: &str,
) -> Result<String, MigrationError> {
    nonblank_js_string(raw)
        .ok_or_else(|| MigrationError::CorruptedTranscript(format!("line {line_no}: {missing}")))
}

fn update_bracket(
    open: &mut Vec<String>,
    entry_kind: &str,
    family: &str,
    op_id: &str,
    line_no: usize,
) -> Result<(), MigrationError> {
    if entry_kind.ends_with("_started") {
        if open.iter().any(|existing| existing == op_id) {
            return Err(MigrationError::CorruptedTranscript(format!(
                "line {line_no}: duplicate {family}_started for operation {op_id:?}"
            )));
        }
        open.push(op_id.to_string());
        return Ok(());
    }
    let Some(pos) = open.iter().position(|existing| existing == op_id) else {
        return Err(MigrationError::CorruptedTranscript(format!(
            "line {line_no}: {entry_kind} for {op_id:?} without matching {family}_started"
        )));
    };
    open.remove(pos);
    Ok(())
}

fn extract_domain_tool_call_id(entry: &JsValue, line_no: usize) -> Result<String, MigrationError> {
    let call = field(entry, "call");
    let raw = if !call.is_undefined() {
        field(call, "id")
    } else {
        let tc = field(entry, "toolCallId");
        if !tc.is_undefined() {
            tc
        } else {
            field(entry, "id")
        }
    };
    require_nonblank_id(raw, line_no, "tool_call missing non-empty call id")
}

fn extract_domain_tool_result_id(
    entry: &JsValue,
    line_no: usize,
) -> Result<String, MigrationError> {
    let result = field(entry, "result");
    let raw = if !result.is_undefined() {
        let direct = field(result, "toolCallId");
        if !direct.is_undefined() {
            direct
        } else {
            field(result, "tool_use_id")
        }
    } else {
        let direct = field(entry, "toolCallId");
        if !direct.is_undefined() {
            direct
        } else {
            field(entry, "tool_use_id")
        }
    };
    require_nonblank_id(raw, line_no, "tool_result missing non-empty toolCallId")
}

fn get_or_insert_turn(
    turns: &mut Vec<(String, TurnTracker)>,
    index: &mut HashMap<String, usize>,
    turn_id: &str,
) -> usize {
    if let Some(&slot) = index.get(turn_id) {
        return slot;
    }
    let slot = turns.len();
    turns.push((turn_id.to_string(), TurnTracker::default()));
    index.insert(turn_id.to_string(), slot);
    slot
}

fn is_turn_scoped_entry(kind: &str) -> bool {
    matches!(
        kind,
        "turn_started"
            | "turn_result"
            | "step_started"
            | "step_completed"
            | "context_snapshot"
            | "agent_instructions"
            | "model_request"
            | "model_stream_event"
            | "tool_call"
            | "tool_result"
            | "permission_started"
            | "permission_completed"
            | "permission_failed"
            | "question_started"
            | "question_completed"
            | "question_failed"
            | "compaction_started"
            | "compaction_completed"
            | "compaction_failed"
            | "accepted_input"
            | "assistant_message"
            | "tool_result_message"
            | "durable_message"
    )
}

fn reconstruct_messages(entries: &[JsValue]) -> Result<(Vec<JsValue>, bool), MigrationError> {
    let mut replacement: Option<(usize, Vec<JsValue>)> = None;
    for (idx, entry) in entries.iter().enumerate() {
        let boundary = field(entry, "boundary");
        if string_is(field(entry, "type"), "control_boundary")
            && string_is(field(boundary, "kind"), "compact")
            && string_is(field(boundary, "subtype"), "compact_boundary")
        {
            let snapshot = field(boundary, "snapshot");
            let snapshot_msgs = field(snapshot, "messages");
            let use_snapshot = field(snapshot, "version").as_number() == Some(1.0)
                && matches!(snapshot_msgs, JsValue::Array(items) if !items.is_empty());
            let selected = if use_snapshot {
                snapshot_msgs
            } else {
                field(boundary, "replacementMessages")
            };
            if let JsValue::Array(items) = selected {
                let mut cloned = Vec::with_capacity(items.len());
                for item in items {
                    cloned.push(validate_and_clone_message(item, false, idx + 1)?);
                }
                replacement = Some((idx, cloned));
            }
        }
    }

    let boundary_idx = replacement.as_ref().map(|(i, _)| *i);
    let mut messages = Vec::new();

    for (idx, entry) in entries.iter().enumerate() {
        let line_no = idx + 1;
        let before_boundary = boundary_idx.is_some_and(|b| idx < b);
        let kind = field(entry, "type")
            .as_string()
            .and_then(|s| s.to_utf8().ok())
            .unwrap_or_default();

        if kind == "accepted_input" && !before_boundary {
            let JsValue::Array(items) = field(entry, "messages") else {
                return Err(MigrationError::CorruptedTranscript(format!(
                    "line {line_no}: accepted_input.messages must be an array"
                )));
            };
            for item in items {
                messages.push(validate_and_clone_message(item, true, line_no)?);
            }
        } else if matches!(
            kind.as_str(),
            "assistant_message" | "tool_result_message" | "durable_message"
        ) && !before_boundary
        {
            let msg = field(entry, "message");
            let cloned = validate_and_clone_message(msg, false, line_no)?;
            if field(field(&cloned, "metadata"), "compactReplacement") != &JsValue::Bool(true) {
                messages.push(cloned);
            }
        } else if kind == "context_snapshot"
            && !before_boundary
            && let JsValue::Array(runtime_msgs) = field(entry, "runtimeContextMessages")
            && !runtime_msgs.is_empty()
        {
            let mut cloned = Vec::with_capacity(runtime_msgs.len());
            for item in runtime_msgs {
                cloned.push(validate_and_clone_message(item, false, line_no)?);
            }
            let at = messages
                .iter()
                .rposition(is_real_user_message)
                .unwrap_or(messages.len());
            messages.splice(at..at, cloned);
        }

        if let Some((b_idx, ref boundary_msgs)) = replacement
            && idx == b_idx
        {
            messages.extend(boundary_msgs.iter().cloned());
        }
    }

    Ok((messages, boundary_idx.is_some()))
}

fn validate_and_clone_message(
    message: &JsValue,
    default_user_role_if_missing: bool,
    line_no: usize,
) -> Result<JsValue, MigrationError> {
    let Some(obj) = message.as_object() else {
        return Err(MigrationError::CorruptedTranscript(format!(
            "line {line_no}: message entry must be an object"
        )));
    };
    let raw_content = obj.get("content").ok_or_else(|| {
        MigrationError::CorruptedTranscript(format!(
            "line {line_no}: message missing content field"
        ))
    })?;
    if !matches!(raw_content, JsValue::String(_) | JsValue::Array(_)) {
        return Err(MigrationError::CorruptedTranscript(format!(
            "line {line_no}: message content must be an array or string"
        )));
    }
    if default_user_role_if_missing && !obj.contains_key("role") {
        let mut cloned: JsObject = obj.clone();
        cloned.insert("role".into(), "user".into());
        return Ok(JsValue::Object(cloned));
    }
    Ok(message.clone())
}

fn is_real_user_message(message: &JsValue) -> bool {
    is_role(message, "user")
        && field(field(message, "metadata"), "synthetic") != &JsValue::Bool(true)
        && content(message).iter().any(|block| {
            let kind = block_type(block);
            if kind.is_some_and(|k| {
                k.equals_str("tool_result")
                    || k.equals_str("tool_result_reference")
                    || (k.equals_str("media_reference")
                        && string_field(block, "toolCallId").is_some())
            }) {
                return false;
            }
            if !is_block(block, "text") {
                return true;
            }
            let Some(text) = string_field(block, "text") else {
                return false;
            };
            let trimmed = String::from_utf16_lossy(
                pilotdeck_context_core::wire::trim_units(text.units()),
            );
            !trimmed.is_empty()
                && trimmed
                    != "[system: the conversation above has been compacted. please continue with the current task.]"
                && ![
                    "<compact-boundary",
                    "<snip-boundary",
                    "<memory-context>",
                    "<internal-compaction-control",
                    "<hook_context",
                ]
                .iter()
                .any(|p| trimmed.starts_with(p))
        })
}

fn verify_message_tool_pairs(messages: &[JsValue]) -> Result<(), MigrationError> {
    if let Err(err) = validate_tool_pair_integrity(messages) {
        return Err(match err {
            LedgerError::ToolPairIntegrity(detail)
                if detail.contains("orphaned tool_call/tool_use without matching tool_result")
                    || detail.contains("trailing unpaired tool_call without tool_result")
                    || detail.contains(
                        "assistant turn arrived before pending tool_call results were provided",
                    ) =>
            {
                MigrationError::SessionNotStationary(detail)
            }
            LedgerError::ToolPairIntegrity(detail) => MigrationError::CorruptedTranscript(detail),
            other => MigrationError::Ledger(other),
        });
    }
    Ok(())
}

fn require_nonblank_utf8(
    value: &JsValue,
    key: &str,
    line_no: usize,
) -> Result<String, MigrationError> {
    let raw = field(value, key).as_string().ok_or_else(|| {
        MigrationError::CorruptedTranscript(format!("line {line_no}: missing string field {key:?}"))
    })?;
    let text = raw.to_utf8().map_err(|_| {
        MigrationError::CorruptedTranscript(format!(
            "line {line_no}: field {key:?} contains invalid UTF-16 surrogates"
        ))
    })?;
    if is_js_blank(&text) {
        return Err(MigrationError::CorruptedTranscript(format!(
            "line {line_no}: field {key:?} must not be empty or whitespace"
        )));
    }
    Ok(text)
}

fn string_is(value: &JsValue, expected: &str) -> bool {
    value.as_string().is_some_and(|s| s.equals_str(expected))
}

fn is_blank_js(value: &JsString) -> bool {
    value.units().iter().copied().all(is_js_whitespace)
}

fn is_js_blank(text: &str) -> bool {
    text.encode_utf16().all(is_js_whitespace)
}

fn parse_positive_safe_u64(value: &JsValue) -> Option<u64> {
    let n = value.as_number()?;
    if n.is_finite() && n.fract() == 0.0 && (1.0..=MAX_SAFE_INTEGER).contains(&n) {
        Some(n as u64)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stationary_accepts_complete_transcript_with_tool_pair() {
        let jsonl = concat!(
            r#"{"type":"turn_started","sessionId":"sess-1","turnId":"t1","sequence":1,"createdAt":"2026-11-16T00:00:00Z"}"#,
            "\n",
            r#"{"type":"accepted_input","sessionId":"sess-1","turnId":"t1","sequence":2,"createdAt":"2026-11-16T00:00:01Z","messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#,
            "\n",
            r#"{"type":"step_started","sessionId":"sess-1","turnId":"t1","step":1,"sequence":3,"createdAt":"2026-11-16T00:00:02Z"}"#,
            "\n",
            r#"{"type":"tool_call","sessionId":"sess-1","turnId":"t1","step":1,"call":{"id":"call-1","name":"read_file"},"sequence":4,"createdAt":"2026-11-16T00:00:03Z"}"#,
            "\n",
            r#"{"type":"assistant_message","sessionId":"sess-1","turnId":"t1","sequence":5,"createdAt":"2026-11-16T00:00:04Z","message":{"role":"assistant","content":[{"type":"tool_call","id":"call-1","name":"read_file","input":{}}]}}"#,
            "\n",
            r#"{"type":"tool_result","sessionId":"sess-1","turnId":"t1","step":1,"result":{"toolCallId":"call-1"},"sequence":6,"createdAt":"2026-11-16T00:00:05Z"}"#,
            "\n",
            r#"{"type":"tool_result_message","sessionId":"sess-1","turnId":"t1","sequence":7,"createdAt":"2026-11-16T00:00:06Z","message":{"role":"user","content":[{"type":"tool_result","toolCallId":"call-1","content":[{"type":"text","text":"ok"}]}]}}"#,
            "\n",
            r#"{"type":"step_completed","sessionId":"sess-1","turnId":"t1","step":1,"outcome":"completed","sequence":8,"createdAt":"2026-11-16T00:00:07Z"}"#,
            "\n",
            r#"{"type":"turn_result","sessionId":"sess-1","turnId":"t1","sequence":9,"createdAt":"2026-11-16T00:00:08Z","result":{"type":"success","usage":{},"permissionDenials":[]}}"#,
            "\n"
        );
        let audit =
            audit_jsonl_transcript(jsonl.as_bytes(), &LiveSessionActivity::default()).unwrap();
        assert_eq!(audit.source_session_id, "sess-1");
        assert_eq!(audit.entry_count, 9);
        assert_eq!(audit.message_count, 3);
        assert_eq!(audit.last_sequence, Some(9));
        assert_eq!(audit.turn_count, 1);
        assert!(audit.turns[0].closed);
    }

    #[test]
    fn rejects_active_writer_lease_pending_approvals_and_open_turn() {
        let closed_jsonl = concat!(
            r#"{"type":"turn_started","sessionId":"sess-1","turnId":"t1","sequence":1}"#,
            "\n",
            r#"{"type":"turn_result","sessionId":"sess-1","turnId":"t1","sequence":2,"result":{"type":"aborted"}}"#,
            "\n"
        );

        let err = audit_jsonl_transcript(
            closed_jsonl.as_bytes(),
            &LiveSessionActivity {
                has_active_writer_lease: true,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, MigrationError::SessionNotStationary(_)));

        let err = audit_jsonl_transcript(
            closed_jsonl.as_bytes(),
            &LiveSessionActivity {
                pending_permission_count: 1,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, MigrationError::SessionNotStationary(_)));

        let err = audit_jsonl_transcript(
            closed_jsonl.as_bytes(),
            &LiveSessionActivity {
                pending_question_count: 2,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, MigrationError::SessionNotStationary(_)));

        let open_turn =
            r#"{"type":"turn_started","sessionId":"sess-1","turnId":"t1","sequence":1}"#;
        let err = audit_jsonl_transcript(open_turn.as_bytes(), &LiveSessionActivity::default())
            .unwrap_err();
        assert!(matches!(err, MigrationError::SessionNotStationary(_)));
    }

    #[test]
    fn rejects_corrupt_truncated_jsonl_and_empty_session_id() {
        let truncated = r#"{"type":"turn_started","sessionId":"sess-1","turnId":"t1""#;
        let err = audit_jsonl_transcript(truncated.as_bytes(), &LiveSessionActivity::default())
            .unwrap_err();
        assert!(matches!(err, MigrationError::CorruptedTranscript(_)));

        let empty_sid = r#"{"type":"turn_started","sessionId":"","turnId":"t1","sequence":1}"#;
        let err = audit_jsonl_transcript(empty_sid.as_bytes(), &LiveSessionActivity::default())
            .unwrap_err();
        assert!(matches!(err, MigrationError::CorruptedTranscript(_)));
    }
}
