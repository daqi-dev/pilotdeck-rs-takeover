//! Session plan/todo state (baseline `plan-todo/`): the `plan-todo.state`
//! reducer over the session's durable entries and the runtime that appends
//! `plan_todo_*` events. State is never kept beside the log: every read folds
//! the live entry list, like the baseline's projection driver.
use crate::{
    host::{
        HostError, HostFuture, ModuleError,
        modules::{
            capability::{PlanTodoHandler, RuntimeContext},
            tool_context::SessionService,
        },
    },
    runtime::{memory::js_trim_units, session::factory::SessionProjection},
    storage::recorder::Recorder,
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::sync::Arc;

const STATUSES: [&str; 4] = ["pending", "in_progress", "completed", "cancelled"];

fn object<const N: usize>(entries: [(&str, JsValue); N]) -> JsObject {
    let mut result = JsObject::new();
    for (key, value) in entries {
        result.insert(key.into(), value);
    }
    result
}
fn number(value: usize) -> JsValue {
    JsValue::Number(value as f64)
}
fn trim(value: &JsString) -> JsString {
    JsString::from_utf16(js_trim_units(value.units()).to_vec())
}
/// `value?.trim()` when the result is truthy.
fn trimmed(value: Option<&JsValue>) -> Option<JsString> {
    value
        .and_then(JsValue::as_string)
        .map(trim)
        .filter(|text| !text.is_empty())
}
fn text<'a>(value: &'a JsValue, key: &str) -> Option<&'a JsString> {
    value.get(key).and_then(JsValue::as_string)
}
fn is(value: Option<&JsString>, expected: &str) -> bool {
    value.is_some_and(|text| text.equals_str(expected))
}
fn valid_status(value: Option<&JsString>) -> bool {
    STATUSES.iter().any(|status| is(value, status))
}
fn is_active(todo: &JsValue) -> bool {
    let status = text(todo, "status");
    is(status, "pending") || is(status, "in_progress")
}
fn count_status(todos: &[JsValue], status: &str) -> usize {
    todos
        .iter()
        .filter(|todo| is(text(todo, "status"), status))
        .count()
}
/// `todo.id ?? todo.content`.
fn key(todo: &JsValue) -> JsValue {
    match todo.get("id") {
        None | Some(JsValue::Undefined | JsValue::Null) => {
            todo.get("content").cloned().unwrap_or(JsValue::Undefined)
        }
        Some(id) => id.clone(),
    }
}
/// `new Map(todos.map(todo => [key, todo])).get(key)`: the last one wins.
fn find<'a>(todos: &'a [JsValue], wanted: &JsValue) -> Option<&'a JsValue> {
    todos.iter().rev().find(|todo| &key(todo) == wanted)
}
fn fallback_id(index: usize) -> JsString {
    format!("todo-{}", index + 1).into()
}

/// `normalizeTodoItem`.
fn normalize(item: &JsValue, index: usize) -> JsValue {
    let status = text(item, "status")
        .filter(|status| valid_status(Some(status)))
        .cloned()
        .unwrap_or_else(|| "pending".into());
    let mut todo = object([
        (
            "id",
            JsValue::String(trimmed(item.get("id")).unwrap_or_else(|| fallback_id(index))),
        ),
        (
            "content",
            JsValue::String(
                trimmed(item.get("content")).unwrap_or_else(|| "(no description)".into()),
            ),
        ),
        ("status", JsValue::String(status)),
    ]);
    if let Some(priority) = trimmed(item.get("priority")) {
        todo.insert("priority".into(), JsValue::String(priority));
    }
    JsValue::Object(todo)
}

/// `dedupeById`: one entry per id (blank ids fall back to their position),
/// the last occurrence, in input order.
fn dedupe(todos: &[JsValue]) -> Vec<&JsValue> {
    let ids: Vec<JsString> = todos
        .iter()
        .enumerate()
        .map(|(index, todo)| trimmed(todo.get("id")).unwrap_or_else(|| fallback_id(index)))
        .collect();
    todos
        .iter()
        .enumerate()
        .filter(|(index, _)| !ids[index + 1..].contains(&ids[*index]))
        .map(|(_, todo)| todo)
        .collect()
}

/// `replacePlanTodos`.
pub fn replace_todos(todos: &[JsValue]) -> Vec<JsValue> {
    dedupe(todos)
        .into_iter()
        .enumerate()
        .map(|(index, todo)| normalize(todo, index))
        .collect()
}

