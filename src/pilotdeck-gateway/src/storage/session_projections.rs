//! Session read model: the transcript reducers behind the baseline's
//! `<session>.jsonl.projections.json` checkpoint and its web history reader.
//!
//! Baseline sources (resources/PilotDeck/src):
//!  - session/projection/AgentTranscriptProjections.ts (conversation,
//!    compact-boundary, subagent-references)
//!  - session/projection/FileHistoryProjection.ts
//!  - session/projection/WebHistoryProjections.ts
//!  - plan-todo/projection/PlanTodoProjection.ts
//!  - goal/projection/GoalProjection.ts
//!  - session/projection/BuiltinSessionProjections.ts:8-16 (registration order)
//!
//! Every function here is a pure replay over transcript entries in the order
//! the caller holds them. The `turn-summary` and `metadata` rows are produced
//! by the session factory's live state and are not recomputed here.
//!
//! Malformed entries: the baseline reducers throw on shapes the transcript
//! writer never produces (e.g. a `control_boundary` without `boundary`), and
//! the driver then drops that projection's cell. Only the goal reducer's
//! explicit validation is mirrored as "row absent"; the other reducers treat
//! such an entry as not matching.
use pilotdeck_protocol::{JsObject, JsString, JsValue};

use super::transcript_adapter::compact_snapshot_messages;

static UNDEFINED: JsValue = JsValue::Undefined;

pub(crate) fn field<'a>(value: &'a JsValue, key: &str) -> &'a JsValue {
    value.get(key).unwrap_or(&UNDEFINED)
}
pub(crate) fn is(value: &JsValue, text: &str) -> bool {
    value.as_string().is_some_and(|s| s.equals_str(text))
}
/// An object literal; `undefined` values are left out, as `JSON.stringify`
/// would drop them.
pub(crate) fn object<const N: usize>(fields: [(&str, JsValue); N]) -> JsValue {
    JsValue::Object(object_map(fields))
}
fn object_map<const N: usize>(fields: [(&str, JsValue); N]) -> JsObject {
    let mut out = JsObject::new();
    for (key, value) in fields {
        if !value.is_undefined() {
            out.insert(key.into(), value);
        }
    }
    out
}
pub(crate) fn truthy(value: &JsValue) -> bool {
    match value {
        JsValue::Undefined | JsValue::Null => false,
        JsValue::Bool(value) => *value,
        JsValue::Number(value) => *value != 0.0 && !value.is_nan(),
        JsValue::String(value) => !value.is_empty(),
        JsValue::Array(_) | JsValue::Object(_) => true,
    }
}
/// `positiveNumber` (WebHistoryProjections.ts:412-414).
pub(crate) fn positive_number(value: &JsValue) -> Option<f64> {
    value.as_number().filter(|n| n.is_finite() && *n > 0.0)
}
fn number(value: f64) -> JsValue {
    JsValue::Number(value)
}
fn array_of(value: &JsValue) -> &[JsValue] {
    match value {
        JsValue::Array(items) => items,
        _ => &[],
    }
}
/// `new Set([...set, value])`: insertion order, one slot per distinct value.
fn add_unique(set: &mut Vec<JsValue>, value: &JsValue) {
    if !set.contains(value) {
        set.push(value.clone());
    }
}

pub(crate) fn is_durable_message_entry(entry: &JsValue) -> bool {
    let kind = field(entry, "type");
    is(kind, "assistant_message") || is(kind, "tool_result_message") || is(kind, "durable_message")
}
/// The `kind === "compact" && "subtype" in boundary && subtype ===
/// "compact_boundary"` guard shared by the reducers; yields the boundary.
fn compact_boundary(entry: &JsValue) -> Option<&JsValue> {
    let boundary = field(entry, "boundary");
    (is(field(entry, "type"), "control_boundary")
        && is(field(boundary, "kind"), "compact")
        && is(field(boundary, "subtype"), "compact_boundary"))
    .then_some(boundary)
}
/// `readCompactSnapshot` (CompactSnapshot.ts:5-17) for a compact boundary.
fn compact_snapshot(boundary: &JsValue) -> Option<&JsValue> {
    compact_snapshot_messages(boundary).ok().flatten()
}
/// `cloneMessages` (model/protocol/clone.ts:43-52) as far as JSON can tell:
/// a message without an array `content` gains an empty one.
fn clone_messages(messages: &JsValue) -> JsValue {
    JsValue::Array(
        array_of(messages)
            .iter()
            .map(|message| {
                let mut copy = message.as_object().cloned().unwrap_or_default();
                if !matches!(field(message, "content"), JsValue::Array(_)) {
                    copy.insert("content".into(), JsValue::Array(vec![]));
                }
                JsValue::Object(copy)
            })
            .collect(),
    )
}

