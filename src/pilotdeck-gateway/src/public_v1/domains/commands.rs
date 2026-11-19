//! `commands.*` — the user-command catalog: discovery, content loading, and
//! session execution (`cli.task.t017`, S3; capability a029).
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `commands.list` / `commands.read`
//! / `commands.execute` 条目。TS 基线（cc7c4e6）是语义参考：
//! * 网关方法 `commands_list`（`src/gateway/dialog/commands.ts::listCommands`）
//!   与 `POST /api/commands/list`；
//! * `POST /api/commands/load`（读命令正文）；
//! * `POST /api/commands/execute`（准备命令正文，不做 bash 执行）。
//!
//! 目录形态（照 TS 的发现布局投影到 v1 世界）：
//! * `project` 命名空间：`<工作区>/.pilotdeck/commands/**/*.md`（工作区经
//!   `files.rs::resolve_project` 解析，与 files 域同一防线）；
//! * `user` 命名空间：`<AGENTCPM_PD_HOME>/commands/**/*.md`（TS 的
//!   `pilotHome/commands` 在 v1 实例 home 上的对应物——不是 `~/.pilotdeck`）；
//! * 去重：同名命令 project 优先于 user（TS 的先见者胜）；
//! * 与 TS 相比少的来源：`.pilotdeck/skills`（技能归 extensions-management
//!   任务的 a030 面）、`BUNDLED_SKILL_STUBS`、web 内建命令（UI/TUI 呈现，
//!   能力采纳记录里 TUI 范围延期）、旧线 `channel/commands` 的 IM 宿主命令
//!   （/update、/search 等——v1 网关不挂 IM 通道，这些没有可执行语义）。本
//!   轮目录因此只含文件命令，全部可 read/execute；「内置与文件命令不重名误
//!   覆盖」在无内建命名空间时平凡成立。
//!
//! `commandId` 是本域的稳定标识 `<namespace>:<相对路径去.md>`（如
//! `project:deploy/rollout`）——既不是展示名（`/deploy/rollout`）也不是文件
//! 路径；`revision` 是文件内容的 `cmdrev-<sha256前24>`（内容变更即过期，
//! commands.execute 的 expectedRevision 对它 CAS）。请求只携带 commandId：
//! 路径形态（绝对路径、`..` 逃逸）的 id 在 commands.read 显式拒绝
//! `PATH_OUTSIDE_SCOPE`（该错误只在 read 的合同错误表里），在 execute 归
//! `NOT_FOUND`。
//!
//! `commands.execute` 语义（照合同 acceptance）：受理为异步域操作，执行体是
//! 「准备正文 + 向会话提交 run」——与 TS `/api/commands/execute` 一致地**只在
//! 服务端做参数替换**（`$ARGUMENTS` ← 空格连接的 args、`$1..$N` 位置参数，
//! 语义含 `\\$N\\b` 的词边界与 TS 的替换顺序；空参数时 `$ARGUMENTS` 仍替换
//! 为空串，`$1..$N` 因循环不执行而原样保留），替换是纯字符串操作，参数值
//! 原样落正文、绝不经 shell——参数注入面由此闭合。会话忙（活动/排队 run）
//! 在受理前给 `SESSION_BUSY`；命令版本过期给 `REVISION_CONFLICT`；完成值
//! `{runId, runState}` 可追到实际 run（「结果可追到实际run」）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_REVISION_CONFLICT, CODE_UNAVAILABLE, ContractError,
    EffectStatus, RetrySpec,
};
use agentcpm_pd_protocol::schema_validator::validate_against_schema;
use rusqlite::params;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::files::resolve_project;
use crate::public_v1::domains::schedules::{
    DDL_DOMAIN_OPERATIONS, DomainDb, accept_domain_operation, finish_domain_operation,
    mark_domain_operation_running,
};
use crate::public_v1::domains::tools::{ToolScope, parse_tool_scope, required_str};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::SubmitDisposition;

/// Contract error codes these operations declare that the protocol crate has
/// no shared constant for yet.
const CODE_SCHEMA_MISMATCH: &str = "SCHEMA_MISMATCH";
const CODE_SESSION_BUSY: &str = "SESSION_BUSY";
const CODE_PATH_OUTSIDE_SCOPE: &str = "PATH_OUTSIDE_SCOPE";

/// The registered, version-fixed input contract every file command declares
/// this round (TS 命令只有 argumentHint 字符串提示，无结构化输入声明；该
/// schema 是 v1 投影的缺省合同). Custom frontmatter `inputSchema` keys have
/// no producer in the baseline and are not consumed (see task report).
const COMMAND_INPUT_SCHEMA_ID: &str = "agentcpm-pd.command.input/1";
const COMMAND_INPUT_SCHEMA: &str = r#"{"type":"object","properties":{"args":{"type":"array","items":{"type":"string"},"description":"Positional arguments: $1..$N, with $ARGUMENTS as their space-joined form."}},"required":["args"],"additionalProperties":false}"#;
/// The completion value of `commands.execute`: the run the prepared content
/// was submitted to, and its state at completion time.
#[allow(dead_code)] // future seam: operations.get completionResult shape
const COMMAND_EXEC_RESULT_SCHEMA_ID: &str = "agentcpm-pd.command.execresult/1";
#[allow(dead_code)]
const COMMAND_EXEC_RESULT_SCHEMA: &str = r#"{"type":"object","properties":{"runId":{"type":"string"},"runState":{"type":"string"}},"required":["runId","runState"],"additionalProperties":false}"#;

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.handler("commands.list", |rt, input, _key, _rev| {
        Box::pin(commands_list(rt, input))
    });
    reg.handler("commands.read", |rt, input, _key, _rev| {
        Box::pin(commands_read(rt, input))
    });
    reg.handler("commands.execute", |rt, input, key, _rev| {
        Box::pin(commands_execute(rt, input, key))
    });
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