/// `mergePlanTodos`.
pub fn merge_todos(existing: &[JsValue], updates: &[JsValue]) -> Vec<JsValue> {
    // `existingById`: insertion-ordered, later duplicates replace in place.
    let mut by_id: Vec<(JsString, JsValue)> = Vec::new();
    let put = |by_id: &mut Vec<(JsString, JsValue)>, id: JsString, value: JsValue| match by_id
        .iter_mut()
        .find(|(known, _)| known == &id)
    {
        Some(slot) => slot.1 = value,
        None => by_id.push((id, value)),
    };
    for (index, todo) in existing.iter().enumerate() {
        let normalized = normalize(todo, index);
        let id = text(&normalized, "id").cloned().unwrap_or_default();
        put(&mut by_id, id, normalized);
    }
    let mut append = Vec::new();
    for update in dedupe(updates) {
        let id = trimmed(update.get("id"));
        let current = id
            .as_ref()
            .and_then(|id| by_id.iter().find(|(known, _)| known == id))
            .map(|(_, todo)| todo.clone());
        let (Some(id), Some(current)) = (id, current) else {
            append.push(update);
            continue;
        };
        let mut next = current.as_object().cloned().unwrap_or_default();
        if let Some(content) = trimmed(update.get("content")) {
            next.insert("content".into(), JsValue::String(content));
        }
        if let Some(status) = text(update, "status").filter(|s| valid_status(Some(s))) {
            next.insert("status".into(), JsValue::String(status.clone()));
        }
        if let Some(priority) = trimmed(update.get("priority")) {
            next.insert("priority".into(), JsValue::String(priority));
        }
        put(&mut by_id, id, JsValue::Object(next));
    }
    let mut merged: Vec<JsValue> = Vec::new();
    let mut seen: Vec<JsString> = Vec::new();
    let mut push = |merged: &mut Vec<JsValue>, todo: JsValue| {
        let id = text(&todo, "id").cloned().unwrap_or_default();
        if !seen.contains(&id) {
            seen.push(id);
            merged.push(todo);
        }
    };
    for (index, todo) in existing.iter().enumerate() {
        let id = trimmed(todo.get("id")).unwrap_or_else(|| fallback_id(index));
        let current = by_id
            .iter()
            .find(|(known, _)| known == &id)
            .map(|(_, todo)| todo.clone())
            .unwrap_or_else(|| normalize(todo, index));
        push(&mut merged, current);
    }
    let first_new = merged.len();
    for (index, todo) in append.into_iter().enumerate() {
        push(&mut merged, normalize(todo, first_new + index));
    }
    merged
}

/// `PilotDeckPlanTodoStateSnapshot`. `activeTodos` is derived from `todos`.
#[derive(Clone, Debug, Default)]
pub struct PlanTodoSnapshot {
    pub approved_plan: Option<JsString>,
    pub requires_initialization: bool,
    pub tool_calls_since_last_todo_write: f64,
    pub last_markdown: Option<JsValue>,
    pub todos: Vec<JsValue>,
    pub history: Vec<JsValue>,
    pub diagnostics: JsObject,
}
impl PlanTodoSnapshot {
    /// `createPlanTodoSnapshot`.
    pub fn new() -> Self {
        Self {
            diagnostics: object([
                ("writeCount", number(0)),
                ("todoCount", number(0)),
                ("activeCount", number(0)),
                ("completedCount", number(0)),
                ("cancelledCount", number(0)),
                ("largeRewriteCount", number(0)),
                ("deletedOpenItemCount", number(0)),
                ("completedWithoutActiveCount", number(0)),
            ]),
            ..Self::default()
        }
    }
    fn active(&self) -> Vec<JsValue> {
        self.todos
            .iter()
            .filter(|todo| is_active(todo))
            .cloned()
            .collect()
    }
    fn counter(&self, name: &str) -> f64 {
        self.diagnostics
            .get(name)
            .and_then(JsValue::as_number)
            .unwrap_or(0.0)
    }
    pub fn to_value(&self) -> JsValue {
        let mut value = JsObject::new();
        if let Some(plan) = &self.approved_plan {
            value.insert("approvedPlan".into(), JsValue::String(plan.clone()));
        }
        value.insert(
            "requiresInitialization".into(),
            JsValue::Bool(self.requires_initialization),
        );
        value.insert(
            "toolCallsSinceLastTodoWrite".into(),
            JsValue::Number(self.tool_calls_since_last_todo_write),
        );
        if let Some(markdown) = &self.last_markdown {
            value.insert("lastMarkdown".into(), markdown.clone());
        }
        value.insert("todos".into(), JsValue::Array(self.todos.clone()));
        value.insert("activeTodos".into(), JsValue::Array(self.active()));
        value.insert("todoHistory".into(), JsValue::Array(self.history.clone()));
        value.insert(
            "todoDiagnostics".into(),
            JsValue::Object(self.diagnostics.clone()),
        );
        JsValue::Object(value)
    }