/// `agent-transcript.conversation` checkpoint state
/// (AgentTranscriptProjections.ts:86-137, encode :230-246).
fn conversation(entries: &[JsValue]) -> JsValue {
    let mut kept = Vec::new();
    let mut completed = Vec::new();
    let mut last_boundary = -1.0;
    let mut replacement = JsValue::Undefined;
    for (index, entry) in entries.iter().enumerate() {
        let kind = field(entry, "type");
        let keep = if is(kind, "accepted_input") || is_durable_message_entry(entry) {
            true
        } else if is(kind, "context_snapshot") {
            !array_of(field(entry, "runtimeContextMessages")).is_empty()
        } else if is(kind, "turn_result") {
            add_unique(&mut completed, field(entry, "turnId"));
            true
        } else if let Some(boundary) = compact_boundary(entry) {
            let snapshot = compact_snapshot(boundary);
            let messages = snapshot.unwrap_or(field(boundary, "replacementMessages"));
            if truthy(messages) {
                last_boundary = index as f64;
                replacement = object([
                    ("index", number(index as f64)),
                    ("turnId", field(entry, "turnId").clone()),
                    ("messages", clone_messages(messages)),
                    (
                        "replayBeforeTurnCompletion",
                        JsValue::Bool(snapshot.is_some()),
                    ),
                ]);
                true
            } else {
                boundary
                    .as_object()
                    .is_some_and(|boundary| boundary.contains_key("snapshot"))
            }
        } else {
            false
        };
        if keep {
            kept.push(object([
                ("entry", entry.clone()),
                ("index", number(index as f64)),
            ]));
        }
    }
    object([
        ("entries", JsValue::Array(kept)),
        ("completedTurnIds", JsValue::Array(completed)),
        ("lastCompactBoundaryIndex", number(last_boundary)),
        ("lastCompactReplacement", replacement),
    ])
}

/// `agent-transcript.compact-boundary` (AgentTranscriptProjections.ts:339-354).
fn compact_boundary_state(entries: &[JsValue]) -> JsValue {
    let last = entries.iter().rev().find(|entry| {
        compact_boundary(entry).is_some_and(|boundary| {
            compact_snapshot(boundary).is_some() || truthy(field(boundary, "replacementMessages"))
        })
    });
    object([(
        "lastCompactBoundary",
        last.cloned().unwrap_or(JsValue::Undefined),
    )])
}

fn entries_of_type(entries: &[JsValue], kind: &str) -> Vec<JsValue> {
    entries
        .iter()
        .filter(|entry| is(field(entry, "type"), kind))
        .cloned()
        .collect()
}

/// `agent-transcript.subagent-references` (AgentTranscriptProjections.ts:380-413).
fn subagent_references(entries: &[JsValue]) -> JsValue {
    object([
        (
            "started",
            JsValue::Array(entries_of_type(entries, "subagent_started")),
        ),
        (
            "completed",
            JsValue::Array(entries_of_type(entries, "subagent_completed")),
        ),
    ])
}

/// `file-history.snapshots` (FileHistoryProjection.ts:17-43): one slot per
/// `messageId`, the latest record replacing the earlier one in place.
fn file_history(entries: &[JsValue]) -> JsValue {
    let mut kept: Vec<JsValue> = Vec::new();
    for entry in entries {
        if !is(field(entry, "type"), "file_snapshot_recorded") {
            continue;
        }
        let id = field(entry, "messageId");
        match kept
            .iter()
            .position(|existing| field(existing, "messageId") == id)
        {
            Some(index) => kept[index] = entry.clone(),
            None => kept.push(entry.clone()),
        }
    }
    object([("entries", JsValue::Array(kept))])
}

/// The five `web-history.*` reducer states (WebHistoryProjections.ts).
#[derive(Default)]
pub(crate) struct WebHistory {
    file_artifacts: Vec<JsValue>,
    turns_with_tool_results: Vec<JsValue>,
    pub(crate) agent_statuses: Vec<JsValue>,
    turn_errors: Vec<JsValue>,
    visible_failure_turns: Vec<JsValue>,
    /// `latestContextBudget` / `latestCompactBudget` / `latestTurnUsage`, in
    /// first-assignment order like the reducer's object spread.
    pub(crate) token_usage: JsObject,
    durable_message_turns: Vec<JsValue>,
    completed_turns: Vec<JsValue>,
}

