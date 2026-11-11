//! Session goal state (baseline `goal/`): the `goal.state` reducer over the
//! session's durable entries and the runtime that appends `goal_changed`.
//! Like the baseline's projection-backed runtime, the state is folded from
//! the live entry list on every read; mutations are serialized per session.
use crate::{
    host::modules::{capability::RuntimeContext, tool_context::SessionService},
    js_value::number_string,
    runtime::{memory::js_trim_units, session::factory::SessionProjection},
    storage::recorder::Recorder,
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// A failed goal operation. `conflict` carries `GoalConflictError`'s
/// `(expectedRevision, actualRevision)`.
#[derive(Debug, Clone, PartialEq)]
pub struct GoalError {
    pub message: String,
    pub conflict: Option<(Option<f64>, f64)>,
}
impl GoalError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            conflict: None,
        }
    }
}

/// `GoalStateSnapshot`.
#[derive(Debug, Clone, PartialEq)]
pub struct GoalState {
    pub goal: Option<JsObject>,
    pub revision: f64,
}

fn trim(value: &JsString) -> JsString {
    JsString::from_utf16(js_trim_units(value.units()).to_vec())
}
fn safe_positive_integer(value: &JsValue) -> bool {
    value
        .as_number()
        .is_some_and(|n| n.fract() == 0.0 && (1.0..=9007199254740991.0).contains(&n))
}
fn defined(value: Option<&JsValue>) -> Option<&JsValue> {
    value.filter(|value| !value.is_undefined())
}

/// `normalizeGoal`: the snapshot shape, in the baseline's key order.
fn normalize(value: &JsValue) -> Result<JsObject, GoalError> {
    let invalid = || GoalError::new("goal snapshot is invalid.");
    let nonblank = |field: &str| {
        value
            .get(field)
            .and_then(JsValue::as_string)
            .filter(|text| !trim(text).is_empty())
    };
    let (Some(id), Some(objective)) = (nonblank("id"), nonblank("objective")) else {
        return Err(invalid());
    };
    let revision = value.get("revision").ok_or_else(invalid)?;
    let phase = value.get("phase").and_then(JsValue::as_string);
    let blocked_reason = defined(value.get("blockedReason"));
    let max_rounds = defined(value.get("maxGoalRounds"));
    if !safe_positive_integer(revision)
        || !["active", "paused", "blocked", "complete"]
            .iter()
            .any(|known| phase.is_some_and(|phase| phase.equals_str(known)))
        || blocked_reason.is_some_and(|reason| reason.as_string().is_none())
        || max_rounds.is_some_and(|rounds| !safe_positive_integer(rounds))
    {
        return Err(invalid());
    }
    let mut goal = JsObject::new();
    goal.insert("id".into(), JsValue::String(id.clone()));
    goal.insert("revision".into(), revision.clone());
    goal.insert("objective".into(), JsValue::String(objective.clone()));
    goal.insert("phase".into(), JsValue::String(phase.unwrap().clone()));
    let reason = blocked_reason
        .and_then(JsValue::as_string)
        .filter(|reason| !trim(reason).is_empty());
    if let Some(reason) = reason {
        goal.insert("blockedReason".into(), JsValue::String(reason.clone()));
    }
    if let Some(rounds) = max_rounds {
        goal.insert("maxGoalRounds".into(), rounds.clone());
    }
    if phase.unwrap().equals_str("blocked") && reason.is_none() {
        return Err(GoalError::new("Blocked goals require a reason."));
    }
    Ok(goal)
}

impl GoalState {
    pub fn new() -> Self {
        Self {
            goal: None,
            revision: 0.0,
        }
    }
    /// The projection's `reduce`.
    pub fn reduce(self, entry: &JsValue) -> Result<Self, GoalError> {
        if !entry
            .get("type")
            .and_then(JsValue::as_string)
            .is_some_and(|kind| kind.equals_str("goal_changed"))
        {
            return Ok(self);
        }
        let revision = entry.get("revision").and_then(JsValue::as_number);
        if revision != Some(self.revision + 1.0) {
            return Err(GoalError::new(format!(
                "Goal revision must increase by one: expected {}, received {}.",
                number_string(self.revision + 1.0),
                entry
                    .get("revision")
                    .and_then(|value| crate::js_value::display(value).ok())
                    .unwrap_or_else(|| "undefined".into()),
            )));
        }
        let revision = revision.unwrap();
        let goal = match entry.get("goal") {
            Some(goal @ JsValue::Object(_)) => Some(normalize(goal)?),
            _ => None,
        };
        if goal
            .as_ref()
            .is_some_and(|goal| goal.get("revision").and_then(JsValue::as_number) != Some(revision))
        {
            return Err(GoalError::new(
                "Goal snapshot revision must match the event revision.",
            ));
        }
        Ok(Self { goal, revision })
    }
}
impl Default for GoalState {
    fn default() -> Self {
        Self::new()
    }
}