    /// The projection's `reduce`.
    pub fn reduce(self, entry: &JsValue) -> Self {
        let kind = text(entry, "type");
        if is(kind, "plan_todo_plan_changed") {
            let approved_plan = trimmed(entry.get("plan"));
            Self {
                requires_initialization: approved_plan.is_some(),
                approved_plan,
                ..Self::new()
            }
        } else if is(kind, "plan_todo_written") {
            self.apply_write(entry)
        } else if is(kind, "plan_todo_progressed") {
            self.apply_progress(text(entry, "toolName"))
        } else {
            self
        }
    }

    fn apply_write(self, entry: &JsValue) -> Self {
        let next: Vec<JsValue> = match entry.get("todos") {
            Some(JsValue::Array(todos)) => todos.clone(),
            _ => Vec::new(),
        };
        let removed: Vec<&JsValue> = self
            .todos
            .iter()
            .filter(|todo| find(&next, &key(todo)).is_none())
            .collect();
        let added = next
            .iter()
            .filter(|todo| find(&self.todos, &key(todo)).is_none())
            .count();
        let changed = next
            .iter()
            .filter(|todo| {
                find(&self.todos, &key(todo)).is_some_and(|previous| {
                    ["content", "status", "priority"]
                        .iter()
                        .any(|field| previous.get(field) != todo.get(field))
                })
            })
            .count();
        let deleted_open = removed.iter().filter(|todo| is_active(todo)).count();
        let previous_active = self.todos.iter().filter(|todo| is_active(todo)).count();
        let preserved = next.len() - added;
        let large_rewrite =
            !self.todos.is_empty() && !next.is_empty() && preserved < self.todos.len().div_ceil(2);
        let next_active = next.iter().filter(|todo| is_active(todo)).count();
        let all_completed = !next.is_empty()
            && next_active == 0
            && next.iter().all(|todo| {
                is(text(todo, "status"), "completed") || is(text(todo, "status"), "cancelled")
            });
        let mode = entry.get("mode").cloned().unwrap_or(JsValue::Undefined);
        let merge = entry.get("merge").cloned().unwrap_or(JsValue::Undefined);
        let reason = trimmed(entry.get("reason"));
        let markdown = entry.get("markdown").filter(|value| !value.is_undefined());

        let mut last_write = object([("mode", mode.clone()), ("merge", merge.clone())]);
        if let Some(reason) = &reason {
            last_write.insert("reason".into(), JsValue::String(reason.clone()));
        }
        for (name, value) in [
            ("addedCount", number(added)),
            ("removedCount", number(removed.len())),
            ("changedCount", number(changed)),
            ("deletedOpenItemCount", number(deleted_open)),
            ("largeRewrite", JsValue::Bool(large_rewrite)),
            ("allCompleted", JsValue::Bool(all_completed)),
        ] {
            last_write.insert(name.into(), value);
        }
        let diagnostics = object([
            ("writeCount", number(self.history.len() + 1)),
            ("todoCount", number(next.len())),
            ("activeCount", number(next_active)),
            ("completedCount", number(count_status(&next, "completed"))),
            ("cancelledCount", number(count_status(&next, "cancelled"))),
            (
                "largeRewriteCount",
                JsValue::Number(
                    self.counter("largeRewriteCount") + f64::from(u8::from(large_rewrite)),
                ),
            ),
            (
                "deletedOpenItemCount",
                JsValue::Number(self.counter("deletedOpenItemCount") + deleted_open as f64),
            ),
            (
                "completedWithoutActiveCount",
                JsValue::Number(
                    self.counter("completedWithoutActiveCount")
                        + f64::from(u8::from(all_completed && previous_active > 0)),
                ),
            ),
            ("lastWrite", JsValue::Object(last_write)),
        ]);

        let mut history_entry = object([
            (
                "createdAt",
                entry
                    .get("createdAt")
                    .cloned()
                    .unwrap_or(JsValue::Undefined),
            ),
            ("mode", mode),
            ("merge", merge),
        ]);
        if let Some(reason) = reason {
            history_entry.insert("reason".into(), JsValue::String(reason));
        }
        if let Some(markdown) = markdown {
            history_entry.insert("markdown".into(), markdown.clone());
        }
        history_entry.insert("todos".into(), JsValue::Array(next.clone()));
        history_entry.insert("diagnostics".into(), JsValue::Object(diagnostics.clone()));
        let mut history = self.history;
        history.push(JsValue::Object(history_entry));
        Self {
            approved_plan: self.approved_plan,
            requires_initialization: false,
            tool_calls_since_last_todo_write: 0.0,
            last_markdown: markdown.cloned(),
            todos: next,
            history,
            diagnostics,
        }
    }