impl WebHistory {
    pub(crate) fn project(entries: &[JsValue]) -> Self {
        let mut state = Self::default();
        for (index, entry) in entries.iter().enumerate() {
            state.reduce(entry, index);
        }
        state
    }

    fn reduce(&mut self, entry: &JsValue, index: usize) {
        let kind = field(entry, "type");
        let turn = field(entry, "turnId");
        let durable = is_durable_message_entry(entry);
        // fileArtifactProjection (:73-87)
        if durable
            && array_of(field(field(entry, "message"), "content"))
                .iter()
                .any(|block| {
                    is(field(block, "type"), "tool_result")
                        || is(field(block, "type"), "tool_result_reference")
                })
        {
            add_unique(&mut self.turns_with_tool_results, turn);
        } else if is(kind, "file_artifacts") && !array_of(field(entry, "artifacts")).is_empty() {
            self.file_artifacts.push(entry.clone());
        }
        // agentStatusProjection (:120-127) and turnErrorProjection (:144-162)
        if is(kind, "agent_status_message") {
            self.agent_statuses.push(entry.clone());
            if is(field(entry, "kind"), "error")
                && field(field(entry, "detail"), "visible") != &JsValue::Bool(false)
            {
                add_unique(&mut self.visible_failure_turns, turn);
            }
        } else if is(kind, "turn_result") && is(field(field(entry, "result"), "type"), "error") {
            self.turn_errors.push(entry.clone());
        }
        reduce_token_usage(&mut self.token_usage, entry, index);
        // incompleteTurnProjection (:223-237)
        if is(kind, "turn_result") {
            add_unique(&mut self.completed_turns, turn);
        } else if durable {
            add_unique(&mut self.durable_message_turns, turn);
        }
    }

    /// fileArtifactProjection.finalize (:88-95): a turn without tool results
    /// only shows artifacts that did not come from the workspace diff.
    pub(crate) fn file_artifacts(&self) -> Vec<JsValue> {
        self.file_artifacts
            .iter()
            .filter_map(|entry| {
                let all = array_of(field(entry, "artifacts"));
                let artifacts: Vec<JsValue> = if self
                    .turns_with_tool_results
                    .contains(field(entry, "turnId"))
                {
                    all.to_vec()
                } else {
                    all.iter()
                        .filter(|artifact| !is(field(artifact, "source"), "workspace_diff"))
                        .cloned()
                        .collect()
                };
                if artifacts.is_empty() {
                    return None;
                }
                let mut copy = entry.as_object().cloned().unwrap_or_default();
                copy.insert("artifacts".into(), JsValue::Array(artifacts));
                Some(JsValue::Object(copy))
            })
            .collect()
    }

    /// turnErrorProjection.finalize (:163-165).
    pub(crate) fn turn_errors(&self) -> Vec<&JsValue> {
        self.turn_errors
            .iter()
            .filter(|entry| !self.visible_failure_turns.contains(field(entry, "turnId")))
            .collect()
    }

    /// incompleteTurnProjection.finalize (:238-240).
    pub(crate) fn incomplete_turn_ids(&self) -> Vec<JsValue> {
        self.durable_message_turns
            .iter()
            .filter(|turn| !self.completed_turns.contains(turn))
            .cloned()
            .collect()
    }

    fn file_artifacts_state(&self) -> JsValue {
        object([
            ("entries", JsValue::Array(self.file_artifacts.clone())),
            (
                "turnsWithToolResults",
                JsValue::Array(self.turns_with_tool_results.clone()),
            ),
        ])
    }
    fn turn_errors_state(&self) -> JsValue {
        object([
            ("entries", JsValue::Array(self.turn_errors.clone())),
            (
                "visibleFailureStatusTurnIds",
                JsValue::Array(self.visible_failure_turns.clone()),
            ),
        ])
    }
    fn incomplete_turns_state(&self) -> JsValue {
        object([
            (
                "durableMessageTurnIds",
                JsValue::Array(self.durable_message_turns.clone()),
            ),
            (
                "completedTurnIds",
                JsValue::Array(self.completed_turns.clone()),
            ),
        ])
    }
}

