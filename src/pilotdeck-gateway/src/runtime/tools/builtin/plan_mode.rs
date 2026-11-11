//! `enter_plan_mode` / `exit_plan_mode` (baseline `tool/builtin/planMode.ts`).
//! Entering asks the loop for plan mode through `data.requestedMode`; exiting
//! submits a markdown plan from the project's plan directory for approval over
//! the elicitation channel and, once approved, marks the plan approved (which
//! arms the todo gate) and asks for the default mode back.
use super::{field, object, schema, string, text};
use crate::{
    host::{HostError, modules::capability::ToolDefinition},
    runtime::{
        plan_todo::PlanTodoRuntime,
        tools::{
            ElicitationRequest, ExecutableTool, ToolElicitation, ToolError, ToolFuture, ToolOutput,
            ToolRunContext,
        },
    },
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::sync::{Arc, LazyLock};

const EXIT_PLAN_MODE_QUESTION: &str = "What should happen next?";
const EXIT_PLAN_MODE_CONTINUE: &str = "continue_planning";
const EXIT_PLAN_MODE_EXECUTE: &str = "execute_plan";

const ENTER_PLAN_MODE_DESCRIPTION: &str = concat!(
    "Enter plan mode for complex tasks requiring exploration and design. ",
    "Switches to a read-only phase where you explore the codebase, understand patterns, ",
    "and write a structured plan to a plan file before making any changes. ",
    "Prefer using this tool for non-trivial implementation tasks, especially when: ",
    "multiple valid approaches exist, the task touches many files, ",
    "or requirements need exploration to fully understand. ",
    "Do NOT call this tool if you are already in plan mode.",
);

const EXIT_PLAN_MODE_DESCRIPTION: &str = concat!(
    "Signal that your plan is complete and ready for user review. ",
    "Pass the plan_file_path for the markdown plan you want to submit from `.pilotdeck/plans`. ",
    "Do NOT use ask_user_question to ask about plan approval — that is exactly what this tool does.",
);

const PLAN_PATH_MESSAGE: &str = "plan_file_path must point to a markdown file under the current project's .pilotdeck/plans directory.";

fn plan_directory_section(path: Option<&JsString>) -> String {
    path.map(|path| {
        format!(
            "## Plan Directory\nCreate your plan as a markdown file under: {}\nYou may name the file yourself, but it must live under this directory.\n",
            text(path)
        )
    })
    .unwrap_or_default()
}

/// `buildEnterPlanModeResult`.
fn enter_result(path: Option<&JsString>) -> String {
    let mut lines: Vec<String> = vec![
        "Plan mode activated. You are now in a read-only exploration and planning phase.".into(),
        String::new(),
        plan_directory_section(path),
        "## What To Do".into(),
        "1. Explore the codebase using read_file, grep, glob to understand existing patterns and structure".into(),
        "2. Identify the key files, functions, and data flows relevant to the task".into(),
        "3. Design your implementation approach — consider trade-offs between alternatives".into(),
    ];
    if path.is_some() {
        lines.push(
            "4. Create and refine your own markdown plan file under the plan directory above"
                .into(),
        );
        lines.push("5. When your plan is ready, call exit_plan_mode with the plan_file_path you want to submit".into());
    } else {
        lines.push(
            "4. When your plan is ready, call exit_plan_mode to present it for user approval"
                .into(),
        );
    }
    lines.push(String::new());
    lines.push("## Rules".into());
    lines.push(format!(
        "- DO NOT call bash with write commands for any reason{}",
        if path.is_some() {
            "; use write_file/edit_file only for markdown plan files under the designated plan directory"
        } else {
            ""
        }
    ));
    lines.push(
        "- You MAY use ask_user_question to clarify requirements or choose between approaches"
            .into(),
    );
    lines.push("- Focus on understanding before proposing — read first, plan second".into());
    lines.join("\n")
}

/// `buildAlreadyInPlanModeResult`.
fn already_result(path: Option<&JsString>) -> String {
    let mut lines: Vec<String> = vec!["Plan mode is already active.".into(), String::new()];
    if let Some(path) = path {
        lines.push(format!("Your plan directory is: {}", text(path)));
        lines.push("Continue refining markdown plan files under that directory, then submit one explicitly with exit_plan_mode(plan_file_path).".into());
        lines.push(String::new());
    }
    lines.push(
        "Stay in read-only exploration/planning until you are ready to call exit_plan_mode.".into(),
    );
    lines.join("\n")
}

/// `buildApprovedPlanResult`.
fn approved_result(plan: &str, path: &str) -> String {
    [
        "User has approved your plan. You can now start coding.",
        "Do NOT output any confirmation text like \"Plan approved, starting implementation\" — the user already knows. Proceed directly with todo_write and implementation.",
        "Before using any non-read-only tool, you MUST call todo_write with a markdown checklist derived from the approved plan.",
        "After each completed implementation step, call todo_write again to refresh the checklist and mark completed items with `- [x]`.",
        "",
        &format!("Submitted plan file: {path}"),
        "You can refer back to it during implementation if needed.",
        "",
        "## Approved Plan",
        plan,
    ]
    .join("\n")
}

/// `buildContinuePlanningResult`.
fn continue_result(feedback: Option<&str>) -> String {
    let section = match feedback {
        Some(feedback) => format!("\n\nUser feedback:\n{feedback}"),
        None => "\n\nNo additional feedback was provided.".into(),
    };
    format!(
        "The user wants to continue planning before implementation. Stay in plan mode, refine the plan file, and call exit_plan_mode again when the updated plan is ready.{section}"
    )
}

fn plan_directory_path(context: &ToolRunContext) -> Option<&JsString> {
    context
        .host
        .plan_directory
        .as_ref()
        .map(|directory| &directory.path)
}

fn in_plan_mode(context: &ToolRunContext) -> bool {
    context.fields.get("permissionMode") == Some(&JsValue::from("plan"))
}

fn text_output(message: impl Into<JsString>, data: JsObject) -> ToolOutput {
    ToolOutput {
        content: vec![JsValue::Object(object([
            ("type", string("text")),
            ("text", JsValue::String(message.into())),
        ]))],
        supplemental_messages: None,
        data: Some(JsValue::Object(data)),
        metadata: None,
    }
}

pub struct EnterPlanModeTool {
    fields: JsObject,
}

impl EnterPlanModeTool {
    pub fn new() -> Self {
        Self {
            fields: object([
                ("name", string("enter_plan_mode")),
                ("aliases", JsValue::Array(vec![string("EnterPlanMode")])),
                ("description", string(ENTER_PLAN_MODE_DESCRIPTION)),
                ("kind", string("session")),
                (
                    "requiredRuntimeCapabilities",
                    JsValue::Array(vec![string("plan_workflow")]),
                ),
                (
                    "inputSchema",
                    JsValue::Object(schema(
                        r#"{"type":"object","additionalProperties":false,"properties":{}}"#,
                    )),
                ),
            ]),
        }
    }
}

impl Default for EnterPlanModeTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolDefinition for EnterPlanModeTool {
    fn fields(&self) -> &JsObject {
        &self.fields
    }
    fn is_read_only(&self, _input: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(true))
    }
    fn is_concurrency_safe(&self, _input: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(true))
    }
}