    fn apply_progress(mut self, tool: Option<&JsString>) -> Self {
        if self.approved_plan.is_none() || self.requires_initialization || is(tool, "todo_write") {
            return self;
        }
        self.tool_calls_since_last_todo_write += 1.0;
        // `withDiagnostics`: the five derived counters are recomputed in place.
        let active = self.todos.iter().filter(|todo| is_active(todo)).count();
        for (name, value) in [
            ("writeCount", self.history.len()),
            ("todoCount", self.todos.len()),
            ("activeCount", active),
            ("completedCount", count_status(&self.todos, "completed")),
            ("cancelledCount", count_status(&self.todos, "cancelled")),
        ] {
            self.diagnostics.insert(name.into(), number(value));
        }
        self
    }
}

/// `NativePlanTodoRuntime`: one session's plan/todo provider over its
/// transcript writer and live entry list.
pub struct PlanTodoRuntime {
    session: JsString,
    recorder: Arc<Recorder>,
    projection: Arc<SessionProjection>,
}
impl PlanTodoRuntime {
    pub fn new(
        session: JsString,
        recorder: Arc<Recorder>,
        projection: Arc<SessionProjection>,
    ) -> Arc<Self> {
        Arc::new(Self {
            session,
            recorder,
            projection,
        })
    }

    pub fn snapshot(&self) -> PlanTodoSnapshot {
        let mut state = Some(PlanTodoSnapshot::new());
        self.projection.for_each_entry(|entry| {
            state = state.take().map(|state| state.reduce(entry));
        });
        state.unwrap_or_default()
    }

    async fn append(&self, turn: &JsString, draft: JsObject) -> Result<(), HostError> {
        // `requireTurnId`.
        if trim(turn).is_empty() {
            return Err(ModuleError::new("Plan/todo mutation requires a turn id.").into());
        }
        let recorder = self.recorder.clone();
        let (session, turn) = (self.session.clone(), turn.clone());
        tokio::task::spawn_blocking(move || {
            recorder
                .writer()
                .record_session_event(&session, &turn, draft)
        })
        .await
        .map_err(|error| ModuleError::new(error.to_string()))?
        .map_err(|error| ModuleError::new(error.to_string()).into())
    }

    async fn record_write(
        &self,
        turn: &JsString,
        todos: &[JsValue],
        mode: &str,
        merge: bool,
        markdown: Option<&JsString>,
        reason: Option<&JsString>,
    ) -> Result<(), HostError> {
        let mut draft = object([
            ("type", "plan_todo_written".into()),
            ("mode", mode.into()),
            ("merge", JsValue::Bool(merge)),
        ]);
        if let Some(markdown) = markdown {
            draft.insert("markdown".into(), JsValue::String(markdown.clone()));
        }
        if let Some(reason) = reason.map(trim).filter(|reason| !reason.is_empty()) {
            draft.insert("reason".into(), JsValue::String(reason));
        }
        draft.insert("todos".into(), JsValue::Array(todos.to_vec()));
        self.append(turn, draft).await
    }

    /// `markPlanApproved`.
    pub async fn mark_plan_approved(
        &self,
        plan: &JsString,
        turn: &JsString,
    ) -> Result<(), HostError> {
        let plan = trim(plan);
        self.append(
            turn,
            object([
                ("type", "plan_todo_plan_changed".into()),
                (
                    "plan",
                    if plan.is_empty() {
                        JsValue::Null
                    } else {
                        JsValue::String(plan)
                    },
                ),
            ]),
        )
        .await
    }