fn commands_invalid_argument(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn commands_not_found(kind: &str, id: &str) -> ContractError {
    ContractError::new(
        CODE_NOT_FOUND,
        format!("{kind} `{id}` not found"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn commands_schema_mismatch(message: impl Into<String>) -> ContractError {
    ContractError::new(
        CODE_SCHEMA_MISMATCH,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn path_outside_scope(id: &str) -> ContractError {
    ContractError::new(
        CODE_PATH_OUTSIDE_SCOPE,
        format!(
            "commandId `{id}` names a path; commands are addressed by their stable id, \
             never by absolute paths or parent traversal"
        ),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn session_busy(session_id: &str) -> ContractError {
    ContractError::new(
        CODE_SESSION_BUSY,
        format!(
            "session `{session_id}` still has an active or queued run; \
             commands.execute requires an idle session"
        ),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

/// A commandId that tries to act as a filesystem path. The catalog ids are
/// `<namespace>:<relative>`; these never start with `/`, contain no `..`
/// segment (in the whole id or in the relative part behind the namespace
/// colon — `project:../escape` is a parent traversal too), and carry no
/// drive/root form.
fn command_id_is_path_shaped(id: &str) -> bool {
    fn shaped(candidate: &str) -> bool {
        candidate.starts_with('/')
            || candidate.starts_with('\\')
            || Path::new(candidate).is_absolute()
            || candidate.split(['/', '\\']).any(|segment| segment == "..")
    }
    shaped(id)
        || id
            .split_once(':')
            .is_some_and(|(_, relative)| shaped(relative))
}

// ---------------------------------------------------------------------------
// Catalog: on-disk command discovery (TS layout, v1 home/workspace mapping)
// ---------------------------------------------------------------------------

struct CommandFile {
    command_id: String,
    /// Display name (TS form: `/<relative path without .md>`).
    name: String,
    namespace: &'static str,
    /// Content-addressed revision (`cmdrev-<24 hex of sha256 of the bytes>`).
    revision: String,
    description: String,
    /// The body after the frontmatter block.
    content: String,
}

impl CommandFile {
    fn item_json(&self) -> Value {
        json!({
            "commandId": self.command_id,
            "name": self.name,
            "source": self.namespace,
            "revision": self.revision,
            "available": true,
            "reasonCode": Value::Null,
        })
    }
}

/// Splits a leading `---` frontmatter block (opening and closing fence each
/// on their own line) off the raw file. Mirrors the TS frontmatter shape;
/// a missing or unterminated block leaves the whole file as body.
fn split_frontmatter(raw: &str) -> (Option<String>, String) {
    let Some(after_open) = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
    else {
        return (None, raw.to_owned());
    };
    let mut offset = 0usize;
    for line in after_open.split_inclusive('\n') {
        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
        if trimmed == "---" {
            let block = after_open[..offset].to_owned();
            let body = after_open[offset + line.len()..].to_owned();
            return (Some(block), body);
        }
        offset += line.len();
    }
    (None, raw.to_owned())
}

/// The flat `key: value` frontmatter fields (the old line's
/// `plugins/loader/markdown.rs` precedent: scalar values only).
fn frontmatter_fields(block: &str) -> HashMap<String, String> {
    let mut fields = HashMap::new();
    for line in block.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || key == "__proto__" {
            continue;
        }
        let value = value.trim();
        let value = value
            .strip_prefix(['\'', '"'])
            .unwrap_or(value)
            .trim_end_matches(['\'', '"']);
        fields.insert(key.to_owned(), value.to_owned());
    }
    fields
}

/// The TS `firstContentLine` fallback: the first non-empty body line with
/// heading markers stripped.
fn first_content_line(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.trim_start_matches('#').trim().to_owned())
        .unwrap_or_default()
}

/// Recursively scans one command namespace (real directories and `.md`
/// files only — symlinked entries are skipped, matching the TS dirent-type
/// checks and keeping the scan inside the declared root). `root` is the
/// namespace base and stays fixed through the recursion: `dir` walks, but
/// the name keeps the full root-relative prefix (the TS
/// `path.relative(baseDir, …)`), so `deploy/rollout.md` is the command
/// `/deploy/rollout`, not `/rollout`.
fn scan_commands_dir(root: &Path, dir: &Path, namespace: &'static str, out: &mut Vec<CommandFile>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // A missing namespace directory contributes nothing (TS).
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            scan_commands_dir(root, &path, namespace, out);
            continue;
        }
        if !file_type.is_file() || !entry.file_name().to_string_lossy().ends_with(".md") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let raw = String::from_utf8_lossy(&bytes);
        let (block, body) = split_frontmatter(&raw);
        let description = block
            .as_deref()
            .map(frontmatter_fields)
            .and_then(|fields| fields.get("description").cloned())
            .filter(|description| !description.is_empty())
            .unwrap_or_else(|| first_content_line(&body));
        let relative = path
            .strip_prefix(root)
            .unwrap_or(path.as_path())
            .to_string_lossy()
            .replace('\\', "/");
        let relative = relative.strip_suffix(".md").unwrap_or(&relative).to_owned();
        out.push(CommandFile {
            command_id: format!("{namespace}:{relative}"),
            name: format!("/{relative}"),
            namespace,
            revision: format!("cmdrev-{}", &format!("{:x}", Sha256::digest(&bytes))[..24]),
            description,
            content: body,
        });
    }
}

/// The command catalog visible at one scope: the project workspace's
/// `.pilotdeck/commands` plus the instance home's `commands`, deduplicated
/// by name (project wins over user — the TS precedence).
fn scan_catalog(
    scope: &ToolScope,
    rt: &PublicV1Runtime,
) -> Result<Vec<CommandFile>, ContractError> {
    let mut files: Vec<CommandFile> = Vec::new();
    if let ToolScope::Project { project_id } | ToolScope::Session { project_id, .. } = scope
        && let Ok(workspace) = resolve_project(rt.store(), project_id)
    {
        let project_commands = workspace.workspace.join(".pilotdeck").join("commands");
        scan_commands_dir(&project_commands, &project_commands, "project", &mut files);
    }
    // A virtual project (no host-declared root) contributes no project
    // namespace, like the TS scan of a missing directory.
    let user_commands = rt.store().home().join("commands");
    scan_commands_dir(&user_commands, &user_commands, "user", &mut files);

    let mut seen: Vec<String> = Vec::new();
    files.retain(|file| {
        if seen.contains(&file.name) {
            return false;
        }
        seen.push(file.name.clone());
        true
    });
    files.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(files)
}

fn find_command<'a>(catalog: &'a [CommandFile], command_id: &str) -> Option<&'a CommandFile> {
    catalog.iter().find(|file| file.command_id == command_id)
}

// ---------------------------------------------------------------------------
// commands.list / commands.read
// ---------------------------------------------------------------------------

/// Keyset pagination cursor (`after:<name>`): resuming after the last
/// returned name has no loss or duplication even when the catalog shifts
/// between pages.
fn parse_after_cursor(input: &Value) -> Result<Option<String>, ContractError> {
    match input.get("cursor").and_then(Value::as_str) {
        None => Ok(None),
        Some(cursor) => cursor
            .strip_prefix("after:")
            .filter(|rest| !rest.is_empty())
            .map(ToOwned::to_owned)
            .map(Some)
            .ok_or_else(|| {
                commands_invalid_argument(format!(
                    "malformed cursor `{cursor}` (expected after:<name>)"
                ))
            }),
    }
}

/// The digest a catalog view's snapshotRevision pins: any id or content
/// revision change moves it.
fn catalog_snapshot_revision(files: &[CommandFile]) -> String {
    let mut hasher = Sha256::new();
    for file in files {
        hasher.update(file.command_id.as_bytes());
        hasher.update([0u8]);
        hasher.update(file.revision.as_bytes());
        hasher.update([0u8]);
    }
    format!("cmdcat-{}", &format!("{:x}", hasher.finalize())[..24])
}

async fn commands_list(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = parse_tool_scope(&rt, &input)?;
    let query = input
        .get("query")
        .and_then(Value::as_str)
        .map(str::to_lowercase);
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 200) as usize;
    let after = parse_after_cursor(&input)?;

    let catalog = scan_catalog(&scope, &rt)?;
    let snapshot_revision = catalog_snapshot_revision(&catalog);
    let mut items: Vec<Value> = Vec::new();
    for file in &catalog {
        if let Some(after) = &after
            && &file.name <= after
        {
            continue;
        }
        if let Some(query) = &query {
            let matched = file.name.to_lowercase().contains(query.as_str())
                || file.description.to_lowercase().contains(query.as_str());
            if !matched {
                continue;
            }
        }
        items.push(file.item_json());
        if items.len() > limit {
            break;
        }
    }
    let next_cursor = if items.len() > limit {
        items.pop();
        items
            .last()
            .and_then(|item| item["name"].as_str())
            .map(|last| format!("after:{last}"))
    } else {
        None
    };
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "items": items,
            "nextCursor": next_cursor,
            "snapshotRevision": snapshot_revision,
        }),
    ))
}