impl ExecutableTool for EnterPlanModeTool {
    fn execute(&self, _input: JsValue, context: ToolRunContext) -> ToolFuture<ToolOutput> {
        Box::pin(async move {
            let path = plan_directory_path(&context);
            if in_plan_mode(&context) {
                return Err(ToolError::failed(already_result(path)));
            }
            Ok(text_output(
                enter_result(path),
                object([("requestedMode", string("plan"))]),
            ))
        })
    }
}

pub struct ExitPlanModeTool {
    fields: JsObject,
}

impl ExitPlanModeTool {
    pub fn new() -> Self {
        Self {
            fields: object([
                ("name", string("exit_plan_mode")),
                ("aliases", JsValue::Array(vec![string("ExitPlanMode")])),
                ("description", string(EXIT_PLAN_MODE_DESCRIPTION)),
                ("kind", string("session")),
                (
                    "requiredRuntimeCapabilities",
                    JsValue::Array(vec![string("plan_workflow"), string("user_interaction")]),
                ),
                (
                    "inputSchema",
                    JsValue::Object(schema(
                        r#"{"type":"object","required":["plan_file_path"],"additionalProperties":false,"properties":{"plan_file_path":{"type":"string","description":"Path to the markdown plan file to submit from the current project's `.pilotdeck/plans` directory."}}}"#,
                    )),
                ),
            ]),
        }
    }
}

