//! Pure `todo_write` definition, markdown parsing, and merge/replace updates
//! aligned with PilotDeck baseline `todo_write` / `plan_todo`.

use pilotdeck_protocol::{JsObject, JsValue};

use crate::js_util::{object, schema, string};

pub const TODO_TOOL_NAME: &str = "todo_write";
pub const VALID_TODO_STATUSES: [&str; 4] = ["pending", "in_progress", "completed", "cancelled"];

const DESCRIPTION: &str = "Read or update a lightweight session checklist for complex work. Call with no arguments to read the current todo list. For editable todos, provide `todos` with stable ids and optional `merge=true` to update existing items by id or append new items. For legacy checklist updates, provide `markdown` using `- [x]` for completed items and `- [ ]` for remaining items. Use for complex 3+ step tasks, multi-deliverable work, codebase exploration, batch processing, information gathering, or generation work. Do a small amount of exploration before writing a detailed list; use todos as checkable checkpoints, not a rigid plan lock. Use status pending, in_progress, completed, or cancelled. Keep only one item in_progress when possible. Mark an item completed only after checking relevant evidence. If an approach fails or assumptions change, mark the old item cancelled and add a revised item instead of silently rewriting it as completed. When changing the todo structure mid-task, include `reason` so the change is auditable in the tool result. Prefer a final verification checkpoint that checks outputs, format, counts, extra artifacts, and key constraints when applicable. This tool only updates a checklist; it does not write files, submit, or replace a final plan. In plan mode, do not use todo_write to write the plan itself, and do not treat a todo list as the final plan. You may use todo_write in plan mode only to organize planning work such as exploration, analysis, writing a markdown plan under `.pilotdeck/plans/`, and submitting that plan with `exit_plan_mode`.";

const INPUT_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"markdown":{"type":"string","description":"Legacy markdown checklist content using `- [ ]` and `- [x]` items. Replaces the current list."},"todos":{"type":"array","description":"Editable todo items. Omit to read current list. With merge=true, items may include only id plus the fields to update.","items":{"type":"object","additionalProperties":false,"properties":{"id":{"type":"string","description":"Stable todo identifier. Required for reliable merge updates to existing items."},"content":{"type":"string","description":"Todo item description."},"status":{"type":"string","enum":["pending","in_progress","completed","cancelled"],"description":"Current todo status."},"priority":{"type":"string","description":"Optional priority label such as high, medium, or low."}}}},"merge":{"type":"boolean","description":"When todos are provided, true updates existing items by id and appends new items; false replaces the list.","default":false},"reason":{"type":"string","description":"Optional reason for structural todo changes, especially added/cancelled/reordered items."}}}"#;

/// Valid status values for a [`TodoItem`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl TodoStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim();
        match trimmed {
            "pending" => Ok(Self::Pending),
            "in_progress" => Ok(Self::InProgress),
            "completed" => Ok(Self::Completed),
            "cancelled" => Ok(Self::Cancelled),
            "" => Err(format!(
                "Todo status must be non-empty and one of: {}",
                VALID_TODO_STATUSES.join(", ")
            )),
            other => Err(format!(
                "Invalid todo status '{other}'; expected one of: {}",
                VALID_TODO_STATUSES.join(", ")
            )),
        }
    }
}

/// One normalized checklist entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: String,
    pub priority: Option<String>,
}

/// Pure request payload for `todo_write`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TodoWriteRequest {
    pub markdown: Option<String>,
    pub todos: Option<Vec<TodoItem>>,
    pub merge: bool,
    pub reason: Option<String>,
}

/// Return the canonical `todo_write` tool definition matching the PilotDeck baseline.
pub fn todo_definition() -> JsObject {
    object([
        ("name", string(TODO_TOOL_NAME)),
        ("aliases", JsValue::Array(vec![string("TodoWrite")])),
        ("description", string(DESCRIPTION)),
        ("kind", string("session")),
        ("inputSchema", JsValue::Object(schema(INPUT_SCHEMA))),
    ])
}

fn js_space(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000D}'
            | ' '
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(js_space)
}