async fn commands_read(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let scope = parse_tool_scope(&rt, &input)?;
    let command_id = required_str(&input, "commandId")?;
    if command_id_is_path_shaped(&command_id) {
        return Err(path_outside_scope(&command_id).into());
    }
    let catalog = scan_catalog(&scope, &rt)?;
    let file = find_command(&catalog, &command_id)
        .ok_or_else(|| commands_not_found("command", &command_id))?;
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "command": file.item_json(),
            "content": file.content,
            "inputSchema": serde_json::from_str::<Value>(COMMAND_INPUT_SCHEMA)
                .unwrap_or_else(|_| json!({})),
        }),
    ))
}

// ---------------------------------------------------------------------------
// commands.execute (async, retry=idempotentSubmit)
// ---------------------------------------------------------------------------

/// The per-dataRoot domain DB (the same seam schedules/background_tasks and
/// `tools.test` accept their async operations against).
fn commands_domain_db(rt: &Arc<PublicV1Runtime>) -> Result<Arc<DomainDb>, ContractError> {
    fn registry() -> &'static Mutex<HashMap<PathBuf, Weak<DomainDb>>> {
        static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<DomainDb>>>> = OnceLock::new();
        REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
    }
    let home = rt.store().home().to_path_buf();
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(db) = reg.get(&home).and_then(Weak::upgrade) {
        return Ok(db);
    }
    let db = Arc::new(DomainDb::open(&home, &[DDL_DOMAIN_OPERATIONS])?);
    reg.insert(home, Arc::downgrade(&db));
    Ok(db)
}