/// `projectContextBudget` (WebHistoryProjections.ts:343-381).
fn context_budget(entry: &JsValue, index: usize) -> Option<JsValue> {
    if !is(field(entry, "type"), "agent_status_message")
        || !is(field(entry, "event"), "context_budget")
    {
        return None;
    }
    let detail = field(entry, "detail");
    detail.as_object()?;
    let display_used = positive_number(field(detail, "displayUsed"));
    let used = display_used.or_else(|| positive_number(field(detail, "used")))?;
    let total = positive_number(field(detail, "total"))?;
    let effective_total = positive_number(field(detail, "effectiveTotal")).unwrap_or(total);
    let optional = |value: Option<f64>| value.map_or(JsValue::Undefined, number);
    let state = field(detail, "state");
    let ratio = field(detail, "ratio");
    Some(object([
        ("index", number(index as f64)),
        (
            "usage",
            object([
                ("used", number(used)),
                ("displayUsed", optional(display_used)),
                (
                    "budgetUsed",
                    optional(positive_number(field(detail, "budgetUsed"))),
                ),
                ("total", number(total)),
                ("effectiveTotal", number(effective_total)),
                (
                    "reservedOutputTokens",
                    number(positive_number(field(detail, "reservedOutputTokens")).unwrap_or(0.0)),
                ),
                (
                    "state",
                    if state.as_string().is_some() {
                        state.clone()
                    } else {
                        JsValue::Undefined
                    },
                ),
                (
                    "ratio",
                    if ratio.as_number().is_some_and(f64::is_finite) {
                        ratio.clone()
                    } else {
                        JsValue::Undefined
                    },
                ),
                ("source", "history".into()),
                ("exact", JsValue::Bool(true)),
            ]),
        ),
    ]))
}

/// `projectCompactBudget` (WebHistoryProjections.ts:383-409).
fn compact_budget(entry: &JsValue, index: usize) -> Option<JsValue> {
    let metadata = field(compact_boundary(entry)?, "compactMetadata");
    let post_tokens = positive_number(field(metadata, "postTokens"))?;
    let optional =
        |key: &str| positive_number(field(metadata, key)).map_or(JsValue::Undefined, number);
    Some(object([
        ("index", number(index as f64)),
        ("postTokens", number(post_tokens)),
        ("preTokens", optional("preTokens")),
        ("messagesSummarized", optional("messagesSummarized")),
    ]))
}

// ── plan-todo.state (PlanTodoProjection.ts) ──

fn plan_todo_initial() -> JsObject {
    let zero = || number(0.0);
    object_map([
        ("requiresInitialization", JsValue::Bool(false)),
        ("toolCallsSinceLastTodoWrite", zero()),
        ("todos", JsValue::Array(vec![])),
        ("activeTodos", JsValue::Array(vec![])),
        ("todoHistory", JsValue::Array(vec![])),
        (
            "todoDiagnostics",
            object([
                ("writeCount", zero()),
                ("todoCount", zero()),
                ("activeCount", zero()),
                ("completedCount", zero()),
                ("cancelledCount", zero()),
                ("largeRewriteCount", zero()),
                ("deletedOpenItemCount", zero()),
                ("completedWithoutActiveCount", zero()),
            ]),
        ),
    ])
}
fn js_trimmed(value: &JsValue) -> Option<JsValue> {
    let text = String::from_utf16_lossy(value.as_string()?.units());
    let trimmed = crate::metadata::js_trim(&text);
    (!trimmed.is_empty()).then(|| JsValue::String(trimmed.into()))
}
fn todo_key(todo: &JsValue) -> &JsValue {
    match field(todo, "id") {
        JsValue::Undefined | JsValue::Null => field(todo, "content"),
        id => id,
    }
}
fn todo_active(todo: &JsValue) -> bool {
    is(field(todo, "status"), "pending") || is(field(todo, "status"), "in_progress")
}
fn count(todos: &[JsValue], test: impl Fn(&JsValue) -> bool) -> JsValue {
    number(todos.iter().filter(|todo| test(todo)).count() as f64)
}
fn counter(diagnostics: &JsValue, key: &str) -> f64 {
    field(diagnostics, key).as_number().unwrap_or(0.0)
}

/// `createSnapshotForPlan` (:120-128).
fn plan_changed(plan: &JsValue) -> JsObject {
    let mut state = plan_todo_initial();
    let approved = js_trimmed(plan);
    state.insert(
        "requiresInitialization".into(),
        JsValue::Bool(approved.is_some()),
    );
    if let Some(approved) = approved {
        state.insert("approvedPlan".into(), approved);
    }
    state
}