    /// `recordTodoWrite`: a markdown checklist replaces the list.
    pub async fn record_todo_write(
        &self,
        markdown: &JsString,
        todos: &[JsValue],
        turn: &JsString,
        reason: Option<&JsString>,
    ) -> Result<Vec<JsValue>, HostError> {
        let normalized = replace_todos(todos);
        self.record_write(turn, &normalized, "markdown", false, Some(markdown), reason)
            .await?;
        Ok(normalized)
    }

    /// `writeTodos`: structured updates replace the list or merge into it.
    pub async fn write_todos(
        &self,
        todos: &[JsValue],
        turn: &JsString,
        merge: bool,
        markdown: Option<&JsString>,
        reason: Option<&JsString>,
    ) -> Result<Vec<JsValue>, HostError> {
        let next = if merge {
            merge_todos(&self.snapshot().todos, todos)
        } else {
            replace_todos(todos)
        };
        self.record_write(turn, &next, "structured", merge, markdown, reason)
            .await?;
        Ok(next)
    }

    /// `markToolProgressChanged`.
    pub async fn mark_tool_progress_changed(
        &self,
        tool: &JsString,
        turn: &JsString,
    ) -> Result<(), HostError> {
        let state = self.snapshot();
        if state.approved_plan.is_none()
            || state.requires_initialization
            || tool.equals_str("todo_write")
        {
            return Ok(());
        }
        self.append(
            turn,
            object([
                ("type", "plan_todo_progressed".into()),
                ("toolName", JsValue::String(tool.clone())),
            ]),
        )
        .await
    }
}

/// `createPlanFileManager({ projectRoot, storage })` as the session composes
/// it (`SessionPlanTodoBundle.ts:37`): plans live in
/// `<projectRoot>/.pilotdeck/plans`, created the first time the directory is
/// asked for — which every tool context does.
pub struct SessionPlanFiles {
    directory: JsString,
    ensured: std::sync::Once,
}
impl SessionPlanFiles {
    pub fn new(project_root: &std::path::Path) -> Self {
        Self {
            directory: Self::directory_for(project_root),
            ensured: std::sync::Once::new(),
        }
    }
    /// The plan directory path, without creating it.
    pub fn directory_for(project_root: &std::path::Path) -> JsString {
        project_root
            .join(".pilotdeck")
            .join("plans")
            .to_string_lossy()
            .into_owned()
            .into()
    }
    fn ensure(&self) -> JsString {
        self.ensured.call_once(|| {
            let _ = std::fs::create_dir_all(String::from_utf16_lossy(self.directory.units()));
        });
        self.directory.clone()
    }
    /// `resolvePlanFilePath`: an `.md` file strictly inside the plan directory.
    fn resolve_path(&self, file_path: &JsString, cwd: &JsString) -> Option<JsString> {
        use crate::runtime::tools::builtin::{is_absolute, join, normalize, relative};
        if trim(file_path).is_empty() {
            return None;
        }
        let directory = self.ensure();
        let absolute = normalize(&if is_absolute(file_path) {
            file_path.clone()
        } else {
            join(cwd, file_path)
        });
        let inside = String::from_utf16_lossy(relative(&directory, &absolute).units());
        if inside.is_empty() || inside.starts_with("..") || inside.starts_with('/') {
            return None;
        }
        String::from_utf16_lossy(absolute.units())
            .to_lowercase()
            .ends_with(".md")
            .then_some(absolute)
    }
}
impl crate::host::modules::tool_context::PlanFiles for SessionPlanFiles {
    fn directory(&self) -> Option<JsString> {
        Some(self.ensure())
    }
    fn resolve(&self, file_path: JsString, cwd: JsString) -> HostFuture<JsValue> {
        let resolved = self.resolve_path(&file_path, &cwd);
        Box::pin(async move { Ok(resolved.map_or(JsValue::Undefined, JsValue::String)) })
    }
    fn read(&self, file_path: JsString, cwd: JsString) -> HostFuture<JsValue> {
        let resolved = self.resolve_path(&file_path, &cwd);
        Box::pin(async move {
            let Some(path) = resolved else {
                return Ok(JsValue::Undefined);
            };
            let path = String::from_utf16_lossy(path.units());
            // `content.trim() || undefined`; an unreadable file is absent.
            Ok(tokio::fs::read_to_string(path)
                .await
                .ok()
                .map(|content| JsString::from(content.as_str()))
                .map(|content| trim(&content))
                .filter(|content| !content.is_empty())
                .map_or(JsValue::Undefined, JsValue::String))
        })
    }
}

