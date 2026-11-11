//! `todo_write` (baseline `tool/builtin/todoWrite.ts`): reads or rewrites the
//! session checklist through `context.planTodo`
//! ([`PlanTodoRuntime`]). Without a plan/todo provider the input is only
//! normalized and echoed, as in the baseline.
use super::{field, object, schema, string, text};
use crate::{
    host::{HostError, modules::capability::ToolDefinition},
    runtime::{
        plan_todo::PlanTodoRuntime,
        tools::{ExecutableTool, ToolError, ToolFuture, ToolOutput, ToolRunContext, js_space},
    },
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::sync::Arc;

const DESCRIPTION: &str = "Read or update a lightweight session checklist for complex work. Call with no arguments to read the current todo list. For editable todos, provide `todos` with stable ids and optional `merge=true` to update existing items by id or append new items. For legacy checklist updates, provide `markdown` using `- [x]` for completed items and `- [ ]` for remaining items. Use for complex 3+ step tasks, multi-deliverable work, codebase exploration, batch processing, information gathering, or generation work. Do a small amount of exploration before writing a detailed list; use todos as checkable checkpoints, not a rigid plan lock. Use status pending, in_progress, completed, or cancelled. Keep only one item in_progress when possible. Mark an item completed only after checking relevant evidence. If an approach fails or assumptions change, mark the old item cancelled and add a revised item instead of silently rewriting it as completed. When changing the todo structure mid-task, include `reason` so the change is auditable in the tool result. Prefer a final verification checkpoint that checks outputs, format, counts, extra artifacts, and key constraints when applicable. This tool only updates a checklist; it does not write files, submit, or replace a final plan. In plan mode, do not use todo_write to write the plan itself, and do not treat a todo list as the final plan. You may use todo_write in plan mode only to organize planning work such as exploration, analysis, writing a markdown plan under `.pilotdeck/plans/`, and submitting that plan with `exit_plan_mode`.";

const INPUT_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"markdown":{"type":"string","description":"Legacy markdown checklist content using `- [ ]` and `- [x]` items. Replaces the current list."},"todos":{"type":"array","description":"Editable todo items. Omit to read current list. With merge=true, items may include only id plus the fields to update.","items":{"type":"object","additionalProperties":false,"properties":{"id":{"type":"string","description":"Stable todo identifier. Required for reliable merge updates to existing items."},"content":{"type":"string","description":"Todo item description."},"status":{"type":"string","enum":["pending","in_progress","completed","cancelled"],"description":"Current todo status."},"priority":{"type":"string","description":"Optional priority label such as high, medium, or low."}}}},"merge":{"type":"boolean","description":"When todos are provided, true updates existing items by id and appends new items; false replaces the list.","default":false},"reason":{"type":"string","description":"Optional reason for structural todo changes, especially added/cancelled/reordered items."}}}"#;

pub struct TodoWriteTool {
    fields: JsObject,
}

impl TodoWriteTool {
    pub fn new() -> Self {
        Self {
            fields: object([
                ("name", string("todo_write")),
                ("aliases", JsValue::Array(vec![string("TodoWrite")])),
                ("description", string(DESCRIPTION)),
                ("kind", string("session")),
                ("inputSchema", JsValue::Object(schema(INPUT_SCHEMA))),
            ]),
        }
    }
}

impl Default for TodoWriteTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolDefinition for TodoWriteTool {
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

fn trim(value: &str) -> &str {
    value.trim_matches(js_space)
}

/// One line against `/^\s*[-*]\s+\[( |x|X)\]\s+(.*?)\s*$/u`: `(checked,
/// content)` with the content already trimmed.
fn todo_line(line: &str) -> Option<(bool, &str)> {
    let rest = line.trim_start_matches(js_space);
    let rest = rest.strip_prefix(['-', '*'])?;
    let after = rest.trim_start_matches(js_space);
    if after.len() == rest.len() {
        return None;
    }
    let rest = after.strip_prefix('[')?;
    let mut chars = rest.chars();
    let checked = match chars.next()? {
        ' ' => false,
        'x' | 'X' => true,
        _ => return None,
    };
    let rest = chars.as_str().strip_prefix(']')?;
    let after = rest.trim_start_matches(js_space);
    if after.len() == rest.len() {
        return None;
    }
    let content = after.trim_end_matches(js_space);
    // `.` does not cross a line terminator, and `$` is the end of the line.
    if content.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
        return None;
    }
    Some((checked, content))
}