/// Match one line against `/^\s*[-*]\s+\[( |x|X)\]\s+(.*?)\s*$/u`.
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
    if content.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
        return None;
    }
    Some((checked, content))
}

/// Parse legacy markdown checklists (`- [x]` / `- [ ]` / `* [ ]`) into [`TodoItem`]s.
/// Checked items become `completed`, the first unchecked becomes `in_progress`,
/// and subsequent unchecked items become `pending`.
pub fn parse_todo_markdown(markdown: &str) -> Vec<TodoItem> {
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
            TodoItem {
                id: format!("todo-{}", index + 1),
                content: content.to_owned(),
                status: status.to_owned(),
                priority: None,
            }
        })
        .collect()
}

fn fallback_id(index: usize) -> String {
    format!("todo-{}", index + 1)
}

fn resolved_id(raw: &str, index: usize) -> String {
    let trimmed = trim_js(raw);
    if trimmed.is_empty() {
        fallback_id(index)
    } else {
        trimmed.to_owned()
    }
}

fn normalized_priority(priority: &Option<String>) -> Option<String> {
    priority
        .as_deref()
        .map(trim_js)
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
}

fn dedupe_last_wins(items: &[TodoItem]) -> Vec<(usize, &TodoItem)> {
    let ids: Vec<String> = items
        .iter()
        .enumerate()
        .map(|(index, item)| resolved_id(&item.id, index))
        .collect();
    items
        .iter()
        .enumerate()
        .filter(|(index, _)| !ids[*index + 1..].contains(&ids[*index]))
        .collect()
}

fn normalize_new_item(item: &TodoItem, index: usize) -> Result<TodoItem, String> {
    let status = TodoStatus::parse(&item.status)?.as_str().to_owned();
    let content = trim_js(&item.content);
    let content = if content.is_empty() {
        "(no description)".to_owned()
    } else {
        content.to_owned()
    };
    Ok(TodoItem {
        id: resolved_id(&item.id, index),
        content,
        status,
        priority: normalized_priority(&item.priority),
    })
}

fn push_unique(merged: &mut Vec<TodoItem>, seen: &mut Vec<String>, item: TodoItem) {
    if !seen.contains(&item.id) {
        seen.push(item.id.clone());
        merged.push(item);
    }
}

/// Apply a [`TodoWriteRequest`] over `existing` items.
///
/// - When `todos` is `Some`:
///   - `merge == false`: validates non-empty `status` in `VALID_TODO_STATUSES`,
///     deduplicates by `id` (last wins), and replaces the list.
///   - `merge == true`: merges updates by `id` (preserving existing order and
///     unmodified fields when an update supplies an empty status/content, while
///     validating any non-empty status against `VALID_TODO_STATUSES`) and appends
///     new items (which must carry a non-empty valid status).
/// - When `todos` is `None` and `markdown` is `Some`: parses checkboxes via
///   [`parse_todo_markdown`] and replaces the list.
/// - When both are `None`: returns `existing` unchanged (read mode).
pub fn apply_todo_update(
    existing: &[TodoItem],
    request: &TodoWriteRequest,
) -> Result<Vec<TodoItem>, String> {
    if let Some(updates) = &request.todos {
        if !request.merge {
            // Validate every incoming item first so no invalid item is silently dropped by dedupe.
            for item in updates {
                TodoStatus::parse(&item.status)?;
            }
            return dedupe_last_wins(updates)
                .into_iter()
                .enumerate()
                .map(|(index, (_, item))| normalize_new_item(item, index))
                .collect();
        }

        let mut by_id: Vec<(String, TodoItem)> = Vec::new();
        let put = |by_id: &mut Vec<(String, TodoItem)>, id: String, value: TodoItem| {
            if let Some(slot) = by_id.iter_mut().find(|(known, _)| known == &id) {
                slot.1 = value;
            } else {
                by_id.push((id, value));
            }
        };

        for (index, item) in existing.iter().enumerate() {
            let mut normalized = item.clone();
            normalized.id = resolved_id(&item.id, index);
            let id = normalized.id.clone();
            put(&mut by_id, id, normalized);
        }

        let mut append: Vec<&TodoItem> = Vec::new();
        for (_, update) in dedupe_last_wins(updates) {
            let trimmed_id = trim_js(&update.id);
            let existing_match = (!trimmed_id.is_empty())
                .then(|| by_id.iter().find(|(known, _)| known == trimmed_id))
                .flatten()
                .map(|(_, item)| item.clone());

            let Some(mut current) = existing_match else {
                TodoStatus::parse(&update.status)?;
                append.push(update);
                continue;
            };

            let status_trimmed = trim_js(&update.status);
            if !status_trimmed.is_empty() {
                current.status = TodoStatus::parse(status_trimmed)?.as_str().to_owned();
            }
            let content_trimmed = trim_js(&update.content);
            if !content_trimmed.is_empty() {
                current.content = content_trimmed.to_owned();
            }
            if let Some(priority) = normalized_priority(&update.priority) {
                current.priority = Some(priority);
            }
            let id = current.id.clone();
            put(&mut by_id, id, current);
        }

        let mut merged: Vec<TodoItem> = Vec::new();
        let mut seen: Vec<String> = Vec::new();

        for (index, item) in existing.iter().enumerate() {
            let id = resolved_id(&item.id, index);
            if let Some((_, updated)) = by_id.iter().find(|(known, _)| known == &id) {
                push_unique(&mut merged, &mut seen, updated.clone());
            }
        }
        let first_new = merged.len();
        for (index, update) in append.into_iter().enumerate() {
            push_unique(
                &mut merged,
                &mut seen,
                normalize_new_item(update, first_new + index)?,
            );
        }
        return Ok(merged);
    }

    if let Some(markdown) = &request.markdown {
        return Ok(parse_todo_markdown(markdown));
    }

    Ok(existing.to_vec())
}