/// Whether the session still has an active (non-terminal) or queued run —
/// the `commands.execute` pre-accept busy gate (mirrors
/// `session_admin::has_active_or_queued_runs`).
fn session_has_active_or_queued_runs(
    rt: &PublicV1Runtime,
    session_id: &str,
) -> Result<bool, ContractError> {
    let conn = rt.store().conn.lock().expect("sqlite mutex");
    let active: i64 = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM runs WHERE session_id = ?1 AND terminal = 0)
             OR EXISTS(SELECT 1 FROM run_queue WHERE session_id = ?1)",
            params![session_id],
            |row| row.get(0),
        )
        .map_err(|error| {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!("sqlite storage error: {error}"),
                RetrySpec::backoff_read(Some(250)),
                EffectStatus::Unknown,
            )
        })?;
    Ok(active != 0)
}

/// Replaces every occurrence of `$<n>` (the digit run exactly `n`, followed
/// by a non-word character or end of string — the `\\$n\\b` boundary) with
/// `value`, the way the TS positional regex does.
fn replace_positional(content: &str, index: usize, value: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let bytes = content.as_bytes();
    let mut offset = 0usize;
    while offset < bytes.len() {
        if bytes[offset] == b'$' {
            let mut digits_end = offset + 1;
            while digits_end < bytes.len() && bytes[digits_end].is_ascii_digit() {
                digits_end += 1;
            }
            if digits_end > offset + 1 {
                let digits = &content[offset + 1..digits_end];
                let boundary = digits_end == bytes.len()
                    || !content[digits_end..]
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
                if boundary && digits.parse::<usize>().is_ok_and(|parsed| parsed == index) {
                    out.push_str(value);
                    offset = digits_end;
                    continue;
                }
            }
        }
        let char_end = offset + content[offset..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&content[offset..char_end]);
        offset = char_end;
    }
    out
}

/// The TS `/api/commands/execute` substitution, in its exact order:
/// `$ARGUMENTS` first (the space-joined args), then `$1..$N` positionally
/// over the evolving string (an argument value containing `$2` is itself
/// substituted when its index is reached — the baseline quirk, preserved).
fn prepare_content(body: &str, args: &[String]) -> String {
    let joined = args.join(" ");
    let mut content = body.replace("$ARGUMENTS", &joined);
    for (index, arg) in args.iter().enumerate() {
        content = replace_positional(&content, index + 1, arg);
    }
    content
}