/// `parseTodoMarkdown`: checked items are completed, the first unchecked one
/// is in progress, the rest pending.
pub fn parse_todo_markdown(markdown: &str) -> Vec<JsValue> {
    let mut assigned_in_progress = false;
    markdown
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .filter_map(todo_line)
        .filter(|(_, content)| !content.is_empty())
        .enumerate()
        .map(|(index, (checked, content))| {
            let status = if checked {
                "completed"
            } else if !assigned_in_progress {
                assigned_in_progress = true;
                "in_progress"
            } else {
                "pending"
            };
            JsValue::Object(object([
                ("id", string(format!("todo-{}", index + 1))),
                ("content", string(content)),
                ("status", string(status)),
            ]))
        })
        .collect()
}

/// `normalizeTodoUpdatesForFallback`.
fn normalize_for_fallback(todos: &[JsValue]) -> Vec<JsValue> {
    todos
        .iter()
        .enumerate()
        .map(|(index, todo)| {
            let trimmed = |key: &str| {
                field(todo, key)
                    .as_string()
                    .map(|value| trim(&text(value)).to_owned())
                    .filter(|value| !value.is_empty())
            };
            let mut item = object([
                (
                    "id",
                    string(trimmed("id").unwrap_or_else(|| format!("todo-{}", index + 1))),
                ),
                (
                    "content",
                    string(trimmed("content").unwrap_or_else(|| "(no description)".into())),
                ),
                (
                    "status",
                    match field(todo, "status") {
                        JsValue::Undefined | JsValue::Null => string("pending"),
                        status => status.clone(),
                    },
                ),
            ]);
            if let Some(priority) = trimmed("priority") {
                item.insert("priority".into(), string(priority));
            }
            JsValue::Object(item)
        })
        .collect()
}

/// `formatTodoWriteResult`.
fn format_result(
    mode: &str,
    todos: &[JsValue],
    merge: bool,
    reason: Option<&str>,
    diagnostics: Option<&JsObject>,
) -> Result<String, ToolError> {
    let display = |value: &JsValue| {
        crate::js_value::display(value)
            .map_err(|_| ToolError::failed(crate::js_value::StackOverflow::MESSAGE))
    };
    let mut lines = vec![
        if mode == "read" {
            "Todo list read:".to_owned()
        } else {
            "Todo list updated:".to_owned()
        },
        format!("mode={mode} merge={merge} count={}", todos.len()),
    ];
    if let Some(reason) = reason {
        lines.push(format!("reason: {reason}"));
    }
    if todos.is_empty() {
        lines.push("No todos are currently recorded.".into());
    }
    for todo in todos {
        let priority = match field(todo, "priority") {
            priority if super::truthy(priority) => format!(" priority={}", display(priority)?),
            _ => String::new(),
        };
        lines.push(format!(
            "- [{}] id={}{priority} {}",
            display(field(todo, "status"))?,
            display(field(todo, "id"))?,
            display(field(todo, "content"))?,
        ));
    }
    if let Some(diagnostics) = diagnostics {
        lines.push(String::new());
        lines.push("Diagnostics:".into());
        lines.push(
            crate::js_value::json_stringify(&JsValue::Object(diagnostics.clone()))
                .map_err(|_| ToolError::failed(crate::js_value::StackOverflow::MESSAGE))?
                .unwrap_or_default(),
        );
    }
    Ok(lines.join("\n"))
}