/// `planTodoManager.forSession`: the handle tools receive as
/// `context.planTodo`.
pub struct PlanTodoSessions(pub Arc<PlanTodoRuntime>);
impl SessionService for PlanTodoSessions {
    fn for_session(&self, session_id: &JsString) -> Option<RuntimeContext> {
        (session_id == &self.0.session).then(|| self.0.clone() as RuntimeContext)
    }
}

/// `createHostPlanTodoModuleHandler`: the loop's `capability.plan_todo`
/// operation, bound to one host turn.
pub struct PlanTodoTurnHandler {
    pub runtime: Arc<PlanTodoRuntime>,
    pub turn: JsString,
}
impl PlanTodoHandler for PlanTodoTurnHandler {
    fn call(&self, call: JsValue) -> HostFuture<JsObject> {
        let runtime = self.runtime.clone();
        let active_turn = self.turn.clone();
        Box::pin(async move {
            let missing = JsValue::Undefined;
            let payload = call.get("payload").unwrap_or(&missing);
            let required = |field: &str| -> Result<JsString, HostError> {
                text(payload, field)
                    .filter(|value| !value.is_empty())
                    .cloned()
                    .ok_or_else(|| ModuleError::new(format!("Missing {field}.").as_str()).into())
            };
            let session = required("sessionId")?;
            let turn = required("turnId")?;
            if session != runtime.session || turn != active_turn {
                return Err(ModuleError::new(
                    "Plan/todo capability identity does not match the active host turn.",
                )
                .into());
            }
            let method = required("method")?;
            let reason = text(payload, "reason");
            if method.equals_str("read") {
            } else if method.equals_str("mark_plan_approved") {
                runtime
                    .mark_plan_approved(&required("plan")?, &turn)
                    .await?;
            } else if method.equals_str("record_todo_write") {
                let markdown = required("markdown")?;
                let todos = wire_todos(payload.get("todos"), true)?;
                runtime
                    .record_todo_write(&markdown, &todos, &turn, reason)
                    .await?;
            } else if method.equals_str("write_todos") {
                let todos = wire_todos(payload.get("todos"), false)?;
                runtime
                    .write_todos(
                        &todos,
                        &turn,
                        payload.get("merge") == Some(&JsValue::Bool(true)),
                        text(payload, "markdown"),
                        reason,
                    )
                    .await?;
            } else if method.equals_str("mark_tool_progress") {
                runtime
                    .mark_tool_progress_changed(&required("toolName")?, &turn)
                    .await?;
            } else {
                return Err(ModuleError::new(JsString::from_utf16(
                    "Unsupported sidecar plan/todo method: "
                        .encode_utf16()
                        .chain(method.units().iter().copied())
                        .collect(),
                ))
                .into());
            }
            Ok(object([("snapshot", runtime.snapshot().to_value())]))
        })
    }
}