impl Default for ExitPlanModeTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolDefinition for ExitPlanModeTool {
    fn fields(&self) -> &JsObject {
        &self.fields
    }
    fn is_read_only(&self, _input: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(true))
    }
    fn is_concurrency_safe(&self, _input: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(true))
    }
    fn requires_user_interaction(&self, _input: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(true))
    }
}

/// `getExitPlanAction`.
fn exit_action(answer: &JsValue) -> Option<&'static str> {
    if field(answer, "type") != &JsValue::from("answered") {
        return None;
    }
    let pick = |value: &JsValue| {
        [EXIT_PLAN_MODE_CONTINUE, EXIT_PLAN_MODE_EXECUTE]
            .into_iter()
            .find(|action| {
                value
                    .as_string()
                    .is_some_and(|value| value.equals_str(action))
            })
    };
    let answers = field(answer, "answers").as_object()?;
    for (_, value) in answers.own_entries() {
        if let JsValue::Array(items) = value {
            if let Some(action) = items.iter().find_map(pick) {
                return Some(action);
            }
            continue;
        }
        if let Some(action) = pick(value) {
            return Some(action);
        }
    }
    None
}

/// `getExitPlanFeedback`.
fn exit_feedback(answer: &JsValue) -> Option<String> {
    if field(answer, "type") != &JsValue::from("answered") {
        return None;
    }
    let annotations = field(answer, "annotations").as_object()?;
    annotations
        .own_entries()
        .into_iter()
        .find_map(|(_, annotation)| {
            field(annotation, "notes")
                .as_string()
                .map(|notes| crate::metadata::js_trim(&text(notes)).to_owned())
                .filter(|notes| !notes.is_empty())
        })
}

static PLAN_TITLE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?m)^#\s+(.+)$").expect("plan title regex"));

/// The first two non-heading lines, cut at 200 UTF-16 units.
fn plan_summary(plan: &str) -> Option<String> {
    let lines: Vec<&str> = plan
        .split('\n')
        .filter(|line| !crate::metadata::js_trim(line).is_empty() && !line.starts_with('#'))
        .take(2)
        .collect();
    let joined: Vec<u16> = lines.join("\n").encode_utf16().take(200).collect();
    (!joined.is_empty()).then(|| String::from_utf16_lossy(&joined))
}