/// Decode a validated `todo_write` JSON input object into a [`TodoWriteRequest`].
pub fn parse_todo_write_request(input: &JsValue) -> Result<TodoWriteRequest, String> {
    let obj = input
        .as_object()
        .ok_or_else(|| "todo_write input must be a JSON object".to_owned())?;
    let markdown = obj
        .get("markdown")
        .and_then(JsValue::as_string)
        .map(|s| String::from_utf16_lossy(s.units()));
    let reason = obj
        .get("reason")
        .and_then(JsValue::as_string)
        .map(|s| trim_js(&String::from_utf16_lossy(s.units())).to_owned())
        .filter(|s| !s.is_empty());
    let merge = obj.get("merge").and_then(JsValue::as_bool).unwrap_or(false);
    let todos = match obj.get("todos") {
        None | Some(JsValue::Undefined) => None,
        Some(JsValue::Array(items)) => {
            let mut parsed = Vec::with_capacity(items.len());
            for item in items {
                let item_obj = item
                    .as_object()
                    .ok_or_else(|| "Each todo entry must be an object".to_owned())?;
                let str_field = |key: &str| {
                    item_obj
                        .get(key)
                        .and_then(JsValue::as_string)
                        .map(|s| String::from_utf16_lossy(s.units()))
                };
                parsed.push(TodoItem {
                    id: str_field("id").unwrap_or_default(),
                    content: str_field("content").unwrap_or_default(),
                    status: str_field("status").unwrap_or_default(),
                    priority: str_field("priority"),
                });
            }
            Some(parsed)
        }
        Some(_) => return Err("todos must be an array".to_owned()),
    };
    Ok(TodoWriteRequest {
        markdown,
        todos,
        merge,
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::validate_tool_input;
    use pilotdeck_protocol::parse_json;

    #[test]
    fn definition_matches_baseline_contract() {
        let def = todo_definition();
        assert_eq!(def.get("name"), Some(&string("todo_write")));
        assert_eq!(
            def.get("aliases"),
            Some(&JsValue::Array(vec![string("TodoWrite")]))
        );
        let schema = def.get("inputSchema").unwrap();
        assert!(
            validate_tool_input(
                schema,
                &parse_json(r#"{"todos":[{"id":"1","content":"step","status":"pending"}]}"#)
                    .unwrap()
            )
            .is_ok()
        );
        assert!(
            validate_tool_input(
                schema,
                &parse_json(r#"{"todos":[{"id":"1","status":"invalid"}]}"#).unwrap()
            )
            .is_err()
        );
    }

    #[test]
    fn markdown_lines_follow_the_baseline_pattern() {
        let todos = parse_todo_markdown(
            "- [x] done item\r\n* [ ] next item\n  - [X]   spaced item  \nnot a todo\n-[ ] no gap\n- [] empty box\n- [ ] \n- [ ] a\rb\n- [ ] later item",
        );
        assert_eq!(
            todos,
            vec![
                TodoItem {
                    id: "todo-1".into(),
                    content: "done item".into(),
                    status: "completed".into(),
                    priority: None,
                },
                TodoItem {
                    id: "todo-2".into(),
                    content: "next item".into(),
                    status: "in_progress".into(),
                    priority: None,
                },
                TodoItem {
                    id: "todo-3".into(),
                    content: "spaced item".into(),
                    status: "completed".into(),
                    priority: None,
                },
                TodoItem {
                    id: "todo-4".into(),
                    content: "later item".into(),
                    status: "pending".into(),
                    priority: None,
                },
            ]
        );
    }

    #[test]
    fn apply_update_validates_status_and_handles_replace_merge_and_markdown() {
        let initial = apply_todo_update(
            &[],
            &TodoWriteRequest {
                todos: Some(vec![
                    TodoItem {
                        id: "t1".into(),
                        content: "inspect".into(),
                        status: "in_progress".into(),
                        priority: Some("high".into()),
                    },
                    TodoItem {
                        id: "t2".into(),
                        content: "verify".into(),
                        status: "pending".into(),
                        priority: None,
                    },
                ]),
                merge: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(initial.len(), 2);

        // Merge updates existing by id and appends new items.
        let merged = apply_todo_update(
            &initial,
            &TodoWriteRequest {
                todos: Some(vec![
                    TodoItem {
                        id: "t1".into(),
                        content: "".into(),
                        status: "completed".into(),
                        priority: None,
                    },
                    TodoItem {
                        id: "t3".into(),
                        content: "ship".into(),
                        status: "pending".into(),
                        priority: Some("low".into()),
                    },
                ]),
                merge: true,
                reason: Some("progress".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].content, "inspect");
        assert_eq!(merged[0].status, "completed");
        assert_eq!(merged[0].priority.as_deref(), Some("high"));
        assert_eq!(merged[1].id, "t2");
        assert_eq!(merged[2].id, "t3");

        // Replace mode overwrites list.
        let replaced = apply_todo_update(
            &merged,
            &TodoWriteRequest {
                todos: Some(vec![TodoItem {
                    id: "t9".into(),
                    content: "only".into(),
                    status: "cancelled".into(),
                    priority: None,
                }]),
                merge: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(replaced.len(), 1);
        assert_eq!(replaced[0].id, "t9");

        // Read mode returns existing unchanged.
        assert_eq!(
            apply_todo_update(&replaced, &TodoWriteRequest::default()).unwrap(),
            replaced
        );

        // Markdown replaces existing.
        let md = apply_todo_update(
            &replaced,
            &TodoWriteRequest {
                markdown: Some("- [ ] alpha\n- [x] beta".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(md.len(), 2);
        assert_eq!(md[0].status, "in_progress");
        assert_eq!(md[1].status, "completed");

        // Empty or invalid status fails.
        assert!(
            apply_todo_update(
                &[],
                &TodoWriteRequest {
                    todos: Some(vec![TodoItem {
                        id: "bad".into(),
                        content: "x".into(),
                        status: "".into(),
                        priority: None,
                    }]),
                    merge: false,
                    ..Default::default()
                }
            )
            .is_err()
        );
        assert!(
            apply_todo_update(
                &initial,
                &TodoWriteRequest {
                    todos: Some(vec![TodoItem {
                        id: "t1".into(),
                        content: "x".into(),
                        status: "done".into(),
                        priority: None,
                    }]),
                    merge: true,
                    ..Default::default()
                }
            )
            .is_err()
        );
    }
}
