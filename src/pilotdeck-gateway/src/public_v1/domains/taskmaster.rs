//! `taskmaster.*` — the seven v1 taskmaster operations (t019 T3).
//!
//! TS baseline (`ui/server/routes/taskmaster.js`, cc7c4e6): every taskmaster
//! surface went through the external `task-master`/`task-master-ai` CLI
//! (`npx task-master init`, `add-task`, `set-status`, `update-task`,
//! `parse-prd --research`) plus direct `fs` access to
//! `<projectPath>/.taskmaster/{docs,tasks}`. The old Rust line has no
//! counterpart at all. This domain is the v1 re-implementation of that
//! surface against the same on-disk layout, with one registered deviation
//! (task report): **no external task-master binary is required or spawned**.
//! The document structure, task set and parsing are implemented in-process,
//! because the v1 product must not depend on `npx`/`task-master` being on
//! PATH (the baseline's `shell:true` + unconditional `yes\n` stdin were its
//! two acknowledged hazards; the v1 process boundary is "no subprocess at
//! all"). Consequences recorded in the report:
//!
//! * `initialize` self-builds `.taskmaster/` instead of running
//!   `npx task-master init`, so the contract's "缺依赖明确 UNAVAILABLE"
//!   acceptance is vacuously satisfied (there is no dependency to miss).
//! * `parse_document` / `write_task` `createGenerated` run a deterministic
//!   in-process parser/generator instead of an AI-research subprocess, and
//!   execute inline (the `commands.execute` pattern: durable before the
//!   receipt is answered) — there is no cancellation window because there is
//!   no in-flight external work.
//! * `tasks.json` keeps the baseline's task-master-compatible shapes
//!   (numeric `id`/`dependencies`, `status` in pending / in-progress / done /
//!   review / deferred / cancelled), read through the same tolerant
//!   legacy/tagged/simple decoding the TS `GET /tasks` route used.
//!
//! Storage is entirely file-based under the project's resolved workspace
//! (`domains::files::resolve_project`): PRD documents are
//! `.taskmaster/docs/<documentId>.{txt,md}` (the TS filename contract
//! `^[\w\-. ]+\.(txt|md)$`), the task set is `.taskmaster/tasks/tasks.json`.
//! Revisions are content-addressed (`frev-…` documents reuse the files
//! domain's file identity, `trev-…` per task, `tset-…` per task set), so the
//! conditional-write gates (`expectedRevision`) never need a counter. The
//! only SQLite this domain needs is the shared async-operation ledger
//! (`v1_domain_operations`, owned by `schedules.rs`) plus a small
//! parse-binding table that backs `remove_document`'s RESOURCE_BUSY gate.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_REVISION_CONFLICT, CODE_UNAVAILABLE, ContractError,
    EffectStatus, RetrySpec,
};
use agentcpm_pd_protocol::schema_validator::validate_against_schema;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::files::{
    ProjectResolveError, ProjectWorkspace, file_revision_of, resolve_project,
};
use crate::public_v1::domains::schedules::{
    DDL_DOMAIN_OPERATIONS, DomainDb, accept_domain_operation, finish_domain_operation,
    mark_domain_operation_running,
};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::now_rfc3339;

/// Contract error codes these operations declare that the protocol crate has
/// no shared constant for yet (same convention as commands.rs).
const CODE_PATH_OUTSIDE_SCOPE: &str = "PATH_OUTSIDE_SCOPE";
const CODE_DEPENDENCY_CONFLICT: &str = "DEPENDENCY_CONFLICT";
const CODE_ALREADY_INITIALIZED: &str = "ALREADY_INITIALIZED";
const CODE_IO_ERROR: &str = "IO_ERROR";
const CODE_RESOURCE_BUSY: &str = "RESOURCE_BUSY";

/// The registered, version-fixed initialization configuration schema
/// (`taskmaster.initialize` input `schemaId`). The TS `/initialize`
/// placeholder accepted `rules` (rule profiles) and nothing else; that is
/// the v1 shape. Unknown fields reject (additionalProperties: false).
const INIT_SCHEMA_ID: &str = "agentcpm-pd.taskmaster.init/1";
const INIT_SCHEMA: &str = r#"{"type":"object","properties":{"rules":{"type":"array","items":{"type":"string"},"description":"Rule profile names applied to the initialized taskmaster."}},"additionalProperties":false}"#;

/// The taskmaster layout version this domain writes; reported as
/// `initialize`'s completion `version` (the baseline reported the external
/// CLI's `--version`; the self-built structure reports its own).
const STRUCTURE_VERSION: &str = "agentcpm-pd-taskmaster-1";

/// The task statuses the baseline's task-master set used (tasks.js and the
/// `set-status` CLI accepted exactly these).
const KNOWN_STATUSES: [&str; 6] = [
    "pending",
    "in-progress",
    "done",
    "review",
    "deferred",
    "cancelled",
];

/// The priorities `add-task --priority` accepted.
const KNOWN_PRIORITIES: [&str; 3] = ["high", "medium", "low"];

/// Active (non-terminal) parse bindings block document removal
/// (`remove_document`'s RESOURCE_BUSY gate).
const DDL_PARSE_BINDINGS: &str = "CREATE TABLE IF NOT EXISTS taskmaster_parse_bindings (
    operation_id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    document_id TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);";

/// One built-in PRD template (`GET /api/taskmaster/prd-templates`). The ids,
/// names, descriptions and categories match the baseline's template catalog
/// (they are the stable templateId contract); the skeleton text is written
/// for v1 with the same section structure and `[Placeholder]` fields the
/// baseline exposed (the baseline interpolated the listing date at request
/// time; v1 keeps a `[Date]` placeholder that `templateVariables` fills).
struct TemplateDef {
    id: &'static str,
    name: &'static str,
    #[allow(dead_code)]
    description: &'static str,
    #[allow(dead_code)]
    category: &'static str,
    content: &'static str,
}

const TEMPLATES: [TemplateDef; 4] = [
    TemplateDef {
        id: "web-app",
        name: "Web Application",
        description: "Template for web application projects with frontend and backend components",
        category: "web",
        content: include_str!("taskmaster_templates/web-app.md"),
    },
    TemplateDef {
        id: "api",
        name: "REST API",
        description: "Template for REST API development projects",
        category: "backend",
        content: include_str!("taskmaster_templates/api.md"),
    },
    TemplateDef {
        id: "mobile-app",
        name: "Mobile Application",
        description: "Template for mobile app development projects (iOS/Android)",
        category: "mobile",
        content: include_str!("taskmaster_templates/mobile-app.md"),
    },
    TemplateDef {
        id: "data-analysis",
        name: "Data Analysis Project",
        description: "Template for data analysis and visualization projects",
        category: "data",
        content: include_str!("taskmaster_templates/data-analysis.md"),
    },
];

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("taskmaster_parse_bindings", DDL_PARSE_BINDINGS);
    reg.handler("taskmaster.initialize", |rt, input, key, _rev| {
        Box::pin(taskmaster_initialize(rt, input, key))
    });
    reg.handler("taskmaster.read_documents", |rt, input, _key, _rev| {
        Box::pin(taskmaster_read_documents(rt, input))
    });
    reg.handler("taskmaster.write_document", |rt, input, _key, _rev| {
        Box::pin(taskmaster_write_document(rt, input))
    });
    reg.handler("taskmaster.remove_document", |rt, input, _key, _rev| {
        Box::pin(taskmaster_remove_document(rt, input))
    });
    reg.handler("taskmaster.parse_document", |rt, input, key, _rev| {
        Box::pin(taskmaster_parse_document(rt, input, key))
    });
    reg.handler("taskmaster.list_tasks", |rt, input, _key, _rev| {
        Box::pin(taskmaster_list_tasks(rt, input))
    });
    reg.handler("taskmaster.write_task", |rt, input, key, _rev| {
        Box::pin(taskmaster_write_task(rt, input, key))
    });
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