/// `todoItems` (`items`: content and a valid status are required) and
/// `todoUpdates` of the wire handler: only non-empty strings are carried over.
fn wire_todos(value: Option<&JsValue>, items: bool) -> Result<Vec<JsValue>, HostError> {
    let fail = |message: &str| -> HostError { ModuleError::new(message).into() };
    let Some(JsValue::Array(entries)) = value else {
        return Err(fail(if items {
            "Plan/todo items must be an array."
        } else {
            "Plan/todo updates must be an array."
        }));
    };
    entries
        .iter()
        .map(|entry| {
            if !matches!(entry, JsValue::Object(_)) {
                return Err(fail("Plan/todo entry must be an object."));
            }
            let nonempty = |field: &str| text(entry, field).filter(|value| !value.is_empty());
            let status = entry.get("status").filter(|value| !value.is_undefined());
            let status_ok = status.is_some_and(|s| valid_status(s.as_string()));
            let mut todo = JsObject::new();
            if items {
                let Some(content) = nonempty("content").filter(|_| status_ok) else {
                    return Err(fail("Plan/todo item is invalid."));
                };
                todo.insert("content".into(), JsValue::String(content.clone()));
                todo.insert(
                    "status".into(),
                    status.cloned().unwrap_or(JsValue::Undefined),
                );
                for field in ["id", "priority"] {
                    if let Some(value) = nonempty(field) {
                        todo.insert(field.into(), JsValue::String(value.clone()));
                    }
                }
            } else {
                if status.is_some() && !status_ok {
                    return Err(fail("Plan/todo update status is invalid."));
                }
                for field in ["id", "content"] {
                    if let Some(value) = nonempty(field) {
                        todo.insert(field.into(), JsValue::String(value.clone()));
                    }
                }
                if let Some(status) = status {
                    todo.insert("status".into(), status.clone());
                }
                if let Some(value) = nonempty("priority") {
                    todo.insert("priority".into(), JsValue::String(value.clone()));
                }
            }
            Ok(JsValue::Object(todo))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilotdeck_protocol::{parse_json, stringify_json};

    fn todos(json: &str) -> Vec<JsValue> {
        match &parse_json(json).unwrap() {
            JsValue::Array(items) => items.clone(),
            _ => panic!("array expected"),
        }
    }
    fn json(values: &[JsValue]) -> String {
        stringify_json(&JsValue::Array(values.to_vec()))
            .unwrap()
            .unwrap()
    }

    // Expected values are the baseline's (`replacePlanTodos` / `mergePlanTodos`
    // run under Node 22 on the same inputs).
    #[test]
    fn replace_keeps_the_last_duplicate_and_numbers_blank_ids_by_position() {
        let next = replace_todos(&todos(
            r#"[{"id":"a","content":"one"},{"content":" two ","status":"completed"},{"id":" a ","content":"three","priority":" high "},{"id":"b","status":"bogus"}]"#,
        ));
        assert_eq!(
            json(&next),
            r#"[{"id":"todo-1","content":"two","status":"completed"},{"id":"a","content":"three","status":"pending","priority":"high"},{"id":"b","content":"(no description)","status":"pending"}]"#
        );
    }

    #[test]
    fn merge_updates_known_ids_in_place_and_appends_the_rest() {
        let existing = todos(
            r#"[{"id":"a","content":"one","status":"in_progress"},{"id":"b","content":"two","status":"pending"}]"#,
        );
        let next = merge_todos(
            &existing,
            &todos(
                r#"[{"id":"b","status":"completed","priority":"low"},{"content":"new"},{"id":"a","content":" "}]"#,
            ),
        );
        assert_eq!(
            json(&next),
            r#"[{"id":"a","content":"one","status":"in_progress"},{"id":"b","content":"two","status":"completed","priority":"low"},{"id":"todo-3","content":"new","status":"pending"}]"#
        );
    }

    #[test]
    fn reducer_counts_rewrites_and_progress() {
        let write = |todos: &str| {
            parse_json(&format!(
                r#"{{"type":"plan_todo_written","mode":"structured","merge":false,"createdAt":"t","todos":{todos}}}"#
            ))
            .unwrap()
        };
        let state = PlanTodoSnapshot::new().reduce(
            &parse_json(r#"{"type":"plan_todo_plan_changed","plan":" the plan "}"#).unwrap(),
        );
        assert!(state.requires_initialization);
        let state = state
            .reduce(&write(
                r#"[{"id":"a","content":"x","status":"pending"},{"id":"b","content":"y","status":"in_progress"}]"#,
            ))
            .reduce(&parse_json(r#"{"type":"plan_todo_progressed","toolName":"bash"}"#).unwrap())
            .reduce(&parse_json(r#"{"type":"plan_todo_progressed","toolName":"todo_write"}"#).unwrap());
        assert_eq!(state.tool_calls_since_last_todo_write, 1.0);
        let state = state.reduce(&write(
            r#"[{"id":"z","content":"only","status":"completed"}]"#,
        ));
        assert_eq!(
            stringify_json(&JsValue::Object(state.diagnostics.clone()))
                .unwrap()
                .unwrap(),
            r#"{"writeCount":2,"todoCount":1,"activeCount":0,"completedCount":1,"cancelledCount":0,"largeRewriteCount":1,"deletedOpenItemCount":2,"completedWithoutActiveCount":1,"lastWrite":{"mode":"structured","merge":false,"addedCount":1,"removedCount":2,"changedCount":0,"deletedOpenItemCount":2,"largeRewrite":true,"allCompleted":true}}"#
        );
        assert_eq!(state.approved_plan, Some("the plan".into()));
        assert_eq!(state.tool_calls_since_last_todo_write, 0.0);
    }
}