/// `GoalUpdate & { turnId }`; `None` is an absent (`undefined`) field.
#[derive(Debug, Clone, Default)]
pub struct GoalUpdate {
    pub action: Option<JsString>,
    pub expected_revision: Option<JsValue>,
    pub objective: Option<JsString>,
    pub blocked_reason: Option<JsString>,
    pub max_goal_rounds: Option<JsValue>,
}

/// What one session's goal runtime keeps for the session's whole life, across
/// the per-turn [`GoalRuntime`] handles.
#[derive(Default)]
pub struct GoalSessionState {
    /// `goalSequence` (`ProjectSessionRuntimeBundle.ts:508`).
    sequence: AtomicU64,
    /// `mutationTail`: create/update run one at a time.
    mutations: tokio::sync::Mutex<()>,
}

/// `NativeGoalRuntime`: one session's goal provider.
pub struct GoalRuntime {
    session: JsString,
    recorder: Arc<Recorder>,
    projection: Arc<SessionProjection>,
    /// `uuid: () => \`${now().getTime()}-${++goalSequence}\``
    /// (`ProjectSessionRuntimeBundle.ts:636`).
    now_ms: Arc<dyn Fn() -> f64 + Send + Sync>,
    shared: Arc<GoalSessionState>,
}
impl GoalRuntime {
    pub fn new(
        session: JsString,
        recorder: Arc<Recorder>,
        projection: Arc<SessionProjection>,
        now_ms: Arc<dyn Fn() -> f64 + Send + Sync>,
        shared: Arc<GoalSessionState>,
    ) -> Arc<Self> {
        Arc::new(Self {
            session,
            recorder,
            projection,
            now_ms,
            shared,
        })
    }

    pub fn snapshot(&self) -> Result<GoalState, GoalError> {
        let mut state = Ok(GoalState::new());
        self.projection.for_each_entry(|entry| {
            state = std::mem::replace(&mut state, Ok(GoalState::new()))
                .and_then(|state| state.reduce(entry));
        });
        state
    }

    async fn append(&self, goal: Option<&JsObject>, turn: &JsString) -> Result<(), GoalError> {
        let revision = self.snapshot()?.revision + 1.0;
        let mut draft = JsObject::new();
        draft.insert("type".into(), "goal_changed".into());
        draft.insert(
            "goal".into(),
            goal.map_or(JsValue::Null, |goal| JsValue::Object(goal.clone())),
        );
        draft.insert("revision".into(), JsValue::Number(revision));
        let recorder = self.recorder.clone();
        let (session, turn) = (self.session.clone(), turn.clone());
        tokio::task::spawn_blocking(move || {
            recorder
                .writer()
                .record_session_event(&session, &turn, draft)
        })
        .await
        .map_err(|error| GoalError::new(error.to_string()))?
        .map_err(|error| GoalError::new(error.to_string()))
    }

    /// `create(objective, { maxGoalRounds, id, turnId })`.
    pub async fn create(
        &self,
        objective: &JsString,
        max_goal_rounds: Option<&JsValue>,
        id: Option<&JsString>,
        turn: &JsString,
    ) -> Result<JsObject, GoalError> {
        let _serialized = self.shared.mutations.lock().await;
        let current = self.snapshot()?;
        if current.goal.is_some() {
            return Err(GoalError {
                message: "A goal already exists; update or clear it before creating another."
                    .into(),
                conflict: Some((None, current.revision)),
            });
        }
        let objective = trim(objective);
        if objective.is_empty() {
            return Err(GoalError::new("Goal objective must not be empty."));
        }
        if max_goal_rounds.is_some_and(|rounds| !safe_positive_integer(rounds)) {
            return Err(GoalError::new("maxGoalRounds must be a positive integer."));
        }
        let revision = current.revision + 1.0;
        let id = id.map(trim).filter(|id| !id.is_empty()).unwrap_or_else(|| {
            format!(
                "goal-{}-{}",
                number_string((self.now_ms)()),
                self.shared.sequence.fetch_add(1, Ordering::SeqCst) + 1
            )
            .into()
        });
        let mut goal = JsObject::new();
        goal.insert("id".into(), JsValue::String(id));
        goal.insert("revision".into(), JsValue::Number(revision));
        goal.insert("objective".into(), JsValue::String(objective));
        goal.insert("phase".into(), "active".into());
        if let Some(rounds) = max_goal_rounds {
            goal.insert("maxGoalRounds".into(), rounds.clone());
        }
        self.append(Some(&goal), turn).await?;
        Ok(goal)
    }