fn invalid_arg(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn not_found(kind: &str, id: &str) -> ContractError {
    ContractError::new(
        CODE_NOT_FOUND,
        format!("{kind} `{id}` not found"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn revision_conflict(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_REVISION_CONFLICT,
        message,
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

fn path_outside_scope(document_id: &str) -> ContractError {
    ContractError::new(
        CODE_PATH_OUTSIDE_SCOPE,
        format!(
            "documentId `{document_id}` names a path; PRD documents are addressed by their \
             file name inside the project's `.taskmaster/docs` directory, never by absolute \
             paths or parent traversal"
        ),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn dependency_conflict(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_DEPENDENCY_CONFLICT,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn io_error(context: &str, error: &std::io::Error) -> ContractError {
    ContractError::new(
        CODE_IO_ERROR,
        format!("{context}: {error}"),
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

fn unavailable(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        message,
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::NotApplied,
    )
}

fn sql_err(context: &str, error: rusqlite::Error) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("{context}: {error}"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn resource_busy(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_RESOURCE_BUSY,
        message,
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

// ---------------------------------------------------------------------------
// Shared plumbing: the per-dataRoot domain db, workspace resolution, events
// ---------------------------------------------------------------------------

/// The per-dataRoot singleton for this domain's SQLite needs (the shared
/// async-operation ledger plus the parse-binding table), mirroring
/// `commands.rs`'s registry so repeated dispatches do not reopen the db.
fn domain_db(rt: &Arc<PublicV1Runtime>) -> Result<Arc<DomainDb>, ContractError> {
    fn registry() -> &'static Mutex<HashMap<PathBuf, Weak<DomainDb>>> {
        static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<DomainDb>>>> = OnceLock::new();
        REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
    }
    let home = rt.store().home().to_path_buf();
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(db) = reg.get(&home).and_then(Weak::upgrade) {
        return Ok(db);
    }
    let db = Arc::new(DomainDb::open(
        &home,
        &[DDL_DOMAIN_OPERATIONS, DDL_PARSE_BINDINGS],
    )?);
    reg.insert(home, Arc::downgrade(&db));
    Ok(db)
}

/// Runs one read/write statement against the shared state db through the
/// store's connection (`DomainDb`'s own with_conn is private to
/// schedules.rs; the store connection is the same file with the same WAL
/// discipline).
fn with_store_conn<T>(
    rt: &Arc<PublicV1Runtime>,
    body: impl FnOnce(&Connection) -> Result<T, ContractError>,
) -> Result<T, ContractError> {
    let conn = rt.store().conn.lock().expect("sqlite mutex");
    body(&conn)
}

/// Resolves the project to a real workspace. A project that is not registered
/// answers NOT_FOUND; a virtual project (no host-declared root) has no file
/// workspace, so it answers NOT_FOUND too — the instance home is never used
/// as a project root.
fn workspace_for(
    rt: &Arc<PublicV1Runtime>,
    project_id: &str,
) -> Result<ProjectWorkspace, ContractError> {
    match resolve_project(rt.store(), project_id) {
        Ok(workspace) => Ok(workspace),
        Err(ProjectResolveError::NotFound(message))
        | Err(ProjectResolveError::Virtual(message)) => Err(ContractError::new(
            CODE_NOT_FOUND,
            format!("project `{project_id}` not found: {message}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )),
        Err(ProjectResolveError::Contract(error)) => Err(error),
    }
}

fn taskmaster_dir(project: &ProjectWorkspace) -> PathBuf {
    project.workspace.join(".taskmaster")
}

fn docs_dir(project: &ProjectWorkspace) -> PathBuf {
    taskmaster_dir(project).join("docs")
}

fn tasks_file(project: &ProjectWorkspace) -> PathBuf {
    taskmaster_dir(project).join("tasks").join("tasks.json")
}

fn config_file(project: &ProjectWorkspace) -> PathBuf {
    taskmaster_dir(project).join("config.json")
}

/// Appends one domain event into the shared `events` table (the same
/// placeholder→`cur-<seq>` pattern the core store uses) and wakes the event
/// notifier so `operations.events` style consumers see it.
fn append_domain_event(
    rt: &Arc<PublicV1Runtime>,
    resource_id: &str,
    event_type: &str,
    payload: &Value,
) -> Result<(), ContractError> {
    with_store_conn(rt, |conn| {
        let placeholder = format!(
            "tmp-tm-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        conn.execute(
            "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at)
             VALUES (?1, ?2, '', ?3, 'agentcpm-pd.event/1', ?4, ?4, ?5)",
            params![
                placeholder,
                resource_id,
                event_type,
                serde_json::to_string(payload).unwrap_or_else(|_| "{}".into()),
                now_rfc3339(),
            ],
        )
        .map_err(|e| sql_err("append taskmaster event", e))?;
        let seq = conn.last_insert_rowid();
        conn.execute(
            "UPDATE events SET cursor = ?1 WHERE seq = ?2",
            params![format!("cur-{seq:012}"), seq],
        )
        .map_err(|e| sql_err("finalize taskmaster event cursor", e))?;
        Ok(())
    })?;
    rt.store().event_notifier().notify_waiters();
    Ok(())
}

/// The event resource identity of one project's taskmaster surface.
fn event_resource(project_id: &str) -> String {
    format!("taskmaster:{project_id}")
}

// ---------------------------------------------------------------------------
// Document ids: the TS filename contract, plus the v1 path-scope gate
// ---------------------------------------------------------------------------

/// A valid PRD document id: one path component matching the baseline's
/// `^[\w\-. ]+\.(txt|md)$` (ASCII word characters, dots, dashes and spaces,
/// ending in `.txt` or `.md`). Anything that names a path (separators,
/// absolute prefixes, `.`/`..`) answers PATH_OUTSIDE_SCOPE instead — the
/// contract's own error for out-of-scope file names.
fn validate_document_id(document_id: &str) -> Result<(), ContractError> {
    let names_a_path = document_id.is_empty()
        || document_id.len() > 255
        || document_id.contains(['/', '\\', '\0'])
        || matches!(document_id, "." | "..")
        || Path::new(document_id).components().count() != 1;
    if names_a_path {
        return Err(path_outside_scope(document_id));
    }
    let stem = document_id
        .strip_suffix(".txt")
        .or_else(|| document_id.strip_suffix(".md"))
        .unwrap_or_default();
    let stem_ok = !stem.is_empty()
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ' '));
    if !stem_ok {
        return Err(invalid_arg(format!(
            "documentId `{document_id}` is not a valid PRD file name; it must match \
             `[word characters, dots, dashes, spaces].(txt|md)` (the baseline's PRD file contract)"
        )));
    }
    Ok(())
}

/// A document's content revision: the files domain's file identity
/// (`frev-…`, sha256 over content and length).
fn document_revision(content: &[u8]) -> String {
    let content_sha = {
        let mut hasher = Sha256::new();
        hasher.update(content);
        format!("{:x}", hasher.finalize())
    };
    file_revision_of(&content_sha, content.len() as u64)
}

fn read_document(
    project: &ProjectWorkspace,
    document_id: &str,
) -> Result<Option<Vec<u8>>, ContractError> {
    let path = docs_dir(project).join(document_id);
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error(&format!("read {}", path.display()), &error)),
    }
}

/// Writes `bytes` to `path` atomically (temp file + rename), creating parent
/// directories as needed.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ContractError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| io_error(&format!("mkdir {}", parent.display()), &e))?;
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = path.with_file_name(format!("{name}.tmp"));
    std::fs::write(&temp, bytes).map_err(|e| io_error(&format!("write {}", temp.display()), &e))?;
    std::fs::rename(&temp, path).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        io_error(&format!("rename {}", path.display()), &e)
    })?;
    Ok(())
}

fn sha256_of(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// Templates
// ---------------------------------------------------------------------------

fn template_by_id(template_id: &str) -> Option<&'static TemplateDef> {
    TEMPLATES.iter().find(|tpl| tpl.id == template_id)
}

fn template_revision(template: &TemplateDef) -> String {
    format!("tpl-{}", &sha256_of(template.content.as_bytes())[..24])
}

/// Applies `templateVariables` to a template's `[Name]` placeholders (the
/// baseline `apply-template` substitution). A variable naming a placeholder
/// the template does not carry is rejected up front — the contract's
/// "模板变量校验".
fn apply_template_variables(
    template: &TemplateDef,
    variables: &[Value],
) -> Result<String, ContractError> {
    let mut content = template.content.to_owned();
    for variable in variables {
        let name = variable.get("name").and_then(Value::as_str).unwrap_or("");
        let value = variable.get("value").and_then(Value::as_str).unwrap_or("");
        if name.is_empty() {
            return Err(invalid_arg(
                "templateVariables entries must carry a non-empty `name`",
            ));
        }
        let placeholder = format!("[{name}]");
        if !content.contains(&placeholder) {
            return Err(invalid_arg(format!(
                "template `{}` has no placeholder `[{name}]`",
                template.id
            )));
        }
        content = content.replace(&placeholder, value);
    }
    Ok(content)
}

// ---------------------------------------------------------------------------
// Task set (tasks.json): task-master-compatible shapes, tolerant reads
// ---------------------------------------------------------------------------

/// The decoded task set: the on-disk task objects and the tag they live
/// under (writes go back under the same tag).
struct TaskSet {
    tag: String,
    tasks: Vec<Value>,
}

/// Reads the project's task set, decoding the baseline's three shapes
/// (legacy array, simple `{tasks}`, tagged `{master: {tasks}}` — TS
/// `GET /tasks/:project` logic). `None` when no task set exists (the
/// project was never initialized).
fn read_task_set(project: &ProjectWorkspace) -> Result<Option<TaskSet>, ContractError> {
    let path = tasks_file(project);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(&format!("read {}", path.display()), &error)),
    };
    let parsed: Value = serde_json::from_slice(&bytes).map_err(|error| {
        unavailable(format!(
            "task set {} is not valid JSON: {error}",
            path.display()
        ))
    })?;
    Ok(Some(decode_task_set(parsed)))
}

fn decode_task_set(parsed: Value) -> TaskSet {
    // Legacy array shape.
    if let Value::Array(tasks) = parsed {
        return TaskSet {
            tag: "master".to_owned(),
            tasks,
        };
    }
    if let Value::Object(ref obj) = parsed {
        // Simple shape: {tasks: [...]}.
        if let Some(Value::Array(tasks)) = obj.get("tasks") {
            return TaskSet {
                tag: "master".to_owned(),
                tasks: tasks.clone(),
            };
        }
        // Tagged shape: prefer the `master` tag, else the first tag with a
        // tasks array.
        if let Some(tasks) = obj
            .get("master")
            .and_then(|tag| tag.get("tasks"))
            .and_then(Value::as_array)
        {
            return TaskSet {
                tag: "master".to_owned(),
                tasks: tasks.clone(),
            };
        }
        if let Some((tag, tasks)) = obj
            .iter()
            .find(|(_, value)| value.get("tasks").is_some_and(Value::is_array))
            .and_then(|(tag, value)| {
                value
                    .get("tasks")
                    .and_then(Value::as_array)
                    .map(|tasks| (tag.clone(), tasks.clone()))
            })
        {
            return TaskSet { tag, tasks };
        }
    }
    TaskSet {
        tag: "master".to_owned(),
        tasks: Vec::new(),
    }
}

/// Serializes the task set back to disk bytes under its tag. The tag object
/// keeps the metadata block this domain writes (a real task-master's
/// metadata under the same tag is replaced wholesale — recorded in the task
/// report: no baseline consumer of that metadata exists in v1).
fn encode_task_set(set: &TaskSet) -> Vec<u8> {
    let mut tag_object = Map::new();
    tag_object.insert(
        "metadata".to_owned(),
        json!({"description": "Task set maintained by the agentcpm-pd taskmaster domain"}),
    );
    tag_object.insert("tasks".to_owned(), Value::Array(set.tasks.clone()));
    let mut file = Map::new();
    file.insert(set.tag.clone(), Value::Object(tag_object));
    // serde_json's default map is key-sorted, so the serialization is
    // deterministic per content.
    let mut text =
        serde_json::to_string_pretty(&Value::Object(file)).unwrap_or_else(|_| "{}".into());
    text.push('\n');
    text.into_bytes()
}

/// The task set revision: content identity of the exact file bytes.
fn task_set_revision_bytes(bytes: &[u8]) -> String {
    format!("tset-{}", &sha256_of(bytes)[..24])
}

fn task_set_revision(set: &TaskSet) -> String {
    task_set_revision_bytes(&encode_task_set(set))
}

fn write_task_set(project: &ProjectWorkspace, set: &TaskSet) -> Result<(), ContractError> {
    write_atomic(&tasks_file(project), &encode_task_set(set))
}

/// A task's on-disk id as the v1 string taskId.
fn task_id_string(task: &Value) -> String {
    match task.get("id") {
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::String(text)) => text.clone(),
        _ => String::new(),
    }
}

fn task_dep_strings(task: &Value) -> Vec<String> {
    task.get("dependencies")
        .and_then(Value::as_array)
        .map(|deps| {
            deps.iter()
                .map(|dep| match dep {
                    Value::Number(number) => number.to_string(),
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn task_str_or(task: &Value, field: &str, fallback: &str) -> String {
    task.get(field)
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_owned()
}

/// A task's semantic revision: content identity of the fields the v1 task
/// item exposes (the TS transform's fields, minus timestamps).
fn task_revision(task: &Value) -> String {
    let projection = json!({
        "id": task.get("id").cloned().unwrap_or(Value::Null),
        "title": task.get("title").cloned().unwrap_or(Value::Null),
        "description": task.get("description").cloned().unwrap_or(Value::Null),
        "status": task.get("status").cloned().unwrap_or(Value::Null),
        "priority": task.get("priority").cloned().unwrap_or(Value::Null),
        "dependencies": task.get("dependencies").cloned().unwrap_or(Value::Null),
        "details": task.get("details").cloned().unwrap_or(Value::Null),
    });
    let mut hasher = Sha256::new();
    hasher.update(b"task:");
    hasher.update(
        serde_json::to_string(&projection)
            .unwrap_or_default()
            .as_bytes(),
    );
    format!("trev-{:x}", hasher.finalize())
}

/// The v1 contract `Task` item projection of one on-disk task.
fn task_item(task: &Value) -> Value {
    json!({
        "taskId": task_id_string(task),
        "title": task_str_or(task, "title", "Untitled Task"),
        "description": task_str_or(task, "description", ""),
        "status": task_str_or(task, "status", "pending"),
        "priority": task_str_or(task, "priority", "medium"),
        "dependencyIds": task_dep_strings(task),
        "revision": task_revision(task),
    })
}

/// The next integer id for a new task (max existing id + 1, or 1).
fn next_task_id(tasks: &[Value]) -> i64 {
    tasks
        .iter()
        .filter_map(|task| task.get("id").and_then(Value::as_i64))
        .max()
        .unwrap_or(0)
        + 1
}

fn find_task<'a>(tasks: &'a [Value], task_id: &str) -> Option<&'a Value> {
    tasks.iter().find(|task| task_id_string(task) == task_id)
}

/// The dependency graph check: every dependency of the task being created
/// (the `extra` edge) must exist, and the resulting graph must stay acyclic
/// (task ids are unique, so a cycle needs existing edges — future input
/// shapes are guarded the same way).
fn validate_dependencies(
    tasks: &[Value],
    new_task_id: &str,
    deps: &[String],
) -> Result<(), ContractError> {
    let exists = |id: &str| find_task(tasks, id).is_some();
    if let Some(dep) = deps
        .iter()
        .find(|dep| !dep.is_empty() && *dep != new_task_id && !exists(dep))
    {
        return Err(dependency_conflict(format!(
            "task `{new_task_id}` depends on `{dep}`, which does not exist in the task set"
        )));
    }
    let mut edges: HashMap<String, Vec<String>> = HashMap::new();
    for task in tasks {
        edges.insert(task_id_string(task), task_dep_strings(task));
    }
    edges.insert(new_task_id.to_owned(), deps.to_vec());
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Visiting,
        Done,
    }
    fn reaches_cycle(
        node: &str,
        edges: &HashMap<String, Vec<String>>,
        marks: &mut HashMap<String, Mark>,
    ) -> bool {
        match marks.get(node) {
            Some(Mark::Done) => return false,
            Some(Mark::Visiting) => return true,
            None => {}
        }
        marks.insert(node.to_owned(), Mark::Visiting);
        let cyclic = edges
            .get(node)
            .is_some_and(|deps| deps.iter().any(|dep| reaches_cycle(dep, edges, marks)));
        marks.insert(node.to_owned(), Mark::Done);
        cyclic
    }
    let mut marks: HashMap<String, Mark> = HashMap::new();
    let nodes: Vec<String> = edges.keys().cloned().collect();
    for node in nodes {
        if reaches_cycle(&node, &edges, &mut marks) {
            return Err(dependency_conflict(
                "the dependency change would introduce a dependency cycle",
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// taskmaster.initialize
// ---------------------------------------------------------------------------

async fn taskmaster_initialize(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let project_id = input
        .get("projectId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let schema_id = input
        .get("schemaId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let configuration = input
        .get("configuration")
        .cloned()
        .unwrap_or_else(|| json!({}));

    // The registered, version-fixed schema check the contract demands:
    // unknown schemaId or unknown fields reject before anything is written.
    if schema_id != INIT_SCHEMA_ID {
        return Err(invalid_arg(format!(
            "schemaId `{schema_id}` is not a registered taskmaster initialization schema \
             (expected `{INIT_SCHEMA_ID}`)"
        ))
        .into());
    }
    let init_schema: Value =
        serde_json::from_str(INIT_SCHEMA).unwrap_or_else(|_| json!({"type": "object"}));
    if let Err(errors) = validate_against_schema(&configuration, &init_schema) {
        let details = errors
            .iter()
            .map(|error| json!({"path": error.path, "message": error.message}))
            .collect::<Vec<_>>();
        let mut error = invalid_arg(
            "configuration does not satisfy the registered taskmaster initialization schema",
        );
        error.details = Some(
            json!({"validationErrors": details})
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
        return Err(error.into());
    }

    let db = domain_db(&rt)?;
    let operation = accept_domain_operation(
        &db,
        "taskmaster.initialize",
        &input,
        key.as_deref(),
        || format!("op-{}", rt.store().next_id()),
        || {
            // The workspace resolves (registered project, real root); a
            // virtual project has no file surface to initialize. An
            // existing `.taskmaster` must never be re-initialized over
            // (the baseline's 400 "already initialized").
            let workspace = workspace_for(&rt, &project_id)?;
            if taskmaster_dir(&workspace).exists() {
                return Err(ContractError::new(
                    CODE_ALREADY_INITIALIZED,
                    format!(
                        "project `{project_id}` already has a `.taskmaster` directory; \
                         re-running initialize must not overwrite edited task files"
                    ),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            Ok(())
        },
    )?;
    let operation_id = operation.operation_id.clone();

    // A replayed idempotency key + identical input must not re-run the
    // initialization: the receipt is already durable, the structure exists.
    if operation.receipt["deduplicated"] == true {
        return Ok((ResponseOutcome::Accepted, operation.receipt));
    }

    // Inline execution (the `commands.execute` pattern): everything is
    // durable before the receipt is answered, so a dropped connection
    // neither cancels nor re-runs the initialization.
    let _ = mark_domain_operation_running(&db, &operation_id, "");

    let workspace = workspace_for(&rt, &project_id)?;
    let root = taskmaster_dir(&workspace);
    let mut created: Vec<String> = Vec::new();
    let mkdir = |dir: &Path| -> Result<(), ContractError> {
        std::fs::create_dir_all(dir).map_err(|e| io_error(&format!("mkdir {}", dir.display()), &e))
    };
    mkdir(&root)?;
    created.push(".taskmaster".to_owned());
    for sub in ["docs", "reports", "templates", "tasks"] {
        let dir = root.join(sub);
        mkdir(&dir)?;
        created.push(format!(".taskmaster/{sub}"));
    }

    // config.json records the accepted configuration and its schema.
    let config = json!({
        "structureVersion": 1,
        "schemaId": schema_id,
        "configuration": configuration,
    });
    let config_bytes = serde_json::to_vec_pretty(&config).unwrap_or_else(|_| b"{}".to_vec());
    write_atomic(&config_file(&workspace), &config_bytes)?;
    created.push(".taskmaster/config.json".to_owned());

    // tasks.json starts as an empty task set under the master tag.
    let empty_set = TaskSet {
        tag: "master".to_owned(),
        tasks: Vec::new(),
    };
    write_task_set(&workspace, &empty_set)?;
    created.push(".taskmaster/tasks/tasks.json".to_owned());

    let completion = json!({
        "projectId": project_id,
        "initialized": true,
        "version": STRUCTURE_VERSION,
        "createdRelativePaths": created,
        "configurationRevision": format!("crev-{}", &sha256_of(&config_bytes)[..24]),
    });
    let _ = append_domain_event(
        &rt,
        &event_resource(&project_id),
        "integration.initialized",
        &json!({"projectId": project_id, "operationId": operation_id}),
    );
    let _ = finish_domain_operation(&db, &operation_id, "", "succeeded", &completion);
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

// ---------------------------------------------------------------------------
// taskmaster.read_documents
// ---------------------------------------------------------------------------

async fn taskmaster_read_documents(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let kind = input
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let document_id = input
        .get("documentId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    let project_id = input.get("projectId").and_then(Value::as_str);
    let cursor = input.get("cursor").and_then(Value::as_str);
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 200) as usize;

    if kind == "template" {
        return read_templates(document_id, cursor, limit);
    }
    if kind != "prd" {
        return Err(invalid_arg(format!("kind must be `prd` or `template`, got `{kind}`")).into());
    }
    // Acceptance: "PRD必须projectId" — the PRD surface is always project
    // scoped (templates are the global surface).
    let Some(project_id) = project_id else {
        return Err(invalid_arg(
            "`projectId` is required when kind is `prd` (PRD documents are project-scoped)",
        )
        .into());
    };
    let workspace = workspace_for(&rt, project_id)?;

    // A named document: exactly that document, with its content.
    if let Some(document_id) = document_id {
        validate_document_id(document_id)?;
        let Some(bytes) = read_document(&workspace, document_id)? else {
            return Err(not_found("document", document_id).into());
        };
        let revision = document_revision(&bytes);
        return Ok((
            ResponseOutcome::Succeeded,
            json!({
                "items": [{
                    "documentId": document_id,
                    "name": document_id,
                    "revision": revision,
                    "content": String::from_utf8_lossy(&bytes).into_owned(),
                    "templateId": Value::Null,
                }],
                "nextCursor": Value::Null,
                "snapshotRevision": docs_snapshot(&workspace),
            }),
        ));
    }

    // The listing: every `.txt`/`.md` in the docs directory (content null),
    // name-ordered for a deterministic pagination (the TS route sorted by
    // mtime; v1 orders by name — recorded in the task report).
    let offset = parse_offset_cursor(cursor, "tdoc")?;
    let mut names: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(docs_dir(&workspace)) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.path().is_file() && (name.ends_with(".txt") || name.ends_with(".md")) {
                names.push(name);
            }
        }
    }
    names.sort();
    let snapshot_revision = docs_snapshot(&workspace);
    let mut items = Vec::new();
    let mut next_cursor = None;
    for (index, name) in names.iter().enumerate().skip(offset) {
        if items.len() >= limit {
            next_cursor = Some(format!("tdoc-{index}"));
            break;
        }
        let revision = read_document(&workspace, name)?
            .map(|bytes| document_revision(&bytes))
            .unwrap_or_else(|| "frev-absent".to_owned());
        items.push(json!({
            "documentId": name,
            "name": name,
            "revision": revision,
            "content": Value::Null,
            "templateId": Value::Null,
        }));
    }
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "items": items,
            "nextCursor": next_cursor,
            "snapshotRevision": snapshot_revision,
        }),
    ))
}

/// The docs listing's snapshot revision: content identity over the sorted
/// (name, revision) pairs.
fn docs_snapshot(project: &ProjectWorkspace) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(docs_dir(project)) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.path().is_file()
                && (name.ends_with(".txt") || name.ends_with(".md"))
                && let Ok(bytes) = std::fs::read(entry.path())
            {
                parts.push(format!("{name}:{}", document_revision(&bytes)));
            }
        }
    }
    parts.sort();
    let mut hasher = Sha256::new();
    hasher.update(b"docs:");
    for part in &parts {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    format!("dsnap-{:x}", hasher.finalize())
}

fn read_templates(
    document_id: Option<&str>,
    cursor: Option<&str>,
    limit: usize,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    if let Some(template_id) = document_id {
        let Some(template) = template_by_id(template_id) else {
            return Err(not_found("template", template_id).into());
        };
        return Ok((
            ResponseOutcome::Succeeded,
            json!({
                "items": [{
                    "documentId": template.id,
                    "name": template.name,
                    "revision": template_revision(template),
                    "content": template.content,
                    "templateId": template.id,
                }],
                "nextCursor": Value::Null,
                "snapshotRevision": templates_snapshot(),
            }),
        ));
    }
    let offset = parse_offset_cursor(cursor, "ttpl")?;
    let mut items = Vec::new();
    let mut next_cursor = None;
    for (index, template) in TEMPLATES.iter().enumerate().skip(offset) {
        if items.len() >= limit {
            next_cursor = Some(format!("ttpl-{index}"));
            break;
        }
        items.push(json!({
            "documentId": template.id,
            "name": template.name,
            "revision": template_revision(template),
            "content": Value::Null,
            "templateId": template.id,
        }));
    }
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "items": items,
            "nextCursor": next_cursor,
            "snapshotRevision": templates_snapshot(),
        }),
    ))
}

fn templates_snapshot() -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"templates:");
    for template in &TEMPLATES {
        hasher.update(template.id.as_bytes());
        hasher.update(template_revision(template).as_bytes());
        hasher.update([0u8]);
    }
    format!("tsnap-{:x}", hasher.finalize())
}

/// Offset cursors (`<prefix>-<n>`); anything else is INVALID_ARGUMENT.
fn parse_offset_cursor(cursor: Option<&str>, prefix: &str) -> Result<usize, ContractError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    cursor
        .strip_prefix(prefix)
        .and_then(|rest| rest.parse::<usize>().ok())
        .ok_or_else(|| invalid_arg(format!("malformed cursor `{cursor}`")))
}

// ---------------------------------------------------------------------------
// taskmaster.write_document
// ---------------------------------------------------------------------------

async fn taskmaster_write_document(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let project_id = input
        .get("projectId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let document_id = input
        .get("documentId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let expected_revision = input
        .get("expectedRevision")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let content = input.get("content").and_then(Value::as_str);
    let template_id = input.get("templateId").and_then(Value::as_str);
    let variables: Vec<Value> = input
        .get("templateVariables")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    // Acceptance: "content和templateId恰选其一".
    let written = match (content, template_id) {
        (Some(content), None) => content.to_owned(),
        (None, Some(template_id)) => {
            let Some(template) = template_by_id(template_id) else {
                return Err(not_found("template", template_id).into());
            };
            apply_template_variables(template, &variables)?
        }
        (Some(_), Some(_)) => {
            return Err(invalid_arg(
                "`content` and `templateId` are mutually exclusive; pass exactly one",
            )
            .into());
        }
        (None, None) => {
            return Err(invalid_arg("either `content` or `templateId` is required").into());
        }
    };
    validate_document_id(&document_id)?;

    let workspace = workspace_for(&rt, &project_id)?;
    let existing = read_document(&workspace, &document_id)?;
    let created_new = existing.is_none();
    match (expected_revision.as_str(), existing) {
        // Create: the document must not exist ("absent" is the create token;
        // an existing file answers REVISION_CONFLICT so user PRDs are never
        // overwritten).
        ("absent", Some(_)) => {
            return Err(revision_conflict(format!(
                "document `{document_id}` already exists; pass its revision to update it"
            ))
            .into());
        }
        ("absent", None) => {}
        // Update: the revision must match the current content identity.
        (revision, Some(bytes)) if *revision == document_revision(&bytes) => {}
        (revision, Some(_)) => {
            return Err(revision_conflict(format!(
                "expectedRevision `{revision}` does not match the document's current revision"
            ))
            .into());
        }
        (revision, None) => {
            return Err(ContractError::new(
                CODE_NOT_FOUND,
                format!(
                    "document `{document_id}` not found (expectedRevision `{revision}`); \
                     pass `absent` to create it"
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
            .into());
        }
    }

    write_atomic(&docs_dir(&workspace).join(&document_id), written.as_bytes())?;
    let revision = document_revision(written.as_bytes());
    let _ = append_domain_event(
        &rt,
        &event_resource(&project_id),
        "integration.documentChanged",
        &json!({
            "projectId": project_id,
            "documentId": document_id,
            "revision": revision,
            "created": created_new,
            "templateId": template_id,
        }),
    );
    Ok((
        ResponseOutcome::Succeeded,
        json!({"document": {
            "documentId": document_id,
            "name": document_id,
            "revision": revision,
            "content": Value::Null,
            "templateId": template_id,
        }}),
    ))
}

// ---------------------------------------------------------------------------
// taskmaster.remove_document
// ---------------------------------------------------------------------------

async fn taskmaster_remove_document(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let project_id = input
        .get("projectId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let document_id = input
        .get("documentId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let expected_revision = input
        .get("expectedRevision")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    validate_document_id(&document_id)?;

    let workspace = workspace_for(&rt, &project_id)?;
    let existing = read_document(&workspace, &document_id)?;
    let Some(bytes) = existing else {
        return Err(not_found("document", &document_id).into());
    };
    if expected_revision != document_revision(&bytes) {
        return Err(revision_conflict(format!(
            "expectedRevision `{expected_revision}` does not match the document's current revision"
        ))
        .into());
    }
    // "存在解析任务绑定时拒绝": an in-flight parse for this document must
    // not lose the content it is parsing.
    let bound = with_store_conn(&rt, |conn| {
        let found: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM taskmaster_parse_bindings
                 WHERE project_id = ?1 AND document_id = ?2
                   AND state IN ('queued', 'running')",
                params![project_id, document_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| sql_err("lookup parse binding", e))?;
        Ok(found.is_some())
    })?;
    if bound {
        return Err(resource_busy(format!(
            "document `{document_id}` is bound to an in-flight taskmaster.parse_document \
             operation; wait for it to finish before removing the document"
        ))
        .into());
    }

    let path = docs_dir(&workspace).join(&document_id);
    std::fs::remove_file(&path).map_err(|e| io_error(&format!("remove {}", path.display()), &e))?;
    let _ = append_domain_event(
        &rt,
        &event_resource(&project_id),
        "integration.documentChanged",
        &json!({
            "projectId": project_id,
            "documentId": document_id,
            "removed": true,
        }),
    );
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "documentId": document_id,
            "removed": true,
            "revision": expected_revision,
        }),
    ))
}

// ---------------------------------------------------------------------------
// taskmaster.parse_document: the deterministic PRD → tasks parser
// ---------------------------------------------------------------------------

/// One requirement section the parser extracted from a PRD document.
struct ParsedSection {
    title: String,
    body: String,
}

/// The text after a top-level list marker (`- `, `* `, `+ `, `1. `, `1) `),
/// or `None` when the line is not a list item.
fn split_list_item(trimmed: &str) -> Option<&str> {
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            return Some(rest);
        }
    }
    let digits = trimmed.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0 && digits <= 3 {
        let after = &trimmed[digits..];
        if let Some(rest) = after
            .strip_prefix(". ")
            .or_else(|| after.strip_prefix(") "))
        {
            return Some(rest);
        }
        if after == "." || after == ")" {
            return Some("");
        }
    }
    None
}

/// Splits a PRD into requirement sections. Task boundaries are the document's
/// own headings: every `## ` heading starts a task; a document with no
/// level-2 headings falls back to its `# ` headings; a document with no
/// headings at all falls back to its top-level list items; a document with
/// none of those becomes one task. Deeper headings (`### `) and everything
/// else stay inside the enclosing section's body.
fn parse_prd_sections(content: &str) -> Vec<ParsedSection> {
    let lines: Vec<&str> = content.lines().collect();
    let is_heading = |line: &str, level: usize| -> bool {
        let trimmed = line.trim_start();
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        let rest = &trimmed[hashes.min(trimmed.len())..];
        hashes == level && (rest.is_empty() || rest.starts_with(' '))
    };
    let level = if lines.iter().any(|line| is_heading(line, 2)) {
        2
    } else if lines.iter().any(|line| is_heading(line, 1)) {
        1
    } else {
        0
    };
    let starts_task = |line: &str| -> bool {
        if level > 0 {
            return is_heading(line, level);
        }
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        indent <= 2 && split_list_item(trimmed).is_some()
    };

    fn flush(title: &mut Option<String>, body: &mut Vec<&str>, sections: &mut Vec<ParsedSection>) {
        if let Some(title) = title.take() {
            sections.push(ParsedSection {
                title,
                body: body.join("\n").trim().to_owned(),
            });
        }
        body.clear();
    }

    let mut sections: Vec<ParsedSection> = Vec::new();
    let mut title: Option<String> = None;
    let mut body: Vec<&str> = Vec::new();
    for line in lines.iter().copied() {
        if starts_task(line) {
            flush(&mut title, &mut body, &mut sections);
            let section_title = if level > 0 {
                line.trim_start().trim_start_matches('#').trim().to_owned()
            } else {
                split_list_item(line.trim_start())
                    .map(ToOwned::to_owned)
                    .unwrap_or_default()
            };
            title = Some(if section_title.is_empty() {
                format!("Section {}", sections.len() + 1)
            } else {
                section_title
            });
        } else if title.is_some() {
            body.push(line);
        }
    }
    flush(&mut title, &mut body, &mut sections);

    if sections.is_empty() && !content.trim().is_empty() {
        // No headings and no list items: the whole document is one task.
        let first_line = content.lines().find(|line| !line.trim().is_empty());
        sections.push(ParsedSection {
            title: first_line
                .map(|line| line.trim().chars().take(80).collect())
                .unwrap_or_else(|| "Parsed requirement".to_owned()),
            body: content.trim().to_owned(),
        });
    }
    sections
}

async fn taskmaster_parse_document(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let project_id = input
        .get("projectId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let document_id = input
        .get("documentId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let document_revision_input = input
        .get("documentRevision")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let expected_task_revision = input
        .get("expectedTaskRevision")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    validate_document_id(&document_id)?;

    let db = domain_db(&rt)?;
    let operation = accept_domain_operation(
        &db,
        "taskmaster.parse_document",
        &input,
        key.as_deref(),
        || format!("op-{}", rt.store().next_id()),
        || {
            let workspace = workspace_for(&rt, &project_id)?;
            let Some(bytes) = read_document(&workspace, &document_id)? else {
                return Err(not_found("document", &document_id));
            };
            // "输入文档固定revision": the parse reads exactly the content
            // the caller pinned.
            if document_revision_input != document_revision(&bytes) {
                return Err(revision_conflict(format!(
                    "documentRevision `{document_revision_input}` does not match the \
                     document's current revision; the document changed"
                )));
            }
            let Some(set) = read_task_set(&workspace)? else {
                return Err(ContractError::new(
                    CODE_NOT_FOUND,
                    format!(
                        "project `{project_id}` has no task set; run taskmaster.initialize first"
                    ),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            };
            // "任务集并发改变则拒绝覆盖".
            if expected_task_revision != "absent"
                && expected_task_revision != task_set_revision(&set)
            {
                return Err(revision_conflict(format!(
                    "expectedTaskRevision `{expected_task_revision}` does not match the \
                     current task set revision"
                )));
            }
            Ok(())
        },
    )?;
    let operation_id = operation.operation_id.clone();

    // A replayed idempotency key + identical input must not re-parse: the
    // tasks are already generated and the completion durable.
    if operation.receipt["deduplicated"] == true {
        return Ok((ResponseOutcome::Accepted, operation.receipt));
    }

    // Inline execution: record the parse binding for the removal gate, run
    // the deterministic parser, release the binding, terminalize.
    let _ = mark_domain_operation_running(&db, &operation_id, "");
    // The binding is an auxiliary gate; a recording failure must not fail
    // the parse itself (the removal gate simply stays weaker for this one
    // operation — the parse still runs to completion).
    let _ = with_store_conn(&rt, |conn| {
        conn.execute(
            "INSERT OR REPLACE INTO taskmaster_parse_bindings
                (operation_id, project_id, document_id, state, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'running', ?4, ?4)",
            params![operation_id, project_id, document_id, now_rfc3339()],
        )
        .map_err(|e| sql_err("record parse binding", e))?;
        Ok(())
    });

    let workspace = workspace_for(&rt, &project_id)?;
    let bytes = read_document(&workspace, &document_id)?.unwrap_or_default();
    let document_revision_actual = document_revision(&bytes);
    let content = String::from_utf8_lossy(&bytes).into_owned();
    let mut set = read_task_set(&workspace)?.unwrap_or(TaskSet {
        tag: "master".to_owned(),
        tasks: Vec::new(),
    });
    let sections = parse_prd_sections(&content);

    let mut created_task_ids: Vec<String> = Vec::new();
    let mut updated_task_ids: Vec<String> = Vec::new();
    let mut diagnostics: Vec<Value> = Vec::new();
    let now = now_rfc3339();
    for section in &sections {
        if section.body.trim().is_empty() {
            diagnostics.push(json!({
                "code": "empty_section",
                "message": format!("section `{}` has no body", section.title),
            }));
        }
        // Update-by-title: an existing task with the same title gets its
        // description refreshed; a new title appends a new task.
        let existing = set.tasks.iter_mut().find(|task| {
            task.get("title")
                .and_then(Value::as_str)
                .is_some_and(|title| title.eq_ignore_ascii_case(&section.title))
        });
        match existing {
            Some(task) => {
                task["description"] = json!(section.body);
                task["updatedAt"] = json!(now);
                updated_task_ids.push(task_id_string(task));
            }
            None => {
                let id = next_task_id(&set.tasks);
                let task = json!({
                    "id": id,
                    "title": section.title,
                    "description": section.body,
                    "status": "pending",
                    "priority": "medium",
                    "dependencies": [],
                    "details": "",
                    "testStrategy": "",
                    "subtasks": [],
                    "createdAt": now,
                    "updatedAt": now,
                });
                created_task_ids.push(task_id_string(&task));
                set.tasks.push(task);
            }
        }
    }
    if sections.is_empty() {
        diagnostics.push(json!({
            "code": "no_sections",
            "message": "the document produced no requirement sections",
        }));
    }

    write_task_set(&workspace, &set)?;
    let task_set_revision = task_set_revision(&set);
    let _ = with_store_conn(&rt, |conn| {
        conn.execute(
            "DELETE FROM taskmaster_parse_bindings WHERE operation_id = ?1",
            params![operation_id],
        )
        .map_err(|e| sql_err("release parse binding", e))?;
        Ok(())
    });
    let _ = append_domain_event(
        &rt,
        &event_resource(&project_id),
        "integration.tasksChanged",
        &json!({
            "projectId": project_id,
            "documentId": document_id,
            "createdTaskIds": created_task_ids,
            "updatedTaskIds": updated_task_ids,
            "operationId": operation_id,
        }),
    );
    let completion = json!({
        "projectId": project_id,
        "documentId": document_id,
        "documentRevision": document_revision_actual,
        "taskSetRevision": task_set_revision,
        "createdTaskIds": created_task_ids,
        "updatedTaskIds": updated_task_ids,
        "diagnostics": diagnostics,
    });
    let _ = finish_domain_operation(&db, &operation_id, "", "succeeded", &completion);
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

// ---------------------------------------------------------------------------
// taskmaster.list_tasks
// ---------------------------------------------------------------------------

async fn taskmaster_list_tasks(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let project_id = input
        .get("projectId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let selection = input
        .get("selection")
        .and_then(Value::as_str)
        .unwrap_or("all");
    let status = input
        .get("status")
        .and_then(Value::as_str)
        .filter(|status| !status.is_empty());
    let cursor = input.get("cursor").and_then(Value::as_str);
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 200) as usize;

    if selection != "all" && selection != "next" {
        return Err(invalid_arg(format!(
            "selection must be `all` or `next`, got `{selection}`"
        ))
        .into());
    }
    let workspace = workspace_for(&rt, &project_id)?;
    let set = read_task_set(&workspace)?;
    let snapshot_revision = match &set {
        Some(set) => task_set_revision(set),
        None => "tset-absent".to_owned(),
    };
    let mut tasks: Vec<Value> = set
        .map(|set| set.tasks)
        .unwrap_or_default()
        .into_iter()
        .filter(|task| status.is_none_or(|wanted| task_str_or(task, "status", "pending") == wanted))
        .collect();
    // Stable order: ascending on-disk id.
    tasks.sort_by_key(task_id_string);

    let (items, empty_reason) = if selection == "next" {
        // "next遵循真实任务依赖与状态": the first task (id order) that is
        // in progress, or pending with every dependency done. No candidate
        // answers an explicit emptyReason, never a fabricated task.
        let done = |id: &str| {
            tasks
                .iter()
                .any(|task| task_id_string(task) == id && task_str_or(task, "status", "") == "done")
        };
        let candidate = tasks.iter().find(|task| {
            let state = task_str_or(task, "status", "pending");
            state == "in-progress"
                || (state == "pending" && task_dep_strings(task).iter().all(|dep| done(dep)))
        });
        match candidate {
            Some(task) => (vec![task_item(task)], Value::Null),
            None => {
                let reason = if tasks.is_empty() {
                    "task set is empty"
                } else if tasks.iter().all(|task| {
                    !matches!(
                        task_str_or(task, "status", "").as_str(),
                        "pending" | "in-progress"
                    )
                }) {
                    "no pending or in-progress task remains"
                } else {
                    "every pending task has unfinished dependencies"
                };
                (Vec::new(), json!(reason))
            }
        }
    } else {
        (tasks.iter().map(task_item).collect(), Value::Null)
    };

    let offset = parse_offset_cursor(cursor, "ttask")?;
    let mut page: Vec<Value> = Vec::new();
    let mut next_cursor = None;
    for (index, item) in items.iter().enumerate().skip(offset) {
        if page.len() >= limit {
            next_cursor = Some(format!("ttask-{index}"));
            break;
        }
        page.push(item.clone());
    }
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "items": page,
            "nextCursor": next_cursor,
            "snapshotRevision": snapshot_revision,
            "emptyReason": empty_reason,
        }),
    ))
}

// ---------------------------------------------------------------------------
// taskmaster.write_task
// ---------------------------------------------------------------------------

/// Terminalizes an accepted inline operation as failed with the error's
/// code, then returns the error for the response. The pre-accept
/// validation and the mutation run in one non-awaiting stretch, so these
/// paths are unreachable in practice — but an accepted operation that
/// cannot complete must be failed durably, never left `running` and never
/// panicked on.
fn failed_operation(
    db: &Arc<DomainDb>,
    operation_id: &str,
    error: &ContractError,
) -> ContractError {
    let _ = finish_domain_operation(
        db,
        operation_id,
        "",
        "failed",
        &json!({"code": error.code, "message": error.message}),
    );
    error.clone()
}

/// The update-path variant of [`failed_operation`]: the task vanished
/// between the pre-accept validation and the mutation.
fn vanished_task(db: &Arc<DomainDb>, operation_id: &str, task_id: &str) -> ContractError {
    failed_operation(db, operation_id, &not_found("task", task_id))
}

async fn taskmaster_write_task(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let project_id = input
        .get("projectId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let mode = input
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let task_id = input
        .get("taskId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(ToOwned::to_owned);
    let title = input
        .get("title")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty());
    let description = input
        .get("description")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty());
    let status = input
        .get("status")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty());
    let priority = input
        .get("priority")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty());
    let details = input
        .get("details")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty());
    let prompt = input
        .get("prompt")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty());
    let dependency_ids: Vec<String> = input
        .get("dependencyIds")
        .and_then(Value::as_array)
        .map(|deps| {
            deps.iter()
                .filter_map(|dep| dep.as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let expected_revision = input
        .get("expectedRevision")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    if !matches!(
        mode.as_str(),
        "createGenerated" | "updateFields" | "setStatus"
    ) {
        return Err(invalid_arg(format!(
            "mode must be createGenerated, updateFields or setStatus, got `{mode}`"
        ))
        .into());
    }
    if let Some(priority) = priority.filter(|value| !KNOWN_PRIORITIES.contains(value)) {
        return Err(invalid_arg(format!(
            "priority must be one of high/medium/low, got `{priority}`"
        ))
        .into());
    }
    if let Some(status) = status.filter(|value| !KNOWN_STATUSES.contains(value)) {
        return Err(invalid_arg(format!(
            "status must be one of {} (the task-master status set), got `{status}`",
            KNOWN_STATUSES.join("/")
        ))
        .into());
    }

    let db = domain_db(&rt)?;
    let operation = accept_domain_operation(
        &db,
        "taskmaster.write_task",
        &input,
        key.as_deref(),
        || format!("op-{}", rt.store().next_id()),
        || {
            let workspace = workspace_for(&rt, &project_id)?;
            let Some(set) = read_task_set(&workspace)? else {
                return Err(ContractError::new(
                    CODE_NOT_FOUND,
                    format!(
                        "project `{project_id}` has no task set; run taskmaster.initialize first"
                    ),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            };
            match mode.as_str() {
                // Create: the dependencies must resolve without cycles.
                "createGenerated" => {
                    if !prompt.is_some() && !(title.is_some() && description.is_some()) {
                        return Err(invalid_arg(
                            "createGenerated requires `prompt`, or both `title` and `description`",
                        ));
                    }
                    validate_dependencies(&set.tasks, "__new__", &dependency_ids)?;
                }
                // Update: the task must exist and its revision must match.
                _ => {
                    let Some(task_id) = &task_id else {
                        return Err(invalid_arg(format!("mode `{mode}` requires `taskId`")));
                    };
                    let Some(task) = find_task(&set.tasks, task_id) else {
                        return Err(not_found("task", task_id));
                    };
                    if expected_revision != task_revision(task) {
                        return Err(revision_conflict(format!(
                            "expectedRevision `{expected_revision}` does not match task \
                             `{task_id}`'s current revision"
                        )));
                    }
                }
            }
            Ok(())
        },
    )?;
    let operation_id = operation.operation_id.clone();

    // A replayed idempotency key + identical input must not re-apply the
    // task mutation: the change is durable and the completion queryable.
    if operation.receipt["deduplicated"] == true {
        return Ok((ResponseOutcome::Accepted, operation.receipt));
    }

    // Inline execution: mutate the task set, emit the change event,
    // terminalize with the task projection. The handler never awaits
    // between the pre-accept validation and the write, so concurrent
    // dispatches cannot interleave on the same task set.
    let _ = mark_domain_operation_running(&db, &operation_id, "");
    let workspace = workspace_for(&rt, &project_id)?;
    let mut set = read_task_set(&workspace)?.unwrap_or(TaskSet {
        tag: "master".to_owned(),
        tasks: Vec::new(),
    });
    let now = now_rfc3339();
    let written: Value = match mode.as_str() {
        "createGenerated" => {
            // The generator: a prompt derives its title from its first
            // line; explicit title+description are used as given (the
            // baseline fed both shapes to the external generator — see the
            // module doc for the in-process deviation).
            let (title, description) = match (title, description, prompt) {
                (Some(title), Some(description), _) => (title.to_owned(), description.to_owned()),
                (_, _, Some(prompt)) => {
                    let first_line = prompt.lines().find(|line| !line.trim().is_empty());
                    let title: String = first_line
                        .map(|line| line.trim().chars().take(80).collect::<String>())
                        .unwrap_or_else(|| "Generated task".to_owned());
                    (title, prompt.to_owned())
                }
                // The pre-accept validation rejected every other shape; a
                // late mismatch still fails the operation loudly.
                _ => {
                    return Err(failed_operation(
                        &db,
                        &operation_id,
                        &invalid_arg(
                            "createGenerated requires `prompt`, or both `title` and `description`",
                        ),
                    )
                    .into());
                }
            };
            let id = next_task_id(&set.tasks);
            let task = json!({
                "id": id,
                "title": title,
                "description": description,
                "status": "pending",
                "priority": priority.unwrap_or("medium"),
                "dependencies": dependency_ids
                    .iter()
                    .map(|dep| match dep.parse::<i64>() {
                        Ok(number) => json!(number),
                        Err(_) => json!(dep),
                    })
                    .collect::<Vec<Value>>(),
                "details": details.unwrap_or(""),
                "testStrategy": "",
                "subtasks": [],
                "createdAt": now,
                "updatedAt": now,
            });
            set.tasks.push(task.clone());
            task
        }
        "updateFields" => {
            let task_id = task_id.clone().unwrap_or_default();
            let Some(task) = set
                .tasks
                .iter_mut()
                .find(|task| task_id_string(task) == task_id)
            else {
                return Err(vanished_task(&db, &operation_id, &task_id).into());
            };
            if let Some(title) = title {
                task["title"] = json!(title);
            }
            if let Some(description) = description {
                task["description"] = json!(description);
            }
            if let Some(priority) = priority {
                task["priority"] = json!(priority);
            }
            if let Some(details) = details {
                task["details"] = json!(details);
            }
            task["updatedAt"] = json!(now);
            task.clone()
        }
        _ => {
            let task_id = task_id.clone().unwrap_or_default();
            let Some(task) = set
                .tasks
                .iter_mut()
                .find(|task| task_id_string(task) == task_id)
            else {
                return Err(vanished_task(&db, &operation_id, &task_id).into());
            };
            // setStatus only changes the status — no other field moves.
            task["status"] = json!(status.unwrap_or("pending"));
            task["updatedAt"] = json!(now);
            task.clone()
        }
    };
    write_task_set(&workspace, &set)?;
    let task_set_revision = task_set_revision(&set);
    let _ = append_domain_event(
        &rt,
        &event_resource(&project_id),
        "integration.tasksChanged",
        &json!({
            "projectId": project_id,
            "taskId": task_id_string(&written),
            "mode": mode,
            "operationId": operation_id,
        }),
    );
    let completion = json!({
        "task": task_item(&written),
        "taskSetRevision": task_set_revision,
    });
    let _ = finish_domain_operation(&db, &operation_id, "", "succeeded", &completion);
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::initialize_instance;
    use crate::public_v1::store::{InstanceMeta, V1Store};
    use crate::public_v1::test_support::TempDir;
    use std::path::PathBuf;

    struct Fixture {
        _tmp: TempDir,
        rt: Arc<PublicV1Runtime>,
        home: PathBuf,
        instance_id: String,
    }

    async fn fixture(tag: &str) -> Fixture {
        let tmp = TempDir::new(tag);
        let home = tmp.0.join("pd-home");
        initialize_instance(&home, None, Some("local"), Some(true)).unwrap();
        let rt = open_runtime(&home);
        let instance_id = rt.store().meta().instance_id.clone();
        Fixture {
            _tmp: tmp,
            rt,
            home,
            instance_id,
        }
    }

    fn open_runtime(root_dir: &Path) -> Arc<PublicV1Runtime> {
        let (file, _token) =
            crate::public_v1::bootstrap::load_local_instance_and_token(root_dir, None).unwrap();
        let meta = InstanceMeta {
            instance_id: file.instance_id.clone(),
            owner_id: file.owner_id.clone(),
            product: file.product.clone(),
            protocol_version: file.protocol_version.clone(),
            node_id: file.node_id.clone(),
            generation: "gen-test-1".to_owned(),
            server_revision: "srv-test-1".to_owned(),
            initialization_revision: file.initialization_revision.clone(),
        };
        let store = V1Store::open_and_recover(root_dir, meta).unwrap();
        PublicV1Runtime::new(store)
    }

    /// Seeds a project whose authorized root is a real temp directory.
    async fn seeded_project(fx: &Fixture, tag: &str) -> PathBuf {
        let workspace = fx.home.join(format!("ws-{tag}"));
        std::fs::create_dir_all(&workspace).unwrap();
        with_store_conn(&fx.rt, |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO authorized_roots (root_id, instance_id, absolute_path, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    format!("root-{tag}"),
                    fx.instance_id,
                    workspace.to_string_lossy(),
                    now_rfc3339()
                ],
            )
            .map_err(|e| sql_err("seed authorized root", e))?;
            Ok(())
        })
        .unwrap();
        fx.rt
            .dispatch(
                "project.register",
                json!({
                    "instanceId": fx.instance_id,
                    "projectId": format!("proj-{tag}"),
                    "name": "Taskmaster tests",
                    "environmentId": "local",
                    "root": {
                        "instanceId": fx.instance_id,
                        "rootId": format!("root-{tag}"),
                        "relativePath": ""
                    }
                }),
                None,
                None,
            )
            .await
            .unwrap();
        workspace
    }

    async fn initialize(fx: &Fixture, tag: &str) -> String {
        let project_id = format!("proj-{tag}");
        let (_, receipt) = fx
            .rt
            .dispatch(
                "taskmaster.initialize",
                json!({
                    "projectId": project_id,
                    "schemaId": INIT_SCHEMA_ID,
                    "configuration": {"rules": ["default"]},
                }),
                Some(&format!("key-init-{tag}")),
                None,
            )
            .await
            .unwrap();
        assert_eq!(receipt["kind"], "operation");
        receipt["operationId"].as_str().unwrap().to_owned()
    }

    async fn dispatch_err(
        rt: &Arc<PublicV1Runtime>,
        method: &str,
        input: Value,
        key: Option<&str>,
    ) -> ContractError {
        match rt.dispatch(method, input, key, None).await {
            Err(DispatchError::Application(error)) => error,
            other => panic!("expected application error for `{method}`, got {other:?}"),
        }
    }

    /// Polls `operations.get` until the operation is terminal and returns
    /// its completion result.
    async fn wait_operation(fx: &Fixture, operation_id: &str) -> Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let (_, page) = fx
                .rt
                .dispatch(
                    "operations.get",
                    json!({"operationId": operation_id}),
                    None,
                    None,
                )
                .await
                .unwrap();
            let state = page["operation"]["state"].as_str().unwrap_or_default();
            if matches!(state, "succeeded" | "failed" | "cancelled" | "interrupted") {
                return page["operation"]["result"].clone();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "operation `{operation_id}` never became terminal (state `{state}`)"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[test]
    fn document_id_validation() {
        for id in ["prd.txt", "my prd.md", "v1.2-spec.txt", "a_b-c.md"] {
            validate_document_id(id).unwrap_or_else(|e| panic!("`{id}` must be valid: {e:?}"));
        }
        for id in [
            "prd.pdf",
            ".txt",
            "sub/dir.txt",
            "../escape.txt",
            "/abs.txt",
            "",
            "a/b",
            "..",
        ] {
            let error = validate_document_id(id).unwrap_err();
            assert!(
                error.code == CODE_PATH_OUTSIDE_SCOPE || error.code == CODE_INVALID_ARGUMENT,
                "`{id}` must be rejected (got {})",
                error.code
            );
        }
        assert_eq!(
            validate_document_id("sub/dir.txt").unwrap_err().code,
            CODE_PATH_OUTSIDE_SCOPE
        );
        assert_eq!(
            validate_document_id("prd.pdf").unwrap_err().code,
            CODE_INVALID_ARGUMENT
        );
    }

    #[test]
    fn prd_parser_splits_headings_and_falls_back() {
        let heading_doc = "# Spec\n\n## First requirement\n\nBody A\n\n### Sub detail\n\nMore\n\n## Second requirement\n\nBody B\n";
        let sections = parse_prd_sections(heading_doc);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].title, "First requirement");
        assert!(sections[0].body.contains("Body A"));
        assert!(sections[0].body.contains("Sub detail"));
        assert_eq!(sections[1].title, "Second requirement");

        let h1_doc = "# One\nBody 1\n# Two\nBody 2\n";
        let sections = parse_prd_sections(h1_doc);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[1].body, "Body 2");

        let list_doc = "- first item\n  detail\n- second item\n";
        let sections = parse_prd_sections(list_doc);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].title, "first item");
        assert_eq!(sections[0].body, "detail");

        let plain = "Just a paragraph.\nAnother line.\n";
        let sections = parse_prd_sections(plain);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].title, "Just a paragraph.");
        assert!(parse_prd_sections("   \n\n").is_empty());
    }

    #[test]
    fn task_set_round_trip_and_revisions() {
        let set = TaskSet {
            tag: "master".to_owned(),
            tasks: vec![json!({
                "id": 1, "title": "T1", "description": "D1", "status": "pending",
                "priority": "high", "dependencies": [], "details": "",
            })],
        };
        let bytes = encode_task_set(&set);
        let decoded = decode_task_set(serde_json::from_slice(&bytes).unwrap());
        assert_eq!(decoded.tag, "master");
        assert_eq!(decoded.tasks.len(), 1);
        assert_eq!(task_id_string(&decoded.tasks[0]), "1");
        // The set revision is the content identity of the exact bytes.
        assert_eq!(task_set_revision_bytes(&bytes), task_set_revision(&decoded));
        // Legacy array, simple {tasks} and tagged shapes decode too.
        let legacy = decode_task_set(json!([{"id": 7, "title": "Legacy"}]));
        assert_eq!(task_id_string(&legacy.tasks[0]), "7");
        let simple = decode_task_set(json!({"tasks": [{"id": 9, "title": "Simple"}]}));
        assert_eq!(simple.tasks.len(), 1);
        let tagged = decode_task_set(json!({"master": {"tasks": [{"id": 3}]}
        }));
        assert_eq!(tagged.tag, "master");
        assert_eq!(tagged.tasks.len(), 1);
        let foreign_tag = decode_task_set(json!({"release": {"tasks": [{"id": 4}]}
        }));
        assert_eq!(foreign_tag.tag, "release");
    }

    #[test]
    fn dependency_validation_rejects_unknown_and_cycles() {
        let tasks = vec![
            json!({"id": 1, "dependencies": [2]}),
            json!({"id": 2, "dependencies": []}),
        ];
        // Unknown dependency on create.
        let error = validate_dependencies(&tasks, "__new__", &["9".to_owned()]).unwrap_err();
        assert_eq!(error.code, CODE_DEPENDENCY_CONFLICT);
        // A cycle through existing rows is caught too.
        let cyclic = vec![
            json!({"id": 1, "dependencies": [2]}),
            json!({"id": 2, "dependencies": [1]}),
        ];
        let error = validate_dependencies(&cyclic, "__new__", &[]).unwrap_err();
        assert_eq!(error.code, CODE_DEPENDENCY_CONFLICT);
        assert!(validate_dependencies(&tasks, "__new__", &["1".to_owned()]).is_ok());
    }

    #[tokio::test]
    async fn initialize_creates_structure_and_rejects_reinit() {
        let fx = fixture("tm-init").await;
        seeded_project(&fx, "init").await;
        let operation_id = initialize(&fx, "init").await;
        let completion = wait_operation(&fx, &operation_id).await;
        assert_eq!(completion["initialized"], true);
        assert_eq!(completion["version"], STRUCTURE_VERSION);
        let created = completion["createdRelativePaths"].as_array().unwrap();
        assert!(created.contains(&json!(".taskmaster/tasks/tasks.json")));
        assert!(completion["configurationRevision"].as_str().is_some());
        let workspace = fx.home.join("ws-init");
        assert!(workspace.join(".taskmaster/docs").is_dir());
        assert!(workspace.join(".taskmaster/config.json").is_file());

        // Re-initialize answers ALREADY_INITIALIZED and touches nothing.
        let error = dispatch_err(
            &fx.rt,
            "taskmaster.initialize",
            json!({
                "projectId": "proj-init",
                "schemaId": INIT_SCHEMA_ID,
                "configuration": {},
            }),
            Some("key-init-2"),
        )
        .await;
        assert_eq!(error.code, CODE_ALREADY_INITIALIZED);
        assert!(workspace.join(".taskmaster/config.json").is_file());

        // Unknown schemaId and unknown configuration fields reject.
        // taskmaster.initialize 是 idempotentSubmit（合同 retry）：负例也必须
        // 携带各不相同的 idempotencyKey（缺键 -32086；同键异输入则先撞
        // IDEMPOTENCY_CONFLICT，到不了本例要验的 INVALID_ARGUMENT）。
        let error = dispatch_err(
            &fx.rt,
            "taskmaster.initialize",
            json!({
                "projectId": "proj-init",
                "schemaId": "agentcpm-pd.taskmaster.init/2",
                "configuration": {},
            }),
            Some("key-init-3"),
        )
        .await;
        assert_eq!(error.code, CODE_INVALID_ARGUMENT);
        let error = dispatch_err(
            &fx.rt,
            "taskmaster.initialize",
            json!({
                "projectId": "proj-init",
                "schemaId": INIT_SCHEMA_ID,
                "configuration": {"rules": ["a"], "shell": "yes"},
            }),
            Some("key-init-4"),
        )
        .await;
        assert_eq!(error.code, CODE_INVALID_ARGUMENT);
    }

    #[tokio::test]
    async fn document_crud_and_template_generation() {
        let fx = fixture("tm-docs").await;
        seeded_project(&fx, "docs").await;
        initialize(&fx, "docs").await;

        // Create with content.
        let (_, created) = fx
            .rt
            .dispatch(
                "taskmaster.write_document",
                json!({
                    "projectId": "proj-docs",
                    "documentId": "prd.txt",
                    "content": "# Spec\n\n## First\n\nBody\n",
                    "expectedRevision": "absent",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        let revision = created["document"]["revision"].as_str().unwrap().to_owned();
        assert!(revision.starts_with("frev-"));

        // Absent on an existing document must not overwrite.
        let error = dispatch_err(
            &fx.rt,
            "taskmaster.write_document",
            json!({
                "projectId": "proj-docs",
                "documentId": "prd.txt",
                "content": "overwritten",
                "expectedRevision": "absent",
            }),
            None,
        )
        .await;
        assert_eq!(error.code, CODE_REVISION_CONFLICT);

        // Stale revision must not overwrite.
        let error = dispatch_err(
            &fx.rt,
            "taskmaster.write_document",
            json!({
                "projectId": "proj-docs",
                "documentId": "prd.txt",
                "content": "overwritten",
                "expectedRevision": "frev-stale",
            }),
            None,
        )
        .await;
        assert_eq!(error.code, CODE_REVISION_CONFLICT);

        // Matching revision updates.
        let (_, updated) = fx
            .rt
            .dispatch(
                "taskmaster.write_document",
                json!({
                    "projectId": "proj-docs",
                    "documentId": "prd.txt",
                    "content": "# Spec v2\n\n## First\n\nBody v2\n",
                    "expectedRevision": revision,
                }),
                None,
                None,
            )
            .await
            .unwrap();
        let new_revision = updated["document"]["revision"].as_str().unwrap().to_owned();
        assert_ne!(revision, new_revision);

        // content and templateId are mutually exclusive.
        let error = dispatch_err(
            &fx.rt,
            "taskmaster.write_document",
            json!({
                "projectId": "proj-docs",
                "documentId": "both.txt",
                "content": "x",
                "templateId": "web-app",
                "expectedRevision": "absent",
            }),
            None,
        )
        .await;
        assert_eq!(error.code, CODE_INVALID_ARGUMENT);

        // Template generation with variables; unknown placeholders reject.
        let error = dispatch_err(
            &fx.rt,
            "taskmaster.write_document",
            json!({
                "projectId": "proj-docs",
                "documentId": "tpl.txt",
                "templateId": "web-app",
                "templateVariables": [{"name": "NoSuch", "value": "x"}],
                "expectedRevision": "absent",
            }),
            None,
        )
        .await;
        assert_eq!(error.code, CODE_INVALID_ARGUMENT);
        let (_, generated) = fx
            .rt
            .dispatch(
                "taskmaster.write_document",
                json!({
                    "projectId": "proj-docs",
                    "documentId": "web-prd.md",
                    "templateId": "web-app",
                    "templateVariables": [{"name": "Date", "value": "2026-11-18"}],
                    "expectedRevision": "absent",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(generated["document"]["templateId"], "web-app");
        let generated_file = fx
            .rt
            .dispatch(
                "taskmaster.read_documents",
                json!({"projectId": "proj-docs", "kind": "prd", "documentId": "web-prd.md"}),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        assert!(
            generated_file["items"][0]["content"]
                .as_str()
                .is_some_and(|content| content.contains("2026-11-18"))
        );

        // Read: single documents carry content; the listing does not.
        let (_, single) = fx
            .rt
            .dispatch(
                "taskmaster.read_documents",
                json!({"projectId": "proj-docs", "kind": "prd", "documentId": "prd.txt"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(single["items"].as_array().unwrap().len(), 1);
        assert!(
            single["items"][0]["content"]
                .as_str()
                .is_some_and(|content| content.contains("Spec v2"))
        );
        let (_, listing) = fx
            .rt
            .dispatch(
                "taskmaster.read_documents",
                json!({"projectId": "proj-docs", "kind": "prd"}),
                None,
                None,
            )
            .await
            .unwrap();
        let items = listing["items"].as_array().unwrap();
        assert!(items.len() >= 2);
        assert!(items.iter().all(|item| item["content"].is_null()));

        // Template listing and single read.
        let (_, templates) = fx
            .rt
            .dispatch(
                "taskmaster.read_documents",
                json!({"kind": "template"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(templates["items"].as_array().unwrap().len(), 4);
        let (_, one) = fx
            .rt
            .dispatch(
                "taskmaster.read_documents",
                json!({"kind": "template", "documentId": "api"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert!(
            one["items"][0]["content"]
                .as_str()
                .is_some_and(|content| content.contains("API"))
        );

        // PRD without projectId rejects.
        let error = dispatch_err(
            &fx.rt,
            "taskmaster.read_documents",
            json!({"kind": "prd"}),
            None,
        )
        .await;
        assert_eq!(error.code, CODE_INVALID_ARGUMENT);

        // Path-shaped ids reject with PATH_OUTSIDE_SCOPE.
        let error = dispatch_err(
            &fx.rt,
            "taskmaster.read_documents",
            json!({"projectId": "proj-docs", "kind": "prd", "documentId": "../x.txt"}),
            None,
        )
        .await;
        assert_eq!(error.code, CODE_PATH_OUTSIDE_SCOPE);

        // Remove with the current revision; a stale revision rejects.
        let error = dispatch_err(
            &fx.rt,
            "taskmaster.remove_document",
            json!({
                "projectId": "proj-docs",
                "documentId": "prd.txt",
                "expectedRevision": revision,
            }),
            None,
        )
        .await;
        assert_eq!(error.code, CODE_REVISION_CONFLICT);
        let (_, removed) = fx
            .rt
            .dispatch(
                "taskmaster.remove_document",
                json!({
                    "projectId": "proj-docs",
                    "documentId": "prd.txt",
                    "expectedRevision": new_revision,
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(removed["removed"], true);
        let error = dispatch_err(
            &fx.rt,
            "taskmaster.read_documents",
            json!({"projectId": "proj-docs", "kind": "prd", "documentId": "prd.txt"}),
            None,
        )
        .await;
        assert_eq!(error.code, CODE_NOT_FOUND);
    }

    #[tokio::test]
    async fn parse_generate_and_list_tasks() {
        let fx = fixture("tm-tasks").await;
        seeded_project(&fx, "tasks").await;
        initialize(&fx, "tasks").await;

        // Write a PRD with two level-2 sections.
        let (_, doc) = fx
            .rt
            .dispatch(
                "taskmaster.write_document",
                json!({
                    "projectId": "proj-tasks",
                    "documentId": "spec.txt",
                    "content": "# Spec\n\n## Alpha\n\nDo alpha\n\n## Beta\n\nDo beta\n",
                    "expectedRevision": "absent",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        let doc_revision = doc["document"]["revision"].as_str().unwrap().to_owned();

        // The empty set's revision comes from list_tasks.
        let (_, empty_list) = fx
            .rt
            .dispatch(
                "taskmaster.list_tasks",
                json!({"projectId": "proj-tasks"}),
                None,
                None,
            )
            .await
            .unwrap();
        let set_revision = empty_list["snapshotRevision"].as_str().unwrap().to_owned();

        let (_, receipt) = fx
            .rt
            .dispatch(
                "taskmaster.parse_document",
                json!({
                    "projectId": "proj-tasks",
                    "documentId": "spec.txt",
                    "documentRevision": doc_revision,
                    "expectedTaskRevision": set_revision,
                }),
                Some("key-parse-1"),
                None,
            )
            .await
            .unwrap();
        let completion = wait_operation(&fx, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(completion["documentRevision"], doc_revision);
        let created = completion["createdTaskIds"].as_array().unwrap();
        assert_eq!(created.len(), 2, "both sections became tasks: {completion}");
        assert_eq!(completion["updatedTaskIds"].as_array().unwrap().len(), 0);
        assert!(completion["taskSetRevision"].as_str().is_some());

        // The tasks are readable and ordered.
        let (_, listed) = fx
            .rt
            .dispatch(
                "taskmaster.list_tasks",
                json!({"projectId": "proj-tasks"}),
                None,
                None,
            )
            .await
            .unwrap();
        let items = listed["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["taskId"], "1");
        assert_eq!(items[0]["title"], "Alpha");
        assert_eq!(items[0]["status"], "pending");
        assert_eq!(items[0]["priority"], "medium");
        assert!(
            items[0]["revision"]
                .as_str()
                .is_some_and(|revision| revision.starts_with("trev-"))
        );

        // next: the first pending task with satisfied dependencies.
        let (_, next) = fx
            .rt
            .dispatch(
                "taskmaster.list_tasks",
                json!({"projectId": "proj-tasks", "selection": "next"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(next["items"].as_array().unwrap().len(), 1);
        assert_eq!(next["items"][0]["title"], "Alpha");
        assert!(next["emptyReason"].is_null());

        // createGenerated with prompt; the generated id is real and readable.
        let (_, receipt) = fx
            .rt
            .dispatch(
                "taskmaster.write_task",
                json!({
                    "projectId": "proj-tasks",
                    "mode": "createGenerated",
                    "prompt": "Add export\nSupport exporting the report to PDF",
                    "expectedRevision": "absent",
                }),
                Some("key-add-1"),
                None,
            )
            .await
            .unwrap();
        let completion = wait_operation(&fx, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(completion["task"]["title"], "Add export");
        assert_eq!(completion["task"]["status"], "pending");
        let generated_id = completion["task"]["taskId"].as_str().unwrap().to_owned();
        let generated_revision = completion["task"]["revision"].as_str().unwrap().to_owned();

        // Unknown dependency rejects with DEPENDENCY_CONFLICT.
        let error = dispatch_err(
            &fx.rt,
            "taskmaster.write_task",
            json!({
                "projectId": "proj-tasks",
                "mode": "createGenerated",
                "prompt": "Depends on nothing that exists",
                "dependencyIds": ["99"],
                "expectedRevision": "absent",
            }),
            Some("key-add-bad-dep"),
        )
        .await;
        assert_eq!(error.code, CODE_DEPENDENCY_CONFLICT);

        // setStatus only moves the status.
        let (_, receipt) = fx
            .rt
            .dispatch(
                "taskmaster.write_task",
                json!({
                    "projectId": "proj-tasks",
                    "mode": "setStatus",
                    "taskId": generated_id,
                    "status": "done",
                    "expectedRevision": generated_revision,
                }),
                Some("key-set-1"),
                None,
            )
            .await
            .unwrap();
        let completion = wait_operation(&fx, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(completion["task"]["status"], "done");
        assert_eq!(completion["task"]["title"], "Add export");

        // updateFields keeps omitted fields.
        let new_revision = completion["task"]["revision"].as_str().unwrap().to_owned();
        let (_, receipt) = fx
            .rt
            .dispatch(
                "taskmaster.write_task",
                json!({
                    "projectId": "proj-tasks",
                    "mode": "updateFields",
                    "taskId": generated_id,
                    "priority": "high",
                    "expectedRevision": new_revision,
                }),
                Some("key-upd-1"),
                None,
            )
            .await
            .unwrap();
        let completion = wait_operation(&fx, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(completion["task"]["priority"], "high");
        assert_eq!(completion["task"]["title"], "Add export");
        assert_eq!(completion["task"]["status"], "done");

        // Illegal status values reject.
        let error = dispatch_err(
            &fx.rt,
            "taskmaster.write_task",
            json!({
                "projectId": "proj-tasks",
                "mode": "setStatus",
                "taskId": generated_id,
                "status": "finished",
                "expectedRevision": "trev-whatever",
            }),
            Some("key-set-bad"),
        )
        .await;
        assert_eq!(error.code, CODE_INVALID_ARGUMENT);

        // Re-parsing the same document updates by title instead of
        // duplicating, and a stale task revision rejects.
        let error = dispatch_err(
            &fx.rt,
            "taskmaster.parse_document",
            json!({
                "projectId": "proj-tasks",
                "documentId": "spec.txt",
                "documentRevision": doc_revision,
                "expectedTaskRevision": set_revision,
            }),
            Some("key-parse-2"),
        )
        .await;
        assert_eq!(error.code, CODE_REVISION_CONFLICT);
        let (_, listed) = fx
            .rt
            .dispatch(
                "taskmaster.list_tasks",
                json!({"projectId": "proj-tasks"}),
                None,
                None,
            )
            .await
            .unwrap();
        let current_set = listed["snapshotRevision"].as_str().unwrap().to_owned();
        let (_, receipt) = fx
            .rt
            .dispatch(
                "taskmaster.parse_document",
                json!({
                    "projectId": "proj-tasks",
                    "documentId": "spec.txt",
                    "documentRevision": doc_revision,
                    "expectedTaskRevision": current_set,
                }),
                Some("key-parse-3"),
                None,
            )
            .await
            .unwrap();
        let completion = wait_operation(&fx, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(completion["createdTaskIds"].as_array().unwrap().len(), 0);
        assert_eq!(completion["updatedTaskIds"].as_array().unwrap().len(), 2);

        // A changed document revision rejects the parse.
        let error = dispatch_err(
            &fx.rt,
            "taskmaster.parse_document",
            json!({
                "projectId": "proj-tasks",
                "documentId": "spec.txt",
                "documentRevision": "frev-changed",
                "expectedTaskRevision": current_set,
            }),
            Some("key-parse-4"),
        )
        .await;
        assert_eq!(error.code, CODE_REVISION_CONFLICT);

        // Uninitialized projects answer NOT_FOUND on the task surface.
        let error = dispatch_err(
            &fx.rt,
            "taskmaster.list_tasks",
            json!({"projectId": "proj-none"}),
            None,
        )
        .await;
        assert_eq!(error.code, CODE_NOT_FOUND);
    }
}
