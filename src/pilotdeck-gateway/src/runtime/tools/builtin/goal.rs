//! `get_goal` / `create_goal` / `update_goal` (baseline
//! `tool/builtin/goal.ts`): the tool surface of the session's
//! [`GoalRuntime`], reached through `context.goal`.
use super::{field, object, schema, string};
use crate::{
    host::{HostError, modules::capability::ToolDefinition},
    runtime::{
        goal::{GoalError, GoalRuntime, GoalUpdate},
        tools::{ExecutableTool, ToolError, ToolFuture, ToolOutput, ToolRunContext},
    },
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Get,
    Create,
    Update,
}

pub struct GoalTool {
    kind: Kind,
    fields: JsObject,
}

/// `createGoalTools()` in the baseline's order.
pub fn goal_tools() -> Vec<Arc<dyn ExecutableTool>> {
    [Kind::Get, Kind::Create, Kind::Update]
        .into_iter()
        .map(|kind| {
            Arc::new(GoalTool {
                kind,
                fields: definition(kind),
            }) as Arc<dyn ExecutableTool>
        })
        .collect()
}

fn definition(kind: Kind) -> JsObject {
    let (name, description, input_schema) = match kind {
        Kind::Get => (
            "get_goal",
            "Read the current durable goal for this session.",
            r#"{"type":"object","properties":{},"additionalProperties":false}"#,
        ),
        Kind::Create => (
            "create_goal",
            "Create the session's durable goal. Only one goal can exist at a time.",
            r#"{"type":"object","properties":{"objective":{"type":"string","description":"The outcome this session is pursuing."},"max_goal_rounds":{"type":"number","description":"Optional maximum number of continuation rounds."},"id":{"type":"string","description":"Optional stable goal identifier."}},"required":["objective"],"additionalProperties":false}"#,
        ),
        Kind::Update => (
            "update_goal",
            "Edit or transition the session's durable goal using an optional expected revision for CAS safety.",
            r#"{"type":"object","properties":{"action":{"type":"string","enum":["edit","pause","resume","complete","block","clear"]},"expected_revision":{"type":"number"},"objective":{"type":"string"},"blocked_reason":{"type":"string"},"max_goal_rounds":{"type":"number"}},"additionalProperties":false}"#,
        ),
    };
    object([
        ("name", string(name)),
        ("description", string(description)),
        ("kind", string("session")),
        ("inputSchema", JsValue::Object(schema(input_schema))),
    ])
}

fn defined<'a>(input: &'a JsValue, key: &str) -> Option<&'a JsValue> {
    Some(field(input, key)).filter(|value| !value.is_undefined())
}

impl ToolDefinition for GoalTool {
    fn fields(&self) -> &JsObject {
        &self.fields
    }
    fn is_read_only(&self, input: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(match self.kind {
            Kind::Get => true,
            Kind::Create => false,
            Kind::Update => {
                field(input, "action")
                    .as_string()
                    .is_some_and(|action| action.equals_str("edit"))
                    && ["objective", "blocked_reason", "max_goal_rounds"]
                        .iter()
                        .all(|key| defined(input, key).is_none())
            }
        }))
    }
    fn is_concurrency_safe(&self, _input: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(self.kind == Kind::Get))
    }
}

/// `requireGoal(context)`.
fn require_goal(context: &ToolRunContext) -> Result<Arc<GoalRuntime>, ToolError> {
    context
        .host
        .goal
        .clone()
        .and_then(|goal| goal.downcast::<GoalRuntime>().ok())
        .ok_or_else(|| {
            ToolError::new(
                "unsupported_tool",
                "Goal tools require a session-scoped goal provider.",
            )
        })
}

/// `mapGoalError`: a revision conflict keeps both revisions as details; any
/// other failure is the thrown error's message.
fn goal_error(error: GoalError) -> ToolError {
    let mut failure = ToolError::failed(error.message.as_str());
    if let Some((expected, actual)) = error.conflict {
        failure.details = Some(object([
            (
                "expectedRevision",
                expected.map_or(JsValue::Undefined, JsValue::Number),
            ),
            ("actualRevision", JsValue::Number(actual)),
        ]));
    }
    failure
}

/// `output(goal, label)`.
fn output(goal: Option<JsObject>, operation: &str) -> ToolOutput {
    let revision = goal
        .as_ref()
        .and_then(|goal| goal.get("revision"))
        .filter(|revision| !matches!(revision, JsValue::Undefined | JsValue::Null))
        .cloned()
        .unwrap_or(JsValue::Null);
    let goal = goal.map_or(JsValue::Null, JsValue::Object);
    ToolOutput {
        content: vec![JsValue::Object(object([
            ("type", string("json")),
            ("value", goal.clone()),
        ]))],
        data: Some(goal),
        metadata: Some(object([
            ("operation", string(operation)),
            ("revision", revision),
        ])),
        supplemental_messages: None,
    }
}

fn turn_id(context: &ToolRunContext) -> JsString {
    context
        .fields
        .get("turnId")
        .and_then(JsValue::as_string)
        .cloned()
        .unwrap_or_default()
}

impl ExecutableTool for GoalTool {
    fn execute(&self, input: JsValue, context: ToolRunContext) -> ToolFuture<ToolOutput> {
        let kind = self.kind;
        Box::pin(async move {
            let goal = require_goal(&context)?;
            let text = |key: &str| defined(&input, key).and_then(JsValue::as_string).cloned();
            match kind {
                Kind::Get => Ok(output(goal.snapshot().map_err(goal_error)?.goal, "get")),
                Kind::Create => goal
                    .create(
                        &text("objective").unwrap_or_default(),
                        defined(&input, "max_goal_rounds"),
                        text("id").as_ref(),
                        &turn_id(&context),
                    )
                    .await
                    .map(|goal| output(Some(goal), "create"))
                    .map_err(goal_error),
                Kind::Update => goal
                    .update(
                        &GoalUpdate {
                            action: text("action"),
                            expected_revision: defined(&input, "expected_revision").cloned(),
                            objective: text("objective"),
                            blocked_reason: text("blocked_reason"),
                            max_goal_rounds: defined(&input, "max_goal_rounds").cloned(),
                        },
                        &turn_id(&context),
                    )
                    .await
                    .map(|goal| output(goal, "update"))
                    .map_err(goal_error),
            }
        })
    }
}