/// `applyTodoWrite` (:130-201).
fn todo_written(state: &JsObject, entry: &JsValue) -> JsObject {
    let get = |key: &str| state.get(key).unwrap_or(&UNDEFINED);
    let previous = array_of(get("todos"));
    let next = array_of(field(entry, "todos")).to_vec();
    let known = |todos: &[JsValue], todo: &JsValue| {
        todos
            .iter()
            .rev()
            .find(|candidate| todo_key(candidate) == todo_key(todo))
            .cloned()
    };
    let removed: Vec<&JsValue> = previous
        .iter()
        .filter(|todo| known(&next, todo).is_none())
        .collect();
    let added = next
        .iter()
        .filter(|todo| known(previous, todo).is_none())
        .count();
    let changed = next
        .iter()
        .filter(|todo| {
            known(previous, todo).is_some_and(|before| {
                ["content", "status", "priority"]
                    .iter()
                    .any(|key| field(&before, key) != field(todo, key))
            })
        })
        .count();
    let deleted_open = removed.iter().filter(|todo| todo_active(todo)).count() as f64;
    let previous_active = array_of(get("activeTodos")).len();
    let preserved = next
        .iter()
        .filter(|todo| known(previous, todo).is_some())
        .count();
    let large_rewrite =
        !previous.is_empty() && !next.is_empty() && preserved < previous.len().div_ceil(2);
    let active: Vec<JsValue> = next
        .iter()
        .filter(|todo| todo_active(todo))
        .cloned()
        .collect();
    let all_completed = !next.is_empty()
        && active.is_empty()
        && next.iter().all(|todo| {
            is(field(todo, "status"), "completed") || is(field(todo, "status"), "cancelled")
        });
    let reason = js_trimmed(field(entry, "reason")).unwrap_or(JsValue::Undefined);
    let history = array_of(get("todoHistory"));
    let old = get("todoDiagnostics");
    let diagnostics = object([
        ("writeCount", number(history.len() as f64 + 1.0)),
        ("todoCount", number(next.len() as f64)),
        ("activeCount", number(active.len() as f64)),
        (
            "completedCount",
            count(&next, |todo| is(field(todo, "status"), "completed")),
        ),
        (
            "cancelledCount",
            count(&next, |todo| is(field(todo, "status"), "cancelled")),
        ),
        (
            "largeRewriteCount",
            number(counter(old, "largeRewriteCount") + f64::from(u8::from(large_rewrite))),
        ),
        (
            "deletedOpenItemCount",
            number(counter(old, "deletedOpenItemCount") + deleted_open),
        ),
        (
            "completedWithoutActiveCount",
            number(
                counter(old, "completedWithoutActiveCount")
                    + f64::from(u8::from(all_completed && previous_active > 0)),
            ),
        ),
        (
            "lastWrite",
            object([
                ("mode", field(entry, "mode").clone()),
                ("merge", field(entry, "merge").clone()),
                ("reason", reason.clone()),
                ("addedCount", number(added as f64)),
                ("removedCount", number(removed.len() as f64)),
                ("changedCount", number(changed as f64)),
                ("deletedOpenItemCount", number(deleted_open)),
                ("largeRewrite", JsValue::Bool(large_rewrite)),
                ("allCompleted", JsValue::Bool(all_completed)),
            ]),
        ),
    ]);
    let mut all_history = history.to_vec();
    all_history.push(object([
        ("createdAt", field(entry, "createdAt").clone()),
        ("mode", field(entry, "mode").clone()),
        ("merge", field(entry, "merge").clone()),
        ("reason", reason),
        ("markdown", field(entry, "markdown").clone()),
        ("todos", JsValue::Array(next.clone())),
        ("diagnostics", diagnostics.clone()),
    ]));
    let approved = get("approvedPlan");
    object_map([
        (
            "approvedPlan",
            if truthy(approved) {
                approved.clone()
            } else {
                JsValue::Undefined
            },
        ),
        ("requiresInitialization", JsValue::Bool(false)),
        ("toolCallsSinceLastTodoWrite", number(0.0)),
        ("lastMarkdown", field(entry, "markdown").clone()),
        ("todos", JsValue::Array(next)),
        ("activeTodos", JsValue::Array(active)),
        ("todoHistory", JsValue::Array(all_history)),
        ("todoDiagnostics", diagnostics),
    ])
}