impl ExecutableTool for ExitPlanModeTool {
    fn execute(&self, input: JsValue, context: ToolRunContext) -> ToolFuture<ToolOutput> {
        Box::pin(async move {
            if !in_plan_mode(&context) {
                return Err(ToolError::failed(
                    "exit_plan_mode can only be used while plan mode is active.",
                ));
            }
            let channel = context
                .host
                .elicitation
                .clone()
                .and_then(|channel| channel.downcast::<ToolElicitation>().ok())
                .ok_or_else(|| {
                    ToolError::new(
                        "unsupported_tool",
                        "exit_plan_mode requires a connected user interaction channel.",
                    )
                })?;
            let Some(directory) = context.host.plan_directory.as_ref() else {
                return Err(ToolError::new("invalid_tool_input", PLAN_PATH_MESSAGE));
            };
            let requested = field(&input, "plan_file_path")
                .as_string()
                .cloned()
                .unwrap_or_default();
            let failed = |error: HostError| ToolError::failed(error.message);
            let resolved = match &directory.resolve(requested.clone()).await.map_err(failed)? {
                JsValue::String(path) if !path.units().is_empty() => path.clone(),
                _ => return Err(ToolError::new("invalid_tool_input", PLAN_PATH_MESSAGE)),
            };
            let plan = match &directory.read(requested).await.map_err(failed)? {
                JsValue::String(plan) => crate::metadata::js_trim(&text(plan)).to_owned(),
                _ => {
                    return Err(ToolError::new(
                        "invalid_tool_input",
                        format!(
                            "Plan file does not exist or could not be read: {}",
                            text(&resolved)
                        ),
                    ));
                }
            };
            if plan.is_empty() {
                return Err(ToolError::new(
                    "invalid_tool_input",
                    "Plan file is empty. Write your plan first before calling exit_plan_mode.",
                ));
            }
            let turn = field(&JsValue::Object(context.fields.clone()), "turnId")
                .as_string()
                .cloned()
                .unwrap_or_default();
            let option = |label: &str, description: &str| {
                JsValue::Object(object([
                    ("label", string(label)),
                    ("description", string(description)),
                ]))
            };
            let questions = JsValue::Array(vec![JsValue::Object(object([
                ("question", string(EXIT_PLAN_MODE_QUESTION)),
                ("header", string("Plan")),
                (
                    "options",
                    JsValue::Array(vec![
                        option(
                            EXIT_PLAN_MODE_CONTINUE,
                            "Keep planning and update the plan before any implementation starts.",
                        ),
                        option(
                            EXIT_PLAN_MODE_EXECUTE,
                            "Leave plan mode and let the agent execute this plan.",
                        ),
                    ]),
                ),
            ]))]);
            let answer = channel
                .0
                .ask_user(
                    ElicitationRequest {
                        tool_call_id: turn.clone(),
                        tool_name: "exit_plan_mode".into(),
                        preview_format: Some("markdown".into()),
                        questions,
                        metadata: Some(JsValue::Object(object([
                            ("source", string("exit_plan_mode")),
                            ("plan", string(plan.as_str())),
                            ("planFilePath", JsValue::String(resolved.clone())),
                        ]))),
                    },
                    context.clone(),
                )
                .await?;
            let action = exit_action(&answer);
            let feedback = exit_feedback(&answer);
            let cancelled = field(&answer, "type") == &JsValue::from("cancelled");
            let Some(action) = action.filter(|_| !cancelled) else {
                return Ok(text_output(
                    "Exit plan mode was cancelled. Stay in plan mode and continue refining the plan file.",
                    object([
                        ("plan", string(plan.as_str())),
                        ("action", string("cancelled")),
                    ]),
                ));
            };
            if action == EXIT_PLAN_MODE_EXECUTE {
                if let Some(plan_todo) = context
                    .host
                    .plan_todo
                    .clone()
                    .and_then(|handle| handle.downcast::<PlanTodoRuntime>().ok())
                {
                    plan_todo
                        .mark_plan_approved(&plan.as_str().into(), &turn)
                        .await
                        .map_err(failed)?;
                }
                let mut data = object([
                    ("plan", string(plan.as_str())),
                    ("action", string(action)),
                    ("requestedMode", string("default")),
                    ("planFilePath", JsValue::String(resolved.clone())),
                ]);
                if let Some(title) = PLAN_TITLE
                    .captures(&plan)
                    .and_then(|captures| captures.get(1))
                {
                    data.insert("planTitle".into(), string(title.as_str()));
                }
                if let Some(summary) = plan_summary(&plan) {
                    data.insert("planSummary".into(), string(summary));
                }
                return Ok(text_output(approved_result(&plan, &text(&resolved)), data));
            }
            let mut data = object([("plan", string(plan.as_str())), ("action", string(action))]);
            if let Some(feedback) = &feedback {
                data.insert("feedback".into(), string(feedback.as_str()));
            }
            Ok(text_output(continue_result(feedback.as_deref()), data))
        })
    }
}

/// `createEnterPlanModeTool()` / `createExitPlanModeTool()`.
pub fn plan_mode_tools() -> Vec<Arc<dyn ExecutableTool>> {
    vec![
        Arc::new(EnterPlanModeTool::new()),
        Arc::new(ExitPlanModeTool::new()),
    ]
}