async fn commands_execute(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let session_id = required_str(&input, "sessionId")?;
    let command_id = required_str(&input, "commandId")?;
    let expected_revision = required_str(&input, "expectedRevision")?;
    let schema_id = required_str(&input, "schemaId")?;
    let arguments = input
        .get("arguments")
        .cloned()
        .ok_or_else(|| commands_invalid_argument("`arguments` is required"))?;

    let session = rt
        .store()
        .get_session(&session_id)
        .map_err(|_| commands_not_found("session", &session_id))?;
    let session_json = &session["session"];
    match session_json.get("sessionState").and_then(Value::as_str) {
        Some("deleted") => return Err(commands_not_found("session", &session_id).into()),
        Some("closed") => {
            return Err(commands_invalid_argument(format!(
                "session `{session_id}` is closed; reopen it before executing commands"
            ))
            .into());
        }
        _ => {}
    }
    let Some(project_id) = session_json.get("projectId").and_then(Value::as_str) else {
        return Err(
            commands_invalid_argument(format!("session `{session_id}` has no project")).into(),
        );
    };
    let scope = ToolScope::Session {
        project_id: project_id.to_owned(),
        session_id: session_id.clone(),
    };
    let catalog = scan_catalog(&scope, &rt)?;
    let file = find_command(&catalog, &command_id)
        .ok_or_else(|| commands_not_found("command", &command_id))?;
    let db = commands_domain_db(&rt)?;

    // Captured plain values for the inline execution below.
    let file_revision = file.revision.clone();
    let file_body = file.content.clone();
    let file_id = command_id.clone();

    let validation = || -> Result<(), ContractError> {
        if file_revision != expected_revision {
            return Err(ContractError::new(
                CODE_REVISION_CONFLICT,
                format!(
                    "command `{file_id}` revision `{file_revision}` does not match the \
                     requested expectedRevision `{expected_revision}`"
                ),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        if schema_id != COMMAND_INPUT_SCHEMA_ID {
            return Err(commands_schema_mismatch(format!(
                "schemaId `{schema_id}` does not match the registered command input \
                 schema `{COMMAND_INPUT_SCHEMA_ID}`"
            )));
        }
        let input_schema = serde_json::from_str::<Value>(COMMAND_INPUT_SCHEMA).unwrap_or(json!({}));
        if let Err(errors) = validate_against_schema(&arguments, &input_schema) {
            let details = errors
                .iter()
                .map(|error| json!({"path": error.path, "message": error.message}))
                .collect::<Vec<_>>();
            let mut error =
                commands_schema_mismatch("arguments do not satisfy the command input schema");
            let mut map = serde_json::Map::new();
            map.insert("validationErrors".to_owned(), json!(details));
            error.details = Some(map);
            return Err(error);
        }
        if session_has_active_or_queued_runs(&rt, &session_id)? {
            return Err(session_busy(&session_id));
        }
        Ok(())
    };

    let operation = accept_domain_operation(
        &db,
        "commands.execute",
        &input,
        idempotency_key.as_deref(),
        || format!("op-{}", rt.store().next_id()),
        validation,
    )?;
    // A deduplicated replay answers with the stored receipt and must NOT
    // re-execute: the original acceptance already submitted its run and
    // terminalized the operation (the module note above — "a dropped
    // connection neither cancels nor re-executes the command"), so falling
    // through would submit a second run and overwrite the completion.
    if operation.receipt["deduplicated"].as_bool() == Some(true) {
        return Ok((ResponseOutcome::Accepted, operation.receipt));
    }
    let operation_id = operation.operation_id.clone();

    // Inline execution: prepare the content (pure string substitution —
    // argument values never reach a shell), submit the run to the session,
    // then terminalize the operation. Everything is durable before the
    // receipt is answered; a dropped connection neither cancels nor
    // re-executes the command (the idempotency record replays the receipt).
    let _ = mark_domain_operation_running(&db, &operation_id, &session_id);
    let args: Vec<String> = arguments["args"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let prepared = prepare_content(&file_body, &args);
    let run_input = json!({
        "sessionId": session_id,
        "input": [{"kind": "text", "text": prepared}],
    });
    let submission = rt.store().submit_run(&run_input, None);
    match submission {
        Ok(SubmitDisposition::NewRun {
            run_id,
            start_immediately,
            ..
        }) => {
            if start_immediately {
                rt.start_session_worker(run_id.clone());
            }
            let run_state = rt
                .store()
                .get_run(&run_id)
                .map(|run| run.state.as_str())
                .unwrap_or("queued");
            let _ = finish_domain_operation(
                &db,
                &operation_id,
                &session_id,
                "succeeded",
                &json!({
                    "invocationId": operation_id,
                    "commandId": file_id,
                    "runId": run_id,
                    "resultSchemaId": COMMAND_EXEC_RESULT_SCHEMA_ID,
                    "value": {"runId": run_id, "runState": run_state},
                }),
            );
        }
        // Unreachable with a None idempotency key (no dedup path).
        Ok(SubmitDisposition::Deduplicated { .. }) => {}
        Err(error) => {
            let _ = finish_domain_operation(
                &db,
                &operation_id,
                &session_id,
                "failed",
                &json!({"code": error.code, "message": error.message}),
            );
        }
    }
    Ok((ResponseOutcome::Accepted, operation.receipt))
}

// ---------------------------------------------------------------------------
// Tests (contract positive/negative cases against the real dispatch table)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::store::{InstanceMeta, V1Store, now_rfc3339};
    use crate::public_v1::test_support::TempDir;
    use agentcpm_pd_protocol::registry::OperationRegistry;
    use rusqlite::params;
    use std::time::{Duration, Instant};

    fn fresh_home(tag: &str) -> (TempDir, PathBuf) {
        let tmp = TempDir::new(tag);
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        (tmp, root_dir)
    }

    fn open_runtime(root_dir: &Path) -> (Arc<PublicV1Runtime>, String) {
        let (file, _token) = load_local_instance_and_token(root_dir, None).unwrap();
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
        let rt = PublicV1Runtime::new(store);
        (rt, file.instance_id.clone())
    }

    /// 播种一个带真实授权根的项目（files.rs 测试同款）并返回工作区路径。
    fn seed_project(rt: &Arc<PublicV1Runtime>, root_dir: &Path, project_id: &str) -> PathBuf {
        let store = rt.store();
        let declared_root = root_dir.parent().unwrap().join("authorized-cmd");
        std::fs::create_dir_all(&declared_root).unwrap();
        let workspace_path = declared_root.join("ws");
        std::fs::create_dir_all(&workspace_path).unwrap();
        {
            let conn = store.conn.lock().expect("sqlite mutex");
            conn.execute(
                "INSERT INTO authorized_roots (root_id, instance_id, absolute_path, created_at)
                 VALUES ('root-t017c', ?1, ?2, ?3)",
                params![
                    store.meta().instance_id,
                    declared_root.display().to_string(),
                    now_rfc3339(),
                ],
            )
            .unwrap();
        }
        rt.store()
            .register_project(&json!({
                "projectId": project_id,
                "name": "T017 Commands",
                "environmentId": "local",
                "root": {"instanceId": store.meta().instance_id, "rootId": "root-t017c", "relativePath": "ws"},
            }))
            .unwrap();
        workspace_path
    }

    fn seed_command(dir: &Path, relative: &str, body: &str) {
        let path = dir.join(format!("{relative}.md"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
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

    async fn wait_operation(rt: &Arc<PublicV1Runtime>, operation_id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let (_, page) = rt
                .dispatch(
                    "operations.get",
                    json!({"operationId": operation_id}),
                    None,
                    None,
                )
                .await
                .unwrap();
            let operation = page["operation"].clone();
            let state = operation["state"].as_str().unwrap_or_default().to_owned();
            if matches!(
                state.as_str(),
                "succeeded" | "failed" | "cancelled" | "interrupted" | "result_unknown"
            ) {
                return operation;
            }
            assert!(
                Instant::now() < deadline,
                "operation `{operation_id}` never became terminal (state `{state}`)"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[test]
    fn substitution_follows_the_baseline_regex_semantics() {
        // $ARGUMENTS is the space-joined args; $n needs the digit run to be
        // exactly n at a word boundary (`\$n\b`).
        assert_eq!(
            prepare_content("echo $ARGUMENTS", &["a".to_owned(), "b".to_owned()]),
            "echo a b"
        );
        assert_eq!(
            prepare_content("$1 and $2", &["one".to_owned(), "two".to_owned()]),
            "one and two"
        );
        assert_eq!(
            prepare_content("$1abc $10 $1.", &["one".to_owned(), "two".to_owned()]),
            "$1abc $10 one.",
            "`$1abc` has no boundary and `$10` needs a 10th argument"
        );
        // Empty args: `$ARGUMENTS` is still replaced (the space-joined empty
        // string), but the per-argument `$1..$N` loop never runs, so `$1`
        // survives verbatim — the TS `/api/commands/execute` order
        // (`replace` first, then a per-arg `forEach`) substitutes no
        // position that has no argument.
        assert_eq!(
            prepare_content("no arguments: [$ARGUMENTS][$1]", &[]),
            "no arguments: [][$1]"
        );
        // The TS sequential-replace quirk: an argument value containing a
        // later positional token is itself substituted.
        assert_eq!(
            prepare_content("$1", &["$2".to_owned(), "inner".to_owned()]),
            "inner"
        );
    }

    #[tokio::test]
    async fn commands_list_discovers_and_deduplicates() {
        let (_tmp, root) = fresh_home("commands-list");
        let (rt, _iid) = open_runtime(&root);
        let workspace = seed_project(&rt, &root, "proj-cmd");
        rt.store()
            .create_session(&json!({"sessionId": "sess-cmd", "projectId": "proj-cmd"}))
            .unwrap();

        seed_command(
            &workspace.join(".pilotdeck").join("commands"),
            "deploy/rollout",
            "---\ndescription: Roll the deployment out\n---\nRun the rollout for $ARGUMENTS.",
        );
        // Same name in both namespaces: project wins over user.
        seed_command(
            &root.join("commands"),
            "hello",
            "---\ndescription: User hello\n---\nHello user.",
        );
        seed_command(
            &workspace.join(".pilotdeck").join("commands"),
            "hello",
            "Hello project.",
        );

        let session_scope = json!({"scope": {"kind": "session", "id": "sess-cmd"}});
        let (_, result) = rt
            .dispatch("commands.list", session_scope, None, None)
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("commands.list", &result)
            .unwrap();
        let items = result["items"].as_array().cloned().unwrap_or_default();
        let names: Vec<String> = items
            .iter()
            .filter_map(|item| item["name"].as_str().map(ToOwned::to_owned))
            .collect();
        assert_eq!(names, vec!["/deploy/rollout", "/hello"]);
        let hello = items
            .iter()
            .find(|item| item["name"] == json!("/hello"))
            .unwrap();
        assert_eq!(hello["source"], json!("project"), "project wins the dedupe");
        assert_eq!(hello["available"], json!(true));
        assert_eq!(hello["reasonCode"], Value::Null);
        assert!(
            hello["revision"]
                .as_str()
                .is_some_and(|r| r.starts_with("cmdrev-"))
        );
        assert!(
            result["snapshotRevision"]
                .as_str()
                .is_some_and(|r| r.starts_with("cmdcat-"))
        );

        // The global scope sees only user commands (no project workspace).
        let (_, global) = rt
            .dispatch(
                "commands.list",
                json!({"scope": {"kind": "global"}}),
                None,
                None,
            )
            .await
            .unwrap();
        let global_names: Vec<String> = global["items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item["name"].as_str().map(ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(global_names, vec!["/hello"]);
        let global_hello = global["items"]
            .as_array()
            .and_then(|items| items.first())
            .cloned()
            .unwrap_or(Value::Null);
        assert_eq!(global_hello["source"], json!("user"));

        // The query filter matches names and descriptions.
        let (_, queried) = rt
            .dispatch(
                "commands.list",
                json!({"scope": {"kind": "session", "id": "sess-cmd"}, "query": "ROLLOUT"}),
                None,
                None,
            )
            .await
            .unwrap();
        let queried_names: Vec<String> = queried["items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item["name"].as_str().map(ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(queried_names, vec!["/deploy/rollout"]);

        // Pagination over the same view is lossless and duplicate-free.
        let mut seen: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut input = json!({"scope": {"kind": "session", "id": "sess-cmd"}, "limit": 1});
            if let Some(cursor) = &cursor {
                input["cursor"] = json!(cursor);
            }
            let (_, page) = rt
                .dispatch("commands.list", input, None, None)
                .await
                .unwrap();
            seen.extend(
                page["items"]
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item["name"].as_str().map(ToOwned::to_owned))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
            );
            match page["nextCursor"].as_str() {
                Some(next) => cursor = Some(next.to_owned()),
                None => break,
            }
        }
        assert_eq!(seen, names, "paginated walk must match the full list");

        let error = dispatch_err(
            &rt,
            "commands.list",
            json!({"scope": {"kind": "global"}, "cursor": "junk"}),
            None,
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");
    }

    #[tokio::test]
    async fn commands_read_returns_content_and_rejects_path_shaped_ids() {
        let (_tmp, root) = fresh_home("commands-read");
        let (rt, _iid) = open_runtime(&root);
        let workspace = seed_project(&rt, &root, "proj-read");
        seed_command(
            &workspace.join(".pilotdeck").join("commands"),
            "greet",
            "---\ndescription: Greet\n---\nHello $1, all: $ARGUMENTS",
        );

        let (_, result) = rt
            .dispatch(
                "commands.read",
                json!({
                    "scope": {"kind": "project", "id": "proj-read"},
                    "commandId": "project:greet",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("commands.read", &result)
            .unwrap();
        assert_eq!(result["command"]["commandId"], json!("project:greet"));
        assert_eq!(result["content"], json!("Hello $1, all: $ARGUMENTS"));
        assert_eq!(
            result["inputSchema"]["properties"]["args"]["type"],
            json!("array")
        );

        // Path-shaped ids are rejected explicitly.
        for bad in ["/etc/passwd", "project:../escape", "user:../../home/x"] {
            let error = dispatch_err(
                &rt,
                "commands.read",
                json!({
                    "scope": {"kind": "project", "id": "proj-read"},
                    "commandId": bad,
                }),
                None,
            )
            .await;
            assert_eq!(error.code, "PATH_OUTSIDE_SCOPE", "id `{bad}`");
        }

        let error = dispatch_err(
            &rt,
            "commands.read",
            json!({
                "scope": {"kind": "project", "id": "proj-read"},
                "commandId": "project:none",
            }),
            None,
        )
        .await;
        assert_eq!(error.code, "NOT_FOUND");
    }

    #[tokio::test]
    async fn commands_execute_submits_the_prepared_run() {
        let (_tmp, root) = fresh_home("commands-exec");
        let (rt, _iid) = open_runtime(&root);
        let workspace = seed_project(&rt, &root, "proj-exec");
        rt.store()
            .create_session(&json!({"sessionId": "sess-exec", "projectId": "proj-exec"}))
            .unwrap();
        seed_command(
            &workspace.join(".pilotdeck").join("commands"),
            "greet",
            "---\ndescription: Greet\n---\nHello $1! Args: [$ARGUMENTS]",
        );

        // Discover the current revision through the catalog.
        let (_, listed) = rt
            .dispatch(
                "commands.list",
                json!({"scope": {"kind": "session", "id": "sess-exec"}}),
                None,
                None,
            )
            .await
            .unwrap();
        let revision = listed["items"]
            .as_array()
            .and_then(|items| items.first())
            .and_then(|item| item["revision"].as_str())
            .unwrap_or_default()
            .to_owned();

        let input = json!({
            "sessionId": "sess-exec",
            "commandId": "project:greet",
            "expectedRevision": revision,
            "schemaId": COMMAND_INPUT_SCHEMA_ID,
            "arguments": {"args": ["world", "extra"]},
        });
        let (_, receipt) = rt
            .dispatch("commands.execute", input.clone(), Some("key-exec-1"), None)
            .await
            .unwrap();
        assert_eq!(receipt["kind"], json!("operation"));
        let operation =
            wait_operation(&rt, receipt["operationId"].as_str().unwrap_or_default()).await;
        assert_eq!(operation["state"], json!("succeeded"));
        let completion = &operation["result"];
        assert_eq!(completion["commandId"], json!("project:greet"));
        assert_eq!(
            completion["resultSchemaId"],
            json!(COMMAND_EXEC_RESULT_SCHEMA_ID)
        );
        let run_id = completion["runId"].as_str().unwrap().to_owned();

        // The result traces to the actual run, whose input carries the
        // prepared (substituted) content.
        let run = rt.store().get_run(&run_id).unwrap();
        assert_eq!(run.session_id, "sess-exec");
        let prepared = run.input["input"][0]["text"].as_str().unwrap_or_default();
        assert_eq!(prepared, "Hello world! Args: [world extra]");

        // Pre-accept rejections: stale revision, wrong schema, bad arguments.
        let mut stale = input.clone();
        stale["expectedRevision"] = json!("cmdrev-deadbeefdeadbeefdeadbeef");
        let error = dispatch_err(&rt, "commands.execute", stale, Some("key-exec-stale")).await;
        assert_eq!(error.code, "REVISION_CONFLICT");

        let mut wrong_schema = input.clone();
        wrong_schema["schemaId"] = json!("agentcpm-pd.command.input/2");
        let error = dispatch_err(
            &rt,
            "commands.execute",
            wrong_schema,
            Some("key-exec-schema"),
        )
        .await;
        assert_eq!(error.code, "SCHEMA_MISMATCH");

        let mut bad_args = input.clone();
        bad_args["arguments"] = json!({"args": "not-an-array"});
        let error = dispatch_err(&rt, "commands.execute", bad_args, Some("key-exec-args")).await;
        assert_eq!(error.code, "SCHEMA_MISMATCH");

        // Unknown session / command.
        let mut no_session = input.clone();
        no_session["sessionId"] = json!("sess-none");
        let error = dispatch_err(&rt, "commands.execute", no_session, Some("key-exec-sess")).await;
        assert_eq!(error.code, "NOT_FOUND");
        let mut no_command = input.clone();
        no_command["commandId"] = json!("project:none");
        let error = dispatch_err(&rt, "commands.execute", no_command, Some("key-exec-cmd")).await;
        assert_eq!(error.code, "NOT_FOUND");

        // The same key replays the accepted receipt; a different input
        // conflicts.
        let (_, replay) = rt
            .dispatch("commands.execute", input.clone(), Some("key-exec-1"), None)
            .await
            .unwrap();
        assert_eq!(replay["deduplicated"], json!(true));
        // The replay is terminal on arrival: it must not submit a second
        // run (the runs row count stays at one) and must not disturb the
        // operation's original completion.
        let session_runs: i64 = {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.query_row(
                "SELECT COUNT(*) FROM runs WHERE session_id = ?1",
                params!["sess-exec"],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            session_runs, 1,
            "an idempotent replay must not submit a second run"
        );
        let replayed =
            wait_operation(&rt, receipt["operationId"].as_str().unwrap_or_default()).await;
        assert_eq!(replayed["state"], json!("succeeded"));
        assert_eq!(
            replayed["result"]["runId"],
            json!(run_id),
            "the replayed operation keeps its original completion"
        );
        let mut other = input.clone();
        other["arguments"] = json!({"args": ["different"]});
        let error = dispatch_err(&rt, "commands.execute", other, Some("key-exec-1")).await;
        assert_eq!(error.code, "IDEMPOTENCY_CONFLICT");
    }

    #[tokio::test]
    async fn commands_execute_never_passes_arguments_through_a_shell() {
        let (_tmp, root) = fresh_home("commands-exec-injection");
        let (rt, _iid) = open_runtime(&root);
        let workspace = seed_project(&rt, &root, "proj-inject");
        rt.store()
            .create_session(&json!({"sessionId": "sess-inject", "projectId": "proj-inject"}))
            .unwrap();
        seed_command(
            &workspace.join(".pilotdeck").join("commands"),
            "raw",
            "Take this: $ARGUMENTS",
        );

        let (_, listed) = rt
            .dispatch(
                "commands.list",
                json!({"scope": {"kind": "session", "id": "sess-inject"}}),
                None,
                None,
            )
            .await
            .unwrap();
        let revision = listed["items"]
            .as_array()
            .and_then(|items| items.first())
            .and_then(|item| item["revision"].as_str())
            .unwrap_or_default()
            .to_owned();

        let malicious = "$(touch pwned.txt); `touch pwned2.txt`; rm -rf /";
        let (_, receipt) = rt
            .dispatch(
                "commands.execute",
                json!({
                    "sessionId": "sess-inject",
                    "commandId": "project:raw",
                    "expectedRevision": revision,
                    "schemaId": COMMAND_INPUT_SCHEMA_ID,
                    "arguments": {"args": [malicious]},
                }),
                Some("key-inject-1"),
                None,
            )
            .await
            .unwrap();
        let operation =
            wait_operation(&rt, receipt["operationId"].as_str().unwrap_or_default()).await;
        assert_eq!(operation["state"], json!("succeeded"));
        let run_id = operation["result"]["runId"].as_str().unwrap().to_owned();
        let run = rt.store().get_run(&run_id).unwrap();
        let prepared = run.input["input"][0]["text"].as_str().unwrap_or_default();
        assert_eq!(
            prepared,
            format!("Take this: {malicious}"),
            "the argument value lands verbatim in the prepared content"
        );
        assert!(
            !workspace.join("pwned.txt").exists() && !workspace.join("pwned2.txt").exists(),
            "argument values must never be executed as shell"
        );
    }

    #[tokio::test]
    async fn commands_execute_rejects_busy_and_closed_sessions() {
        let (_tmp, root) = fresh_home("commands-exec-busy");
        let (rt, _iid) = open_runtime(&root);
        let workspace = seed_project(&rt, &root, "proj-busy");
        rt.store()
            .create_session(&json!({"sessionId": "sess-busy", "projectId": "proj-busy"}))
            .unwrap();
        rt.store()
            .create_session(&json!({"sessionId": "sess-closed", "projectId": "proj-busy"}))
            .unwrap();
        seed_command(
            &workspace.join(".pilotdeck").join("commands"),
            "ping",
            "ping $ARGUMENTS",
        );
        let (_, listed) = rt
            .dispatch(
                "commands.list",
                json!({"scope": {"kind": "session", "id": "sess-busy"}}),
                None,
                None,
            )
            .await
            .unwrap();
        let revision = listed["items"]
            .as_array()
            .and_then(|items| items.first())
            .and_then(|item| item["revision"].as_str())
            .unwrap_or_default()
            .to_owned();
        let input = json!({
            "sessionId": "sess-busy",
            "commandId": "project:ping",
            "expectedRevision": revision,
            "schemaId": COMMAND_INPUT_SCHEMA_ID,
            "arguments": {"args": []},
        });

        // A held-open run makes the session busy: the command is refused
        // before accept (SESSION_BUSY is retryable, so the key is not pinned).
        let (_, held) = rt
            .dispatch(
                "run.submit",
                json!({
                    "sessionId": "sess-busy",
                    "input": [{"kind": "text", "text": "[pd-driver:sleep=1500]"}],
                }),
                Some("key-hold-run"),
                None,
            )
            .await
            .unwrap();
        let held_run = held["runId"].as_str().unwrap().to_owned();
        let error = dispatch_err(&rt, "commands.execute", input.clone(), Some("key-busy-1")).await;
        assert_eq!(error.code, "SESSION_BUSY");

        // Once the session is idle again, the same key executes.
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let run = rt.store().get_run(&held_run).unwrap();
            if run.terminal {
                break;
            }
            assert!(Instant::now() < deadline, "held run never finished");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let (_, receipt) = rt
            .dispatch("commands.execute", input, Some("key-busy-1"), None)
            .await
            .unwrap();
        assert_eq!(receipt["deduplicated"], json!(false));
        wait_operation(&rt, receipt["operationId"].as_str().unwrap_or_default()).await;

        // A closed session refuses execution with a clear error.
        let (_, session) = rt
            .dispatch(
                "session.get",
                json!({"sessionId": "sess-closed"}),
                None,
                None,
            )
            .await
            .unwrap();
        let session_rev = session["session"]["revision"].as_str().unwrap().to_owned();
        rt.dispatch(
            "session.close",
            json!({"sessionId": "sess-closed", "expectedRevision": session_rev}),
            None,
            None,
        )
        .await
        .unwrap();
        let error = dispatch_err(
            &rt,
            "commands.execute",
            json!({
                "sessionId": "sess-closed",
                "commandId": "project:ping",
                "expectedRevision": revision,
                "schemaId": COMMAND_INPUT_SCHEMA_ID,
                "arguments": {"args": []},
            }),
            Some("key-closed-1"),
        )
        .await;
        assert_eq!(error.code, "INVALID_ARGUMENT");
    }
}