/// `applyToolProgress` + `withDiagnostics` (:203-229).
fn todo_progressed(state: &mut JsObject, tool_name: &JsValue) {
    let get = |key: &str| state.get(key).cloned().unwrap_or(JsValue::Undefined);
    if !truthy(&get("approvedPlan"))
        || truthy(&get("requiresInitialization"))
        || is(tool_name, "todo_write")
    {
        return;
    }
    let calls = get("toolCallsSinceLastTodoWrite")
        .as_number()
        .unwrap_or(0.0)
        + 1.0;
    let todos = get("todos");
    let todos = array_of(&todos);
    let mut diagnostics = get("todoDiagnostics")
        .as_object()
        .cloned()
        .unwrap_or_default();
    for (key, value) in [
        (
            "writeCount",
            number(array_of(&get("todoHistory")).len() as f64),
        ),
        ("todoCount", number(todos.len() as f64)),
        (
            "activeCount",
            number(array_of(&get("activeTodos")).len() as f64),
        ),
        (
            "completedCount",
            count(todos, |todo| is(field(todo, "status"), "completed")),
        ),
        (
            "cancelledCount",
            count(todos, |todo| is(field(todo, "status"), "cancelled")),
        ),
    ] {
        diagnostics.insert(key.into(), value);
    }
    state.insert("toolCallsSinceLastTodoWrite".into(), number(calls));
    state.insert("todoDiagnostics".into(), JsValue::Object(diagnostics));
}

fn plan_todo(entries: &[JsValue]) -> JsValue {
    let mut state = plan_todo_initial();
    for entry in entries {
        let kind = field(entry, "type");
        if is(kind, "plan_todo_plan_changed") {
            state = plan_changed(field(entry, "plan"));
        } else if is(kind, "plan_todo_written") {
            state = todo_written(&state, entry);
        } else if is(kind, "plan_todo_progressed") {
            todo_progressed(&mut state, field(entry, "toolName"));
        }
    }
    JsValue::Object(state)
}

// ── goal.state (GoalProjection.ts) ──

fn safe_integer_at_least_one(value: &JsValue) -> bool {
    value
        .as_number()
        .is_some_and(|n| n.fract() == 0.0 && (1.0..=9_007_199_254_740_991.0).contains(&n))
}
fn non_blank(value: &JsValue) -> bool {
    js_trimmed(value).is_some()
}
/// `normalizeGoal` (:47-71); `None` is the baseline's `TypeError`.
fn normalize_goal(goal: &JsValue) -> Option<JsValue> {
    goal.as_object()?;
    let blocked_reason = field(goal, "blockedReason");
    let max_rounds = field(goal, "maxGoalRounds");
    let phase = field(goal, "phase");
    if !non_blank(field(goal, "id"))
        || !non_blank(field(goal, "objective"))
        || !safe_integer_at_least_one(field(goal, "revision"))
        || !["active", "paused", "blocked", "complete"]
            .iter()
            .any(|name| is(phase, name))
        || !(blocked_reason.is_undefined() || blocked_reason.as_string().is_some())
        || !(max_rounds.is_undefined() || safe_integer_at_least_one(max_rounds))
    {
        return None;
    }
    let reason = non_blank(blocked_reason);
    if is(phase, "blocked") && !reason {
        return None;
    }
    Some(object([
        ("id", field(goal, "id").clone()),
        ("revision", field(goal, "revision").clone()),
        ("objective", field(goal, "objective").clone()),
        ("phase", phase.clone()),
        (
            "blockedReason",
            if reason {
                blocked_reason.clone()
            } else {
                JsValue::Undefined
            },
        ),
        ("maxGoalRounds", max_rounds.clone()),
    ]))
}
/// `None` when a `goal_changed` entry fails the reducer's checks (:17-25):
/// the baseline driver rebuilds the cell, fails again, and drops it
/// (SessionProjectionDriver.ts:165-173), so the checkpoint has no row.
fn goal(entries: &[JsValue]) -> Option<JsValue> {
    let mut revision = 0.0;
    let mut current = JsValue::Null;
    for entry in entries {
        if !is(field(entry, "type"), "goal_changed") {
            continue;
        }
        let next = field(entry, "revision");
        if next.as_number() != Some(revision + 1.0) {
            return None;
        }
        current = if truthy(field(entry, "goal")) {
            let goal = normalize_goal(field(entry, "goal"))?;
            if field(&goal, "revision") != next {
                return None;
            }
            goal
        } else {
            JsValue::Null
        };
        revision += 1.0;
    }
    Some(object([("goal", current), ("revision", number(revision))]))
}

pub(crate) const TURN_SUMMARY: &str = "agent-transcript.turn-summary";
pub(crate) const METADATA: &str = "agent-transcript.metadata";

/// The checkpoint rows as `(name, version, state)` in the baseline's
/// registration order (BuiltinSessionProjections.ts:8-16). `turn_summary` and
/// `metadata` are the caller's live states for those two rows.
#[cfg(test)]
pub(crate) fn checkpoint_rows(
    entries: &[JsValue],
    turn_summary: JsValue,
    metadata: JsValue,
) -> Vec<(&'static str, f64, JsValue)> {
    checkpoint_rows_restored(entries, turn_summary, metadata, None)
}