impl ExecutableTool for TodoWriteTool {
    fn execute(&self, input: JsValue, context: ToolRunContext) -> ToolFuture<ToolOutput> {
        Box::pin(async move {
            let plan_todo: Option<Arc<PlanTodoRuntime>> = context
                .host
                .plan_todo
                .clone()
                .and_then(|handle| handle.downcast::<PlanTodoRuntime>().ok());
            let turn: JsString = context
                .fields
                .get("turnId")
                .and_then(JsValue::as_string)
                .cloned()
                .unwrap_or_default();
            let failed = |error: HostError| ToolError::failed(error.message);
            let merge = super::truthy(field(&input, "merge"));
            let reason: Option<JsString> = field(&input, "reason")
                .as_string()
                .map(|reason| JsString::from(trim(&text(reason))));
            let reason_text = reason
                .as_ref()
                .map(text)
                .filter(|reason| !reason.is_empty());
            let markdown = field(&input, "markdown").as_string();

            let (mode, todos) = if let JsValue::Array(updates) = field(&input, "todos") {
                let todos = match &plan_todo {
                    Some(plan) => plan
                        .write_todos(updates, &turn, merge, None, reason.as_ref())
                        .await
                        .map_err(failed)?,
                    None => normalize_for_fallback(updates),
                };
                ("structured", todos)
            } else if let Some(markdown) = markdown {
                let parsed = parse_todo_markdown(&text(markdown));
                let todos = match &plan_todo {
                    Some(plan) => plan
                        .record_todo_write(markdown, &parsed, &turn, reason.as_ref())
                        .await
                        .map_err(failed)?,
                    None => parsed,
                };
                ("markdown", todos)
            } else {
                (
                    "read",
                    plan_todo
                        .as_ref()
                        .map(|plan| plan.snapshot().todos)
                        .unwrap_or_default(),
                )
            };
            let diagnostics = plan_todo.as_ref().map(|plan| plan.snapshot().diagnostics);

            let rendered = format_result(
                mode,
                &todos,
                merge,
                reason_text.as_deref(),
                diagnostics.as_ref(),
            )?;
            let mut data = JsObject::new();
            if let Some(markdown) = markdown {
                data.insert("markdown".into(), JsValue::String(markdown.clone()));
            }
            data.insert("todos".into(), JsValue::Array(todos.clone()));
            data.insert("mode".into(), string(mode));
            data.insert("merge".into(), JsValue::Bool(merge));
            if let Some(reason) = &reason_text {
                data.insert("reason".into(), string(reason.as_str()));
            }
            let mut metadata = object([
                ("todoCount", JsValue::Number(todos.len() as f64)),
                ("mode", string(mode)),
            ]);
            if let Some(diagnostics) = diagnostics {
                data.insert("diagnostics".into(), JsValue::Object(diagnostics.clone()));
                metadata.insert("diagnostics".into(), JsValue::Object(diagnostics));
            }
            Ok(ToolOutput {
                content: vec![JsValue::Object(object([
                    ("type", string("text")),
                    ("text", string(rendered)),
                ]))],
                data: Some(JsValue::Object(data)),
                metadata: Some(metadata),
                supplemental_messages: None,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilotdeck_protocol::stringify_json;

    // Expected output: baseline `parseTodoMarkdown` on the same text (bbx
    // `tool-todo-write`, turn 4, plus the CRLF and lone-CR lines).
    #[test]
    fn markdown_lines_follow_the_baseline_pattern() {
        let todos = parse_todo_markdown(
            "- [x] done item\r\n* [ ] next item\n  - [X]   spaced item  \nnot a todo\n-[ ] no gap\n- [] empty box\n- [ ] \n- [ ] a\rb\n- [ ] later item",
        );
        assert_eq!(
            stringify_json(&JsValue::Array(todos)).unwrap().unwrap(),
            r#"[{"id":"todo-1","content":"done item","status":"completed"},{"id":"todo-2","content":"next item","status":"in_progress"},{"id":"todo-3","content":"spaced item","status":"completed"},{"id":"todo-4","content":"later item","status":"pending"}]"#
        );
    }
}