    /// `update(update)`; `Ok(None)` is the cleared (or already absent) goal.
    pub async fn update(
        &self,
        update: &GoalUpdate,
        turn: &JsString,
    ) -> Result<Option<JsObject>, GoalError> {
        let _serialized = self.shared.mutations.lock().await;
        let current = self.snapshot()?;
        if let Some(expected) = &update.expected_revision
            && expected.as_number() != Some(current.revision)
        {
            return Err(GoalError {
                message: format!(
                    "Goal revision conflict: expected {}, current is {}.",
                    crate::js_value::display(expected).unwrap_or_default(),
                    number_string(current.revision)
                ),
                conflict: Some((expected.as_number(), current.revision)),
            });
        }
        let action = |name: &str| update.action.as_ref().is_some_and(|a| a.equals_str(name));
        let Some(existing) = &current.goal else {
            if action("clear") {
                return Ok(None);
            }
            return Err(GoalError::new("No goal exists for this session."));
        };
        if action("clear") {
            self.append(None, turn).await?;
            return Ok(None);
        }
        let current_field = |field: &str| defined(existing.get(field)).cloned();
        let objective = update
            .objective
            .as_ref()
            .map(trim)
            .filter(|objective| !objective.is_empty())
            .map(JsValue::String)
            .or_else(|| current_field("objective"));
        let Some(objective) = objective else {
            return Err(GoalError::new("Goal objective must not be empty."));
        };
        let phase: JsValue = if action("pause") {
            "paused".into()
        } else if action("resume") {
            "active".into()
        } else if action("complete") {
            "complete".into()
        } else if action("block") {
            "blocked".into()
        } else {
            current_field("phase").unwrap_or(JsValue::Undefined)
        };
        let blocked = phase
            .as_string()
            .is_some_and(|phase| phase.equals_str("blocked"));
        let max_goal_rounds = update
            .max_goal_rounds
            .clone()
            .filter(|rounds| !matches!(rounds, JsValue::Null))
            .or_else(|| current_field("maxGoalRounds"));
        if max_goal_rounds
            .as_ref()
            .is_some_and(|rounds| !safe_positive_integer(rounds))
        {
            return Err(GoalError::new("maxGoalRounds must be a positive integer."));
        }
        // `{ ...current.goal, revision, objective, phase, blockedReason?,
        // maxGoalRounds? }`, then `delete goal.blockedReason` unless blocked:
        // existing keys keep their position, new ones are appended.
        let update_reason = update
            .blocked_reason
            .as_ref()
            .map(trim)
            .filter(|reason| !reason.is_empty());
        let mut goal = JsObject::new();
        for (name, value) in existing.entries() {
            if name.equals_str("blockedReason") && !blocked {
                continue;
            }
            goal.insert(name.clone(), value.clone());
        }
        goal.insert("revision".into(), JsValue::Number(current.revision + 1.0));
        goal.insert("objective".into(), objective);
        goal.insert("phase".into(), phase);
        if blocked && let Some(reason) = update_reason {
            goal.insert("blockedReason".into(), JsValue::String(reason));
        }
        if let Some(rounds) = max_goal_rounds {
            goal.insert("maxGoalRounds".into(), rounds);
        }
        self.append(Some(&goal), turn).await?;
        Ok(Some(goal))
    }
}

/// `goalManager.forSession`: the handle tools receive as `context.goal`.
pub struct GoalSessions(pub Arc<GoalRuntime>);
impl SessionService for GoalSessions {
    fn for_session(&self, session_id: &JsString) -> Option<RuntimeContext> {
        (session_id == &self.0.session).then(|| self.0.clone() as RuntimeContext)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilotdeck_protocol::{parse_json, stringify_json};

    fn reduce(state: GoalState, json: &str) -> Result<GoalState, GoalError> {
        state.reduce(&parse_json(json).unwrap())
    }

    #[test]
    fn reducer_normalizes_the_snapshot_key_order() {
        let state = reduce(
            GoalState::new(),
            r#"{"type":"goal_changed","revision":1,"goal":{"id":"g","revision":1,"objective":"o","phase":"blocked","maxGoalRounds":5,"blockedReason":"why"}}"#,
        )
        .unwrap();
        assert_eq!(
            stringify_json(&JsValue::Object(state.goal.clone().unwrap()))
                .unwrap()
                .unwrap(),
            r#"{"id":"g","revision":1,"objective":"o","phase":"blocked","blockedReason":"why","maxGoalRounds":5}"#
        );
        let cleared = reduce(state, r#"{"type":"goal_changed","revision":2,"goal":null}"#).unwrap();
        assert_eq!(
            cleared,
            GoalState {
                goal: None,
                revision: 2.0
            }
        );
    }

    #[test]
    fn reducer_rejects_a_revision_gap_and_ignores_other_entries() {
        let state = reduce(GoalState::new(), r#"{"type":"turn_started"}"#).unwrap();
        assert_eq!(state, GoalState::new());
        let error =
            reduce(state, r#"{"type":"goal_changed","revision":3,"goal":null}"#).unwrap_err();
        assert_eq!(
            error.message,
            "Goal revision must increase by one: expected 1, received 3."
        );
    }
}