/// [`checkpoint_rows`] for a session that restored its token-usage row from
/// a checkpoint (see [`restored_token_usage`]).
pub(crate) fn checkpoint_rows_restored(
    entries: &[JsValue],
    turn_summary: JsValue,
    metadata: JsValue,
    token_usage: Option<&RestoredTokenUsage>,
) -> Vec<(&'static str, f64, JsValue)> {
    let mut web = WebHistory::project(entries);
    if let Some(restored) = token_usage {
        web.token_usage = continue_token_usage(restored, entries);
    }
    let mut rows = vec![
        ("agent-transcript.conversation", 3.0, conversation(entries)),
        (TURN_SUMMARY, 1.0, turn_summary),
        (METADATA, 2.0, metadata),
        (
            "agent-transcript.compact-boundary",
            1.0,
            compact_boundary_state(entries),
        ),
        (
            "agent-transcript.subagent-references",
            1.0,
            subagent_references(entries),
        ),
        ("file-history.snapshots", 1.0, file_history(entries)),
        (
            "web-history.file-artifacts",
            1.0,
            web.file_artifacts_state(),
        ),
        (
            "web-history.agent-statuses",
            1.0,
            JsValue::Array(web.agent_statuses.clone()),
        ),
        ("web-history.turn-errors", 1.0, web.turn_errors_state()),
        (
            "web-history.token-usage",
            1.0,
            JsValue::Object(web.token_usage.clone()),
        ),
        (
            "web-history.incomplete-turns",
            1.0,
            web.incomplete_turns_state(),
        ),
        ("plan-todo.state", 1.0, plan_todo(entries)),
    ];
    if let Some(goal) = goal(entries) {
        rows.push(("goal.state", 1.0, goal));
    }
    rows
}

/// tokenUsageProjection's reducer (WebHistoryProjections.ts:197-212): an
/// assignment to a key the state already has keeps its place, like the
/// reducer's object spread.
fn reduce_token_usage(state: &mut JsObject, entry: &JsValue, index: usize) {
    if let Some(budget) = context_budget(entry, index) {
        state.insert("latestContextBudget".into(), budget);
    } else if let Some(budget) = compact_budget(entry, index) {
        state.insert("latestCompactBudget".into(), budget);
    } else if is(field(entry, "type"), "turn_result") {
        let usage = field(field(entry, "result"), "usage");
        if [
            "inputTokens",
            "outputTokens",
            "cacheReadTokens",
            "cacheWriteTokens",
            "totalTokens",
        ]
        .iter()
        .any(|key| positive_number(field(usage, key)).is_some())
        {
            state.insert("latestTurnUsage".into(), usage.clone());
        }
    }
}

/// A token-usage checkpoint row as the session restored it:
/// `decodeWebTokenUsage` (WebHistoryProjections.ts:412-447) rebuilds the
/// state in a fixed shape — `latestContextBudget`, `latestCompactBudget`,
/// `latestTurnUsage`, the compact budget as `index, preTokens?, postTokens,
/// messagesSummarized?` — and the reducer then continues from the entries
/// after the row's `asOfSequence`. `None` when the row is missing, of another
/// version, ahead of the log, or does not decode (the projection then
/// replays the whole log, SessionProjectionDriver.ts:192-211).
#[derive(Clone, Debug)]
pub(crate) struct RestoredTokenUsage {
    pub(crate) as_of_sequence: f64,
    pub(crate) state: JsObject,
}

pub(crate) fn restored_token_usage(
    rows: &JsValue,
    last_sequence: f64,
) -> Option<RestoredTokenUsage> {
    let row = field(rows, "web-history.token-usage");
    if field(row, "version").as_number() != Some(1.0) {
        return None;
    }
    let as_of_sequence = field(row, "asOfSequence").as_number()?;
    if as_of_sequence > last_sequence {
        return None;
    }
    let record = field(row, "state").as_object()?;
    let integer = |value: &JsValue| {
        value
            .as_number()
            .filter(|n| n.is_finite() && n.fract() == 0.0 && *n >= 0.0)
            .map(JsValue::Number)
    };
    let optional = |value: Option<&JsValue>| -> Result<Option<JsValue>, ()> {
        match value {
            None | Some(JsValue::Undefined) => Ok(None),
            Some(JsValue::Number(n)) if n.is_finite() && *n >= 0.0 => Ok(Some(JsValue::Number(*n))),
            Some(_) => Err(()),
        }
    };
    let mut state = JsObject::new();
    if let Some(context) = record
        .get("latestContextBudget")
        .filter(|v| !v.is_undefined())
    {
        let context = context.as_object()?;
        let usage = context.get("usage").and_then(JsValue::as_object)?;
        let mut budget = JsObject::new();
        budget.insert("index".into(), integer(context.get("index")?)?);
        budget.insert("usage".into(), JsValue::Object(usage.clone()));
        state.insert("latestContextBudget".into(), JsValue::Object(budget));
    }
    if let Some(compact) = record
        .get("latestCompactBudget")
        .filter(|v| !v.is_undefined())
    {
        let compact = compact.as_object()?;
        let pre = optional(compact.get("preTokens")).ok()?;
        let summarized = optional(compact.get("messagesSummarized")).ok()?;
        let post = optional(compact.get("postTokens")).ok()??;
        let mut budget = JsObject::new();
        budget.insert("index".into(), integer(compact.get("index")?)?);
        if let Some(pre) = pre {
            budget.insert("preTokens".into(), pre);
        }
        budget.insert("postTokens".into(), post);
        if let Some(summarized) = summarized {
            budget.insert("messagesSummarized".into(), summarized);
        }
        state.insert("latestCompactBudget".into(), JsValue::Object(budget));
    }
    if let Some(usage) = record.get("latestTurnUsage").filter(|v| !v.is_undefined()) {
        // `checkpointCanonicalUsage`: validated, then cloned as is.
        let usage = usage.as_object()?;
        for key in [
            "inputTokens",
            "outputTokens",
            "cacheReadTokens",
            "cacheWriteTokens",
            "totalTokens",
            "nativeCost",
        ] {
            optional(usage.get(key)).ok()?;
        }
        state.insert("latestTurnUsage".into(), JsValue::Object(usage.clone()));
    }
    Some(RestoredTokenUsage {
        as_of_sequence,
        state,
    })
}

/// The token-usage state continued from a restored row.
fn continue_token_usage(restored: &RestoredTokenUsage, entries: &[JsValue]) -> JsObject {
    let mut state = restored.state.clone();
    for (index, entry) in entries.iter().enumerate() {
        if field(entry, "sequence")
            .as_number()
            .is_some_and(|sequence| sequence > restored.as_of_sequence)
        {
            reduce_token_usage(&mut state, entry, index);
        }
    }
    state
}

/// `projectSubagentReferences` as the web reader consumes it: the started
/// and completed entries in log order.
pub(crate) fn subagent_entries(entries: &[JsValue]) -> (Vec<JsValue>, Vec<JsValue>) {
    (
        entries_of_type(entries, "subagent_started"),
        entries_of_type(entries, "subagent_completed"),
    )
}

/// A JS string value built from UTF-16 units, for template-literal results.
pub(crate) fn string_units(units: Vec<u16>) -> JsValue {
    JsValue::String(JsString::from_utf16(units))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilotdeck_protocol::{parse_json, stringify_json};

    /// The baseline driver's checkpoint for hand-written transcripts, produced
    /// by `tests/fixtures/session-projections/generate.mjs`.
    const ORACLE: &str = include_str!("../../tests/fixtures/session-projections/oracle.json");

    fn text(value: &JsValue) -> String {
        stringify_json(value).unwrap().unwrap()
    }

    /// Every row except the two the session factory owns must serialize to
    /// the baseline's bytes, in the baseline's row order.
    fn assert_case(name: &str) {
        let oracle = parse_json(ORACLE).unwrap();
        let case = field(field(&oracle, "cases"), name);
        let expected = field(case, "checkpoint").as_object().unwrap();
        let entries = array_of(field(case, "entries"));
        let row = |name: &str| field(expected.get(name).unwrap(), "state").clone();
        let rows = checkpoint_rows(entries, row(TURN_SUMMARY), row(METADATA));
        let names: Vec<&str> = rows.iter().map(|(name, _, _)| *name).collect();
        let expected_names: Vec<String> = expected
            .entries()
            .iter()
            .map(|(name, _)| String::from_utf16_lossy(name.units()))
            .collect();
        assert_eq!(names, expected_names, "{name}: row set and order");
        for (row_name, version, state) in &rows {
            let want = expected.get(row_name).unwrap();
            assert_eq!(
                Some(*version),
                field(want, "version").as_number(),
                "{name}: {row_name} version"
            );
            assert_eq!(
                text(state),
                text(field(want, "state")),
                "{name}: {row_name} state"
            );
        }
    }

    #[test]
    fn checkpoint_rows_match_the_baseline_driver() {
        assert_case("valid");
    }

    #[test]
    fn a_goal_entry_the_reducer_rejects_drops_the_goal_row() {
        assert_case("brokenGoal");
    }
}
