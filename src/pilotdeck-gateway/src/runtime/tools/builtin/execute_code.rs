//! `execute_code` (baseline `tool/builtin/executeCode.ts`): runs a
//! model-authored Python 3 script as a detached child process. The script can
//! call an allow-list of tools back through a private Unix-socket RPC bridge
//! (`import pilotdeck_tools`); every such call goes through the tool runtime's
//! recursion hook ([`ToolRunContext::execute_tool`]), so it is validated,
//! permission-checked and audited like a model tool call.
//!
//! Process ownership: the interpreter leads its own session (`detached: true`
//! in the baseline). Timeout and abort send SIGTERM to the whole group and
//! SIGKILL 500 ms later (`NodeCodeRuntimePort.ts:207-222`). The run settles
//! when the interpreter has exited and both pipes reached EOF.
//!
//! Deliberate deviation (security review): a descendant that leaves the
//! group (`setsid`, `start_new_session=True`) and keeps the inherited
//! stdout/stderr open. The baseline only signals the group and resolves at
//! stream `end`, so for the same input it hangs until that descendant exits —
//! past `timeout_seconds`, and with no bound at all once the interpreter exits
//! normally, because the exit handler clears the timeout
//! (`NodeCodeRuntimePort.ts:125-139,207-222`). Rust deliberately does not
//! reproduce that:
//! - once the interpreter has exited, the pipes get [`PIPE_GRACE`] to reach
//!   EOF; then the run settles with what was read (later output is dropped);
//! - on timeout/abort the interpreter's process tree is snapshotted before
//!   the group is signalled, and every descendant found there gets the same
//!   SIGTERM / SIGKILL as the group, so a detached descendant alive at that
//!   moment is reaped too. Best effort: one that has already been re-parented
//!   (e.g. left behind by a script that exited normally), or a platform
//!   without `/proc` or `ps`, is not found; the grace period still bounds the
//!   call. PIDs are signalled from that one snapshot, so a PID reused within
//!   the 500 ms escalation window could be hit;
//! - after a timeout/abort the call returns at most [`SETTLE_BOUND`] after
//!   the SIGKILL even if the interpreter itself never exits.
//!
//! A script without such a descendant sees EOF at exit, so its output, status
//! and duration are unchanged.
//!
//! Other differences from the baseline, all outside what a well-formed script
//! can observe:
//! - RPC connections are tasks owned by the tool future. When the script is
//!   gone (exit, timeout, abort) an in-flight nested call is dropped; the
//!   baseline leaves it running detached and discards its answer.
//! - After a timeout/abort that settles before the 500 ms SIGKILL is due, the
//!   group is killed at settle time instead of on that timer.
//! - Only the Unix-socket transport exists. The baseline's TCP transport is
//!   its Windows path; this module is Unix-only like `bash`.
//! - A request line that parses to JSON `null` crashes the baseline handler
//!   (`request.tool` on null); here it is answered like any other request
//!   without a tool name.
#![cfg(unix)]
use super::{
    bash::{is_read_only_shell_command, spawn_env},
    cwd, field, object, schema, string, text,
};
use crate::{
    host::{
        HostError,
        modules::capability::{ToolCall, ToolDefinition},
    },
    js_value,
    node_io::node_tmpdir,
    process_group::GroupGuard,
    runtime::tools::{
        ExecutableTool, NestedToolExecutor, ToolError, ToolFuture, ToolOutput, ToolRunContext,
        js_space, scheduler::content_text,
    },
};
use pilotdeck_protocol::{JsObject, JsString, JsValue, parse_json};
use regex::Regex;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    process::Command,
    task::JoinSet,
};

const DEFAULT_TIMEOUT_SECONDS: f64 = 300.0;
const DEFAULT_MAX_TOOL_CALLS: f64 = 50.0;
const MAX_STDOUT_BYTES: usize = 50_000;
const MAX_STDERR_BYTES: usize = 10_000;
/// `EXECUTE_CODE_BASE_ALLOWED_TOOLS` (executeCode.ts:126-134).
const BASE_ALLOWED_TOOLS: [&str; 7] = [
    "web_fetch",
    "read_file",
    "write_file",
    "edit_file",
    "grep",
    "glob",
    "bash",
];
/// `killProcess(child, true)`'s escalation delay.
const KILL_AFTER: Duration = Duration::from_millis(500);
/// How long the pipes may stay open after the interpreter has exited. A
/// deliberate deviation: the baseline waits for EOF without a bound (see the
/// module docs).
const PIPE_GRACE: Duration = Duration::from_millis(200);
/// Upper bound between the escalated SIGKILL and settling, for an
/// interpreter that does not exit even then.
const SETTLE_BOUND: Duration = Duration::from_secs(2);

const INPUT_SCHEMA: &str = r#"{"type":"object","required":["code"],"additionalProperties":false,"properties":{"code":{"type":"string","description":"Python 3 source code to execute. Use `from pilotdeck_tools import ...` to call allowed PilotDeck tools."},"description":{"type":"string","description":"Optional human-readable note; ignored by execution."},"timeout_seconds":{"type":"integer","description":"Maximum execution time in seconds. Defaults to 300; maximum 300."},"max_tool_calls":{"type":"integer","description":"Maximum number of PilotDeck tool calls the script may make. Defaults to 50; maximum 50."}}}"#;

// The Python helper module written next to the script as `pilotdeck_tools.py`.
// Verbatim from `resources/PilotDeck/src/tool/builtin/executeCode.ts`: the
// transport header is `UDS_PYTHON_TRANSPORT_HEADER` (lines 732-765), the
// helpers are the template literals of `generatePilotDeckToolsModule`
// (lines 658-727). The module is `${header}\n${helpers...}` (lines 728-729).

/// executeCode.ts:732-765.
const UDS_PYTHON_TRANSPORT_HEADER: &str = r#""""Auto-generated PilotDeck execute_code RPC helpers."""
import json
import os
import shlex
import socket

_sock = None


def _connect():
    global _sock
    if _sock is None:
        _sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        _sock.connect(os.environ["PILOTDECK_RPC_SOCKET"])
        _sock.settimeout(300)
    return _sock


def _call(tool_name, args):
    conn = _connect()
    conn.sendall((json.dumps({"tool": tool_name, "args": args}) + "\n").encode("utf-8"))
    chunks = []
    while True:
        chunk = conn.recv(65536)
        if not chunk:
            raise RuntimeError("PilotDeck RPC server disconnected")
        chunks.append(chunk)
        if chunk.endswith(b"\n"):
            break
    response = json.loads(b"".join(chunks).decode("utf-8").strip())
    if response.get("error"):
        raise RuntimeError(response.get("error"))
    return response
"#;

/// executeCode.ts:658-665.
const WEB_SEARCH_HELPER: &str = r#"
def web_search(query, country=None):
    args = {"query": query}
    if country is not None:
        args["gl"] = country
    return _call("web_search", args)

"#;

/// executeCode.ts:666-675.
const WEB_FETCH_HELPER: &str = r#"
def web_fetch(url, mode=None, prompt=None):
    args = {"url": url}
    if mode is not None:
        args["mode"] = mode
    if prompt is not None:
        args["prompt"] = prompt
    return _call("web_fetch", args)

"#;

/// executeCode.ts:676-685.
const READ_FILE_HELPER: &str = r#"
def read_file(file_path, offset=0, limit=None):
    args = {"file_path": file_path}
    if offset is not None and offset > 0:
        args["offset"] = offset
    if limit is not None:
        args["limit"] = limit
    return _call("read_file", args)

"#;

/// executeCode.ts:686-690.
const WRITE_FILE_HELPER: &str = r#"
def write_file(file_path, content):
    return _call("write_file", {"file_path": file_path, "content": content})

"#;

/// executeCode.ts:691-700.
const EDIT_FILE_HELPER: &str = r#"
def edit_file(file_path, old_string, new_string, replace_all=False):
    return _call("edit_file", {
        "file_path": file_path,
        "old_string": old_string,
        "new_string": new_string,
        "replace_all": replace_all,
    })

"#;

/// executeCode.ts:701-710.
const GREP_HELPER: &str = r#"
def grep(pattern, path=None, glob=None):
    args = {"pattern": pattern}
    if path is not None:
        args["path"] = path
    if glob is not None:
        args["glob"] = glob
    return _call("grep", args)

"#;

/// executeCode.ts:711-718.
const GLOB_HELPER: &str = r#"
def glob(pattern, path=None):
    args = {"pattern": pattern}
    if path is not None:
        args["path"] = path
    return _call("glob", args)

"#;

/// executeCode.ts:719-727.
const BASH_HELPER: &str = r#"
def bash(command, timeout_ms=None, workdir=None):
    args = {"command": command}
    if workdir is not None:
        args["command"] = "cd " + shlex.quote(workdir) + " && " + command
    if timeout_ms is not None:
        args["timeout"] = timeout_ms
    return _call("bash", args)
"#;

/// `generatePilotDeckToolsModule("uds", allowedTools)`.
fn pilotdeck_tools_module(allowed: &[&str]) -> String {
    if allowed.is_empty() {
        return "\"\"\"No PilotDeck helper RPC is available in this strict host sandbox.\"\"\"\n"
            .into();
    }
    let mut module = format!("{UDS_PYTHON_TRANSPORT_HEADER}\n");
    for (name, helper) in [
        ("web_search", WEB_SEARCH_HELPER),
        ("web_fetch", WEB_FETCH_HELPER),
        ("read_file", READ_FILE_HELPER),
        ("write_file", WRITE_FILE_HELPER),
        ("edit_file", EDIT_FILE_HELPER),
        ("grep", GREP_HELPER),
        ("glob", GLOB_HELPER),
        ("bash", BASH_HELPER),
    ] {
        if allowed.contains(&name) {
            module.push_str(helper);
        }
    }
    module
}

/// `CreateExecuteCodeToolOptions`, reduced to what this host selects. The
/// provider seams (workspace, code runtime, transport) are this module's own
/// implementations.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExecuteCodeToolOptions {
    /// `webSearch !== false`: adds the `web_search` helper and RPC capability.
    pub web_search: bool,
    /// `sandbox` (the execution world's `executeCodeSandbox` / sandboxed code
    /// runtime): a constrained mode confines the interpreter and its probe,
    /// and drops the host-side mutation helpers from the RPC surface.
    pub sandbox: crate::runtime::tools::sandbox::Sandbox,
}

pub struct ExecuteCodeTool {
    fields: JsObject,
    allowed: Arc<Vec<&'static str>>,
    /// `restrictSandboxedRpcTools(allowedTools, policy)`.
    rpc_allowed: Arc<Vec<&'static str>>,
    sandbox: crate::runtime::tools::sandbox::Sandbox,
}

impl ExecuteCodeTool {
    pub fn new(options: ExecuteCodeToolOptions) -> Self {
        // `resolveExecuteCodeAllowedTools` and `availableHelpers`.
        let mut helpers: Vec<&'static str> = Vec::new();
        if options.web_search {
            helpers.push("web_search");
        }
        helpers.extend(BASE_ALLOWED_TOOLS);
        // These run in the host, outside the confined interpreter, so a
        // constrained policy must not let RPC become an escape hatch
        // (executeCode.ts:506-518).
        let rpc_allowed: Vec<&'static str> = helpers
            .iter()
            .copied()
            .filter(|name| {
                !options.sandbox.confines() || !matches!(*name, "bash" | "write_file" | "edit_file")
            })
            .collect();
        Self {
            fields: object([
                ("name", string("execute_code")),
                (
                    "description",
                    string(description(&helpers, options.sandbox.mode())),
                ),
                ("kind", string("custom")),
                ("inputSchema", JsValue::Object(schema(INPUT_SCHEMA))),
            ]),
            allowed: Arc::new(helpers),
            rpc_allowed: Arc::new(rpc_allowed),
            sandbox: options.sandbox,
        }
    }
}

/// The description `createExecuteCodeTool` assembles (executeCode.ts:160-191)
/// for the given helper set and sandbox mode.
fn description(helpers: &[&str], mode: crate::runtime::project::SandboxMode) -> String {
    let has = |name: &str| helpers.contains(&name);
    let helper_example = if has("edit_file") {
        "grep -> read_file -> edit_file"
    } else if has("read_file") {
        "grep -> read_file"
    } else {
        "bash -> concise output"
    };
    let file_write_guidance = if has("write_file") || has("edit_file") {
        "Before modifying an existing file, call read_file first so PilotDeck can verify freshness. Prefer edit_file for targeted changes and write_file for new files or complete rewrites. "
    } else {
        "This session exposes only the listed helper subset; unavailable file-write and network helpers must not be imported or called. "
    };
    let (helper_surface, helper_guidance) = if helpers.is_empty() {
        (
            "No PilotDeck helper RPC is available in this strict host sandbox. ".to_owned(),
            "Use normal Python only against the profile-owned process and its mounted workspace; do not import PilotDeck helpers. ".to_owned(),
        )
    } else {
        (
            format!("Available helper functions: {}. ", helpers.join(", ")),
            format!(
                "Use normal Python control flow to orchestrate tools: loops for batch work, conditionals for branching, data structures for aggregation, and try/except around individual helper calls when one failure should not abort the whole script. Helper failures raise RuntimeError. You can chain helper results, e.g. {helper_example}. Print only the concise final result needed by the agent. "
            ),
        )
    };
    // `describeExecutionEnvironment(sandbox)` (executeCode.ts:255-265).
    let execution_environment = match mode {
        crate::runtime::project::SandboxMode::DangerFullAccess => "The script runs from the workspace cwd and inherits the same runtime environment as normal tools such as bash, including configured API, proxy, PATH, virtualenv, and conda variables; do not print secrets or dump the full environment. ".to_owned(),
        mode => format!("The script runs from the workspace cwd with the host {} file-effect policy. It inherits the same runtime environment as normal tools such as bash, including configured API, proxy, PATH, virtualenv, and conda variables; do not print secrets or dump the full environment. ", mode.as_str()),
    };
    format!(
        "Run a local Python 3 script that can call a small allow-list of PilotDeck tools via `import pilotdeck_tools`. {execution_environment}Only the script's final stdout/stderr summary is returned to the model; intermediate tool results stay inside the script. {helper_surface}{helper_guidance}{file_write_guidance}Notebook edits, agent, task tools, MCP tools, and execute_code itself are not available."
    )
}

impl ToolDefinition for ExecuteCodeTool {
    fn fields(&self) -> &JsObject {
        &self.fields
    }
    fn is_read_only(&self, input: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(is_read_only(input)))
    }
    fn is_concurrency_safe(&self, _input: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(false))
    }
}

// ---------------------------------------------------------------------------
// isReadOnly
// ---------------------------------------------------------------------------

/// `isExecuteCodeReadOnly`: no write-capable helper call and only read-only
/// `bash(...)` calls whose first argument is a string literal.
fn is_read_only(input: &JsValue) -> bool {
    let JsValue::String(code) = field(input, "code") else {
        return false;
    };
    if code.is_empty() {
        return false;
    }
    // Both passes index the same UTF-16 units, like the baseline's strings.
    let code = code.units();
    let searchable = strip_python_comments_and_strings(code);
    if !call_offsets(&searchable, "write_file").is_empty()
        || !call_offsets(&searchable, "edit_file").is_empty()
    {
        return false;
    }
    call_offsets(&searchable, "bash").into_iter().all(|offset| {
        read_first_python_string_argument(code, offset).is_some_and(|command| {
            !command.is_empty() && is_read_only_shell_command(&String::from_utf16_lossy(&command))
        })
    })
}

fn unit_is(unit: u16, c: char) -> bool {
    unit == c as u16
}
fn unit_space(unit: u16) -> bool {
    char::from_u32(unit as u32).is_some_and(js_space)
}
fn unit_word(unit: u16) -> bool {
    char::from_u32(unit as u32).is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Offsets just past each match of `/\b<name>\s*\(/gu` (`lastIndex`).
fn call_offsets(text: &[u16], name: &str) -> Vec<usize> {
    let name: Vec<u16> = name.encode_utf16().collect();
    let mut offsets = Vec::new();
    let mut index = 0;
    while index + name.len() <= text.len() {
        let boundary = index == 0 || !unit_word(text[index - 1]);
        if boundary && text[index..].starts_with(&name) {
            let mut end = index + name.len();
            while end < text.len() && unit_space(text[end]) {
                end += 1;
            }
            if end < text.len() && unit_is(text[end], '(') {
                offsets.push(end + 1);
                index = end + 1;
                continue;
            }
        }
        index += 1;
    }
    offsets
}

/// `readFirstPythonStringArgument(code, offset)`.
fn read_first_python_string_argument(code: &[u16], offset: usize) -> Option<Vec<u16>> {
    let mut index = offset;
    while index < code.len() && unit_space(code[index]) {
        index += 1;
    }
    let quote = *code.get(index)?;
    if !unit_is(quote, '"') && !unit_is(quote, '\'') {
        return None;
    }
    let triple = code[index..].starts_with(&[quote; 3]);
    let delimiter = if triple { 3 } else { 1 };
    index += delimiter;
    let mut value = Vec::new();
    while index < code.len() {
        if unit_is(code[index], '\\') {
            let escaped = *code.get(index + 1)?;
            value.push(if unit_is(escaped, 'n') {
                '\n' as u16
            } else if unit_is(escaped, 't') {
                '\t' as u16
            } else {
                escaped
            });
            index += 2;
            continue;
        }
        if code[index..].starts_with(&[quote; 3][..delimiter]) {
            return Some(value);
        }
        value.push(code[index]);
        index += 1;
    }
    None
}

/// `stripPythonCommentsAndStrings`: comments and string literals become
/// spaces (newlines inside strings are kept), one unit for one unit.
fn strip_python_comments_and_strings(code: &[u16]) -> Vec<u16> {
    const SPACE: u16 = ' ' as u16;
    let mut output = Vec::with_capacity(code.len());
    let mut index = 0;
    while index < code.len() {
        let unit = code[index];
        if unit_is(unit, '#') {
            while index < code.len() && !unit_is(code[index], '\n') {
                output.push(SPACE);
                index += 1;
            }
            continue;
        }
        if unit_is(unit, '"') || unit_is(unit, '\'') {
            let length = if code[index..].starts_with(&[unit; 3]) {
                3
            } else {
                1
            };
            output.extend(std::iter::repeat_n(SPACE, length));
            index += length;
            while index < code.len() {
                if unit_is(code[index], '\\') {
                    output.extend([SPACE, SPACE]);
                    index += 2;
                    continue;
                }
                if code[index..].starts_with(&[unit; 3][..length]) {
                    output.extend(std::iter::repeat_n(SPACE, length));
                    index += length;
                    break;
                }
                output.push(if unit_is(code[index], '\n') {
                    '\n' as u16
                } else {
                    SPACE
                });
                index += 1;
            }
            continue;
        }
        output.push(unit);
        index += 1;
    }
    output
}

// ---------------------------------------------------------------------------
// validateInput
// ---------------------------------------------------------------------------

fn issue(path: &str, message: String) -> JsObject {
    object([
        ("path", string(path)),
        ("code", string("invalid_schema")),
        ("message", string(message)),
    ])
}

/// `validateExecuteCodeInput`.
async fn validate(input: &JsValue) -> Option<Vec<JsObject>> {
    let code = match field(input, "code") {
        JsValue::String(code) => text(code),
        _ => String::new(),
    };
    let mut issues = Vec::new();
    if code.chars().all(js_space) {
        issues.push(issue("$.code", "$.code must not be empty.".into()));
    }
    if field(input, "timeout_seconds")
        .as_number()
        .is_some_and(|value| !(1.0..=DEFAULT_TIMEOUT_SECONDS).contains(&value))
    {
        issues.push(issue(
            "$.timeout_seconds",
            format!("$.timeout_seconds must be between 1 and {DEFAULT_TIMEOUT_SECONDS}."),
        ));
    }
    if field(input, "max_tool_calls")
        .as_number()
        .is_some_and(|value| !(0.0..=DEFAULT_MAX_TOOL_CALLS).contains(&value))
    {
        issues.push(issue(
            "$.max_tool_calls",
            format!("$.max_tool_calls must be between 0 and {DEFAULT_MAX_TOOL_CALLS}."),
        ));
    }
    if issues.is_empty()
        && let Some(diagnostic) =
            super::syntax_diagnostics::python_diagnostics("execute_code.py", &code).await
    {
        issues.push(issue(
            "$.code",
            format!(
                "Python syntax error at L{}:{}: {}",
                js_value::number_string(diagnostic.line),
                js_value::number_string(diagnostic.column),
                diagnostic.message
            ),
        ));
    }
    (!issues.is_empty()).then_some(issues)
}

// ---------------------------------------------------------------------------
// execute
// ---------------------------------------------------------------------------

impl ExecutableTool for ExecuteCodeTool {
    fn validate_input(
        &self,
        input: JsValue,
        _context: ToolRunContext,
    ) -> ToolFuture<Option<Vec<JsObject>>> {
        Box::pin(async move { Ok(validate(&input).await) })
    }

    fn execute(&self, input: JsValue, context: ToolRunContext) -> ToolFuture<ToolOutput> {
        let allowed = self.allowed.clone();
        let rpc_allowed = self.rpc_allowed.clone();
        let sandbox = self.sandbox;
        Box::pin(async move {
            let started = now_ms();
            let result =
                run_execute_code(&input, &context, started, allowed, rpc_allowed, sandbox).await?;
            let mut metadata = object([
                ("status", string(result.status)),
                ("tool_calls_made", JsValue::Number(result.tool_calls_made)),
                ("duration_seconds", JsValue::Number(result.duration_seconds)),
                ("cwd", JsValue::String(cwd(&context))),
                ("env_inheritance", string("full")),
            ]);
            metadata.insert("python_path_augmented".into(), JsValue::Bool(true));
            Ok(ToolOutput {
                content: vec![JsValue::Object(object([
                    ("type", string("text")),
                    ("text", string(result.format())),
                ]))],
                data: Some(JsValue::Object(result.data())),
                metadata: Some(metadata),
                supplemental_messages: None,
            })
        })
    }
}

/// `ExecuteCodeOutput`.
struct Outcome {
    status: &'static str,
    output: String,
    error: Option<String>,
    tool_calls_made: f64,
    duration_seconds: f64,
    tool_call_log: Vec<JsValue>,
}

impl Outcome {
    /// `buildOutput`: an empty error is left out; the duration is
    /// `Math.round(((Date.now() - startedAt) / 1000) * 100) / 100`.
    fn build(
        status: &'static str,
        output: String,
        error: Option<String>,
        started: f64,
        calls: (f64, Vec<JsValue>),
    ) -> Self {
        Self {
            status,
            output,
            error: error.filter(|error| !error.is_empty()),
            tool_calls_made: calls.0,
            duration_seconds: duration_seconds(now_ms() - started),
            tool_call_log: calls.1,
        }
    }
    /// `formatExecuteCodeResult`.
    fn format(&self) -> String {
        let mut lines = vec![
            format!("status: {}", self.status),
            format!(
                "duration_seconds: {}",
                js_value::number_string(self.duration_seconds)
            ),
            format!(
                "tool_calls_made: {}",
                js_value::number_string(self.tool_calls_made)
            ),
        ];
        if let Some(error) = &self.error {
            lines.push(format!("error: {error}"));
        }
        if !self.output.is_empty() {
            lines.push(String::new());
            lines.push(self.output.clone());
        }
        lines.join("\n")
    }
    fn data(&self) -> JsObject {
        let mut data = object([
            ("status", string(self.status)),
            ("output", string(self.output.as_str())),
        ]);
        if let Some(error) = &self.error {
            data.insert("error".into(), string(error.as_str()));
        }
        data.insert(
            "tool_calls_made".into(),
            JsValue::Number(self.tool_calls_made),
        );
        data.insert(
            "duration_seconds".into(),
            JsValue::Number(self.duration_seconds),
        );
        data.insert(
            "tool_call_log".into(),
            JsValue::Array(self.tool_call_log.clone()),
        );
        data
    }
}

/// `Math.round((elapsedMs / 1000) * 100) / 100`: seconds rounded half-up to
/// two places. Printed as a JS number, so trailing zeros are dropped.
fn duration_seconds(elapsed_ms: f64) -> f64 {
    ((elapsed_ms / 1000.0) * 100.0 + 0.5).floor() / 100.0
}

/// `Date.now()`.
fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_millis() as f64)
}

/// The temporary workspace and the socket file; both are removed when the
/// run ends, including when its future is dropped (`cleanup()`).
struct Workspace {
    root: PathBuf,
    socket: PathBuf,
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn random_hex() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// `mkdtemp(join(tmpdir(), "pilotdeck_execute_code_"))`.
fn create_workspace_root() -> Result<PathBuf, ToolError> {
    use std::os::unix::fs::DirBuilderExt;
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let prefix = format!(
        "{}/pilotdeck_execute_code_",
        node_tmpdir(|key| std::env::var_os(key))
    );
    loop {
        let suffix: String = uuid::Uuid::new_v4().as_bytes()[..6]
            .iter()
            .map(|byte| ALPHABET[*byte as usize % ALPHABET.len()] as char)
            .collect();
        let root = PathBuf::from(format!("{prefix}{suffix}"));
        match std::fs::DirBuilder::new().mode(0o700).create(&root) {
            Ok(()) => return Ok(root),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                let template = format!("{prefix}XXXXXX");
                return Err(ToolError::failed(
                    crate::node_io::diagnostic(&error, "mkdtemp", Some(&template))
                        .map_or_else(|| error.to_string(), |(_, message)| message),
                ));
            }
        }
    }
}

/// `createNodeExecutionTransportPort().create()` for `uds`: macOS uses `/tmp`
/// (its per-user temp dir is too long for `sun_path`), other hosts the temp
/// dir.
fn socket_path() -> PathBuf {
    let directory = if cfg!(target_os = "macos") {
        "/tmp".to_owned()
    } else {
        node_tmpdir(|key| std::env::var_os(key))
    };
    PathBuf::from(format!(
        "{directory}/pilotdeck_rpc_{}_{}_{}.sock",
        std::process::id(),
        js_value::number_string(now_ms()),
        &random_hex()[..13]
    ))
}

fn lossy(value: &JsString) -> String {
    String::from_utf16_lossy(value.units())
}

/// `resolveExecutable(["python3", "python"], context.env, signal)`: the first
/// candidate whose `--version` exits 0. Under a constrained sandbox the probe
/// runs confined too (`createNodeSandboxedCodeRuntimePort.resolveExecutable`):
/// a provider that cannot prepare it skips the candidate, and a probe that
/// does not finish within 5 s does not count.
async fn resolve_python(
    env: Option<&[(JsString, JsString)]>,
    context: &ToolRunContext,
    sandbox: crate::runtime::tools::sandbox::Sandbox,
) -> Result<Option<&'static str>, ToolError> {
    for candidate in ["python3", "python"] {
        if context.is_aborted() {
            return Err(ToolError::aborted());
        }
        let mut command = if sandbox.confines() {
            let probe_cwd = super::os_path(&cwd(context));
            let Ok(prepared) =
                sandbox.prepare(candidate, vec!["--version".into()], &probe_cwd, None)
            else {
                continue;
            };
            let mut command = Command::new(prepared.executable);
            command.args(prepared.args).current_dir(probe_cwd);
            command
        } else {
            let mut command = Command::new(candidate);
            command.arg("--version");
            command
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        apply_env(&mut command, env);
        pilotdeck_child_signals::install_child_signal_reset(command.as_std_mut());
        let Ok(mut child) = command.spawn() else {
            continue;
        };
        let limit = if sandbox.confines() {
            Duration::from_secs(5)
        } else {
            Duration::from_secs(u64::from(u32::MAX))
        };
        let available = tokio::select! {
            status = child.wait() => status.is_ok_and(|status| status.code() == Some(0)),
            _ = tokio::time::sleep(limit) => false,
            _ = context.cancelled() => false,
        };
        if available {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn apply_env(command: &mut Command, env: Option<&[(JsString, JsString)]>) {
    if let Some(env) = env {
        command.env_clear();
        for (key, value) in env {
            command.env(lossy(key), lossy(value));
        }
    }
}

/// `buildChildEnv(context.env ?? process.env, transport, tempRoot, cwd)`.
fn child_env(
    env: Option<Vec<(JsString, JsString)>>,
    workspace: &Workspace,
    workspace_cwd: &JsString,
) -> Vec<(JsString, JsString)> {
    let mut env = env.unwrap_or_else(|| {
        std::env::vars_os()
            .map(|(key, value)| {
                (
                    JsString::from(key.to_string_lossy().as_ref()),
                    JsString::from(value.to_string_lossy().as_ref()),
                )
            })
            .collect()
    });
    let temp_root = workspace.root.to_string_lossy().into_owned();
    // `source.PYTHONPATH ? `${tempRoot}:${source.PYTHONPATH}` : tempRoot`.
    let python_path = match env
        .iter()
        .find(|(name, _)| name.equals_str("PYTHONPATH"))
        .map(|(_, value)| lossy(value))
        .filter(|value| !value.is_empty())
    {
        Some(existing) => format!("{temp_root}:{existing}"),
        None => temp_root.clone(),
    };
    let mut set =
        |key: &str, value: JsString| match env.iter_mut().find(|(name, _)| name.equals_str(key)) {
            Some(entry) => entry.1 = value,
            None => env.push((key.into(), value)),
        };
    set("PYTHONPATH", python_path.as_str().into());
    set("PILOTDECK_WORKSPACE_CWD", workspace_cwd.clone());
    set(
        "PILOTDECK_EXECUTE_CODE_TEMP_ROOT",
        temp_root.as_str().into(),
    );
    set(
        "PILOTDECK_RPC_SOCKET",
        workspace.socket.to_string_lossy().as_ref().into(),
    );
    set("PYTHONDONTWRITEBYTECODE", "1".into());
    env
}

async fn run_execute_code(
    input: &JsValue,
    context: &ToolRunContext,
    started: f64,
    allowed: Arc<Vec<&'static str>>,
    rpc_allowed: Arc<Vec<&'static str>>,
    sandbox: crate::runtime::tools::sandbox::Sandbox,
) -> Result<Outcome, ToolError> {
    let timeout_seconds = field(input, "timeout_seconds")
        .as_number()
        .unwrap_or(DEFAULT_TIMEOUT_SECONDS);
    let max_tool_calls = field(input, "max_tool_calls")
        .as_number()
        .unwrap_or(DEFAULT_MAX_TOOL_CALLS);
    let none = || (0.0, Vec::new());
    let Some(execute_tool) = context.execute_tool.clone() else {
        return Ok(Outcome::build(
            "unsupported",
            String::new(),
            Some(
                "execute_code requires a ToolRuntime recursion hook, but this host did not provide one."
                    .into(),
            ),
            started,
            none(),
        ));
    };
    let env = spawn_env(context)?;
    let Some(python) = resolve_python(env.as_deref(), context, sandbox).await? else {
        return Ok(Outcome::build(
            "unsupported",
            String::new(),
            Some("execute_code requires python3 on PATH.".into()),
            started,
            none(),
        ));
    };
    if context.is_aborted() {
        return Err(ToolError::aborted());
    }
    let workspace = Workspace {
        root: create_workspace_root()?,
        socket: socket_path(),
    };
    let rpc = Arc::new(Rpc {
        execute_tool,
        allowed: rpc_allowed,
        max_tool_calls,
        outer_id: context
            .fields
            .get("currentToolCallId")
            .and_then(JsValue::as_string)
            .cloned()
            .unwrap_or_else(|| "execute_code".into()),
        calls: Mutex::new((0.0, Vec::new())),
    });
    let code = match field(input, "code") {
        JsValue::String(code) => text(code),
        _ => String::new(),
    };
    let run = async {
        let script = workspace.root.join("script.py");
        std::fs::write(
            workspace.root.join("pilotdeck_tools.py"),
            pilotdeck_tools_module(&allowed),
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(&script, code).map_err(|error| error.to_string())?;
        let listener = UnixListener::bind(&workspace.socket)
            .map_err(|error| format!("listen {error} {}", workspace.socket.to_string_lossy()))?;
        let workspace_cwd = cwd(context);
        let env = child_env(env, &workspace, &workspace_cwd);
        tokio::select! {
            execution = run_python(python, &script, &workspace_cwd, &env, timeout_seconds, context, sandbox) => execution,
            error = serve(listener, rpc.clone()) => Err(error.message.clone()),
        }
    };
    let execution = run.await;
    drop(workspace);
    let calls = rpc
        .calls
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let execution = match execution {
        Ok(execution) => execution,
        // `catch (error)`: anything thrown after the workspace exists.
        Err(message) => {
            return Ok(Outcome::build(
                "error",
                String::new(),
                Some(message),
                started,
                calls,
            ));
        }
    };
    let status = if execution.cancelled {
        "cancelled"
    } else if execution.timed_out {
        "timeout"
    } else if execution.exit_code != Some(0) {
        "error"
    } else {
        "success"
    };
    let status_error = match status {
        "cancelled" => Some("Script execution was cancelled.".to_owned()),
        "timeout" => Some(format!(
            "Script timed out after {}s and was killed.",
            js_value::number_string(timeout_seconds)
        )),
        "error" if !execution.stderr.is_empty() => Some(execution.stderr.clone()),
        "error" => Some(format!(
            "Script exited with code {}.",
            execution
                .exit_code
                .map_or_else(|| "unknown".to_owned(), |code| code.to_string())
        )),
        _ => None,
    };
    let output = if status == "error" && !execution.stderr.is_empty() {
        format!("{}\n--- stderr ---\n{}", execution.stdout, execution.stderr)
            .trim_matches(js_space)
            .to_owned()
    } else {
        execution.stdout
    };
    Ok(Outcome::build(
        status,
        strip_ansi(&output),
        status_error.map(|error| strip_ansi(&error)),
        started,
        calls,
    ))
}

/// `stripAnsi`.
fn strip_ansi(value: &str) -> String {
    static ANSI: LazyLock<Regex> =
        LazyLock::new(|| Regex::new("\u{1b}\\[[0-?]*[ -/]*[@-~]").expect("valid ANSI pattern"));
    ANSI.replace_all(value, "").into_owned()
}

// ---------------------------------------------------------------------------
// The interpreter process (NodeCodeRuntimePort.ts)
// ---------------------------------------------------------------------------

/// `CodeRuntimeResult`.
struct Execution {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
    cancelled: bool,
}

/// `collectHeadTail`: the first 40% of the budget and the last 60%, with a
/// marker between them once the stream outgrew the budget.
struct HeadTail {
    head: Vec<u8>,
    tail: Vec<u8>,
    head_bytes: usize,
    tail_bytes: usize,
    max_bytes: usize,
    total: usize,
}
impl HeadTail {
    fn new(max_bytes: usize) -> Self {
        // `Math.floor(maxBytes * 0.4)`.
        let head_bytes = max_bytes * 2 / 5;
        Self {
            head: Vec::new(),
            tail: Vec::new(),
            head_bytes,
            tail_bytes: max_bytes - head_bytes,
            max_bytes,
            total: 0,
        }
    }
    fn push(&mut self, mut chunk: &[u8]) {
        self.total += chunk.len();
        if self.head.len() < self.head_bytes {
            let keep = chunk.len().min(self.head_bytes - self.head.len());
            self.head.extend_from_slice(&chunk[..keep]);
            chunk = &chunk[keep..];
        }
        if !chunk.is_empty() {
            self.tail.extend_from_slice(chunk);
            if self.tail.len() > self.tail_bytes {
                let overflow = self.tail.len() - self.tail_bytes;
                self.tail.drain(..overflow);
            }
        }
    }
    fn finish(self) -> String {
        let head = String::from_utf8_lossy(&self.head);
        let tail = String::from_utf8_lossy(&self.tail);
        if self.total > self.max_bytes && !tail.is_empty() {
            let omitted = self.total.saturating_sub(head.len() + tail.len());
            format!(
                "{head}\n\n... [OUTPUT TRUNCATED - {} bytes omitted out of {} total] ...\n\n{tail}",
                grouped(omitted),
                grouped(self.total)
            )
        } else {
            format!("{head}{tail}")
        }
    }
}

/// `Number.prototype.toLocaleString()` of a non-negative integer in the
/// default `en-US` locale.
fn grouped(value: usize) -> String {
    let digits = value.to_string();
    let mut output = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            output.push(',');
        }
        output.push(digit);
    }
    output
}

/// `spawn <file> <code>` as Node reports a failed spawn.
fn spawn_error(error: &std::io::Error, file: &str) -> String {
    match crate::node_io::lookup(error) {
        Some((code, _)) => format!("spawn {file} {code}"),
        None => format!("spawn {file} {error}"),
    }
}

/// `codeRuntime.run(...)`: `python <script>` in the workspace cwd, detached,
/// stdin ignored, stdout head/tail-bounded and stderr head-bounded.
async fn run_python(
    python: &str,
    script: &Path,
    workspace_cwd: &JsString,
    env: &[(JsString, JsString)],
    timeout_seconds: f64,
    context: &ToolRunContext,
    sandbox: crate::runtime::tools::sandbox::Sandbox,
) -> Result<Execution, String> {
    // `sandbox.prepare({ executable: python, args: [script], cwd, env,
    // policy: { mode, workspaceRoot: cwd } })`; the provider error is the
    // tool's `catch` branch.
    let workspace = super::os_path(workspace_cwd);
    let prepared = sandbox.prepare(
        python,
        vec![script.to_string_lossy().into_owned()],
        &workspace,
        None,
    )?;
    // Deliberate deviation (as in `bash`'s runner): an interpreter is never
    // started for a turn that is already aborted; the run reports
    // `cancelled` as an abort during the run would.
    if context.is_aborted() {
        return Ok(Execution {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
            cancelled: true,
        });
    }
    let mut command = Command::new(&prepared.executable);
    command
        .args(&prepared.args)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    apply_env(&mut command, Some(env));
    pilotdeck_child_signals::install_child_detach_session(command.as_std_mut());
    pilotdeck_child_signals::install_child_signal_reset(command.as_std_mut());
    let mut child = command
        .spawn()
        .map_err(|error| spawn_error(&error, &prepared.executable))?;
    let mut guard = GroupGuard(
        child
            .id()
            .and_then(|pid| rustix::process::Pid::from_raw(pid as i32)),
    );
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut out = HeadTail::new(MAX_STDOUT_BYTES);
    let mut err: Vec<u8> = Vec::new();
    let mut out_buffer = vec![0u8; 64 * 1024];
    let mut err_buffer = vec![0u8; 64 * 1024];
    let timeout_at = tokio::time::Instant::now() + Duration::from_secs_f64(timeout_seconds);
    let mut timeout_armed = true;
    // Armed even when the abort landed between the check above and the
    // spawn: the interpreter is stopped at once rather than run.
    let mut abort_armed = true;
    let mut kill_at: Option<tokio::time::Instant> = None;
    let mut timed_out = false;
    let mut cancelled = false;
    let mut status: Option<std::process::ExitStatus> = None;
    // Deliberate deviation (module docs): the baseline waits for both streams
    // to end however long a detached descendant keeps them open; with the
    // same input it hangs until that descendant exits.
    let mut drain_until: Option<tokio::time::Instant> = None;
    let mut settle_by: Option<tokio::time::Instant> = None;
    // Descendants found when the timeout/abort fired; signalled with the group.
    let mut descendants: Vec<rustix::process::Pid> = Vec::new();
    let leader = guard.0;
    while stdout.is_some() || stderr.is_some() || status.is_none() {
        tokio::select! {
            read = async { stdout.as_mut().unwrap().read(&mut out_buffer).await }, if stdout.is_some() => {
                match read {
                    Ok(length) if length > 0 => out.push(&out_buffer[..length]),
                    _ => stdout = None,
                }
            }
            read = async { stderr.as_mut().unwrap().read(&mut err_buffer).await }, if stderr.is_some() => {
                match read {
                    Ok(length) if length > 0 => {
                        let keep = length.min(MAX_STDERR_BYTES.saturating_sub(err.len()));
                        err.extend_from_slice(&err_buffer[..keep]);
                    }
                    _ => stderr = None,
                }
            }
            exited = child.wait(), if status.is_none() => {
                status = Some(exited.map_err(|error| error.to_string())?);
                // The timers stop observing once the process has exited
                // (`settled`); a kill that is already scheduled stays.
                timeout_armed = false;
                abort_armed = false;
                drain_until = Some(tokio::time::Instant::now() + PIPE_GRACE);
            }
            _ = tokio::time::sleep_until(drain_until.unwrap_or(timeout_at)), if drain_until.is_some() => {
                // A descendant still holds a pipe: stop waiting for its EOF.
                drain_until = None;
                stdout = None;
                stderr = None;
            }
            _ = tokio::time::sleep_until(settle_by.unwrap_or(timeout_at)), if settle_by.is_some() && status.is_none() => {
                break;
            }
            _ = tokio::time::sleep_until(timeout_at), if timeout_armed => {
                timeout_armed = false;
                timed_out = true;
                if let Some(leader) = leader {
                    descendants = descendant_pids(leader).await;
                }
                guard.signal(rustix::process::Signal::TERM);
                signal_each(&descendants, rustix::process::Signal::TERM);
                kill_at.get_or_insert(tokio::time::Instant::now() + KILL_AFTER);
            }
            _ = context.cancelled(), if abort_armed => {
                abort_armed = false;
                cancelled = true;
                if let Some(leader) = leader {
                    descendants = descendant_pids(leader).await;
                }
                guard.signal(rustix::process::Signal::TERM);
                signal_each(&descendants, rustix::process::Signal::TERM);
                kill_at.get_or_insert(tokio::time::Instant::now() + KILL_AFTER);
            }
            _ = tokio::time::sleep_until(kill_at.unwrap_or(timeout_at)), if kill_at.is_some() => {
                kill_at = None;
                guard.signal(rustix::process::Signal::KILL);
                signal_each(&descendants, rustix::process::Signal::KILL);
                settle_by = Some(tokio::time::Instant::now() + SETTLE_BOUND);
            }
        }
    }
    if kill_at.is_some() {
        guard.signal(rustix::process::Signal::KILL);
        signal_each(&descendants, rustix::process::Signal::KILL);
    }
    if status.is_none() {
        // SETTLE_BOUND elapsed: the interpreter outlived its SIGKILL. The
        // still-armed guard and `kill_on_drop` repeat the KILL on drop.
        return Ok(Execution {
            exit_code: None,
            stdout: out.finish(),
            stderr: String::from_utf8_lossy(&err).into_owned(),
            timed_out,
            cancelled,
        });
    }
    // A script that ran to completion may leave its own background
    // processes behind, as in the baseline.
    guard.disarm();
    Ok(Execution {
        exit_code: status.and_then(|status| status.code()),
        stdout: out.finish(),
        stderr: String::from_utf8_lossy(&err).into_owned(),
        timed_out,
        cancelled,
    })
}

/// Send `signal` to each pid (best effort; a pid that is gone is skipped).
fn signal_each(pids: &[rustix::process::Pid], signal: rustix::process::Signal) {
    for pid in pids {
        let _ = rustix::process::kill_process(*pid, signal);
    }
}

/// Every live descendant of `root` (children, grandchildren, ...), including
/// ones that left its session or group. Linux reads `/proc/<pid>/stat`;
/// elsewhere `ps -A -o pid= -o ppid=` is asked. A failure yields what was
/// found (possibly nothing): this only widens a best-effort cleanup.
async fn descendant_pids(root: rustix::process::Pid) -> Vec<rustix::process::Pid> {
    let table = process_table().await;
    let mut found = vec![root.as_raw_nonzero().get()];
    let mut index = 0;
    while index < found.len() {
        let parent = found[index];
        for (pid, ppid) in &table {
            if *ppid == parent && !found.contains(pid) {
                found.push(*pid);
            }
        }
        index += 1;
    }
    found
        .into_iter()
        .skip(1)
        .filter_map(rustix::process::Pid::from_raw)
        .collect()
}

/// `(pid, ppid)` for every visible process.
async fn process_table() -> Vec<(i32, i32)> {
    #[cfg(target_os = "linux")]
    {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let pid: i32 = entry.file_name().to_str()?.parse().ok()?;
                let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
                // `pid (comm) state ppid ...`; comm may contain spaces and ')'.
                let rest = &stat[stat.rfind(')')? + 1..];
                let ppid = rest.split_whitespace().nth(1)?.parse().ok()?;
                Some((pid, ppid))
            })
            .collect()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let mut command = Command::new("ps");
        command
            .args(["-A", "-o", "pid=", "-o", "ppid="])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let Ok(Ok(output)) = tokio::time::timeout(Duration::from_secs(2), command.output()).await
        else {
            return Vec::new();
        };
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let pid = fields.next()?.parse().ok()?;
                let ppid = fields.next()?.parse().ok()?;
                Some((pid, ppid))
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// The RPC bridge
// ---------------------------------------------------------------------------

struct Rpc {
    execute_tool: NestedToolExecutor,
    /// `rpcAllowedTools` (unrestricted: no constrained sandbox policy).
    allowed: Arc<Vec<&'static str>>,
    max_tool_calls: f64,
    /// `context.currentToolCallId ?? "execute_code"`.
    outer_id: JsString,
    /// `toolCallsMade` and `toolCallLog`.
    calls: Mutex<(f64, Vec<JsValue>)>,
}

/// `createRpcServer`: one task per connection, all owned by this future.
/// Resolves only when a nested call fails at the host level.
async fn serve(listener: UnixListener, rpc: Arc<Rpc>) -> HostError {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    connections.spawn(connection(stream, rpc.clone()));
                }
            }
            Some(finished) = connections.join_next(), if !connections.is_empty() => {
                if let Ok(Err(error)) = finished {
                    return error;
                }
            }
        }
    }
}

/// One socket: newline-delimited JSON requests, answered in order.
async fn connection(stream: UnixStream, rpc: Arc<Rpc>) -> Result<(), HostError> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line).await {
            Ok(length) if length > 0 && line.ends_with(b"\n") => {}
            // EOF; an unterminated tail is never processed.
            _ => return Ok(()),
        }
        let request = String::from_utf8_lossy(&line);
        let request = request.trim_matches(js_space);
        if request.is_empty() {
            continue;
        }
        let response = rpc.handle(request).await?;
        let mut payload = js_value::json_stringify(&JsValue::Object(response))
            .ok()
            .flatten()
            .unwrap_or_else(|| "{}".into());
        payload.push('\n');
        if write.write_all(payload.as_bytes()).await.is_err() {
            return Ok(());
        }
    }
}

fn rpc_error(message: String, code: &str) -> JsObject {
    object([("error", string(message)), ("code", string(code))])
}

impl Rpc {
    /// `handleRpcLine`.
    async fn handle(&self, line: &str) -> Result<JsObject, HostError> {
        let request = match parse_json(line) {
            Ok(request) => request,
            // `JSON.parse(line)`'s V8 SyntaxError message.
            Err(error) => {
                let message = pilotdeck_protocol::json_syntax::parse_error(line)
                    .unwrap_or_else(|| error.to_string());
                return Ok(rpc_error(
                    format!("Invalid RPC request: {message}"),
                    "invalid_rpc",
                ));
            }
        };
        let tool_name = request
            .get("tool")
            .and_then(JsValue::as_string)
            .cloned()
            .unwrap_or_default();
        let args = match request.get("args") {
            Some(JsValue::Object(args)) => args.clone(),
            _ => JsObject::new(),
        };
        if !self
            .allowed
            .iter()
            .any(|allowed| tool_name.equals_str(allowed))
        {
            return Ok(rpc_error(
                format!(
                    "Tool '{}' is not available in execute_code.",
                    lossy(&tool_name)
                ),
                "tool_not_allowed",
            ));
        }
        let sequence = {
            let mut calls = self
                .calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if calls.0 >= self.max_tool_calls {
                return Ok(rpc_error(
                    format!(
                        "Tool call limit reached ({}). No more tool calls allowed in this execution.",
                        js_value::number_string(self.max_tool_calls)
                    ),
                    "tool_call_limit_reached",
                ));
            }
            calls.0 += 1.0;
            calls.0
        };
        let started = now_ms();
        let mut id = self.outer_id.units().to_vec();
        id.extend(format!(":code:{}", js_value::number_string(sequence)).encode_utf16());
        let result = (self.execute_tool)(ToolCall {
            id: JsString::from_utf16(id),
            name: tool_name.clone(),
            input: JsValue::Object(args),
        })
        .await?;
        let ok = result
            .get("type")
            .and_then(JsValue::as_string)
            .is_some_and(|kind| kind.equals_str("success"));
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .1
            .push(JsValue::Object(object([
                ("tool", JsValue::String(tool_name)),
                ("duration_ms", JsValue::Number(now_ms() - started)),
                ("ok", JsValue::Bool(ok)),
            ])));
        tool_result_to_rpc_response(&result, ok)
    }
}

/// `toolResultToRpcResponse`.
fn tool_result_to_rpc_response(result: &JsObject, ok: bool) -> Result<JsObject, HostError> {
    let content = match result.get("content") {
        Some(JsValue::Array(blocks)) => blocks
            .iter()
            .map(content_text)
            .collect::<Result<Vec<_>, _>>()?
            .join("\n"),
        _ => String::new(),
    };
    let metadata = result
        .get("metadata")
        .cloned()
        .unwrap_or(JsValue::Undefined);
    if ok {
        return Ok(object([
            ("content", string(content)),
            (
                "data",
                result.get("data").cloned().unwrap_or(JsValue::Undefined),
            ),
            ("metadata", metadata),
        ]));
    }
    let error = result.get("error");
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(JsValue::as_string)
        .map(lossy)
        .unwrap_or_default();
    // `formatToolErrorDetails`: the issue messages, one per line.
    let details = error
        .and_then(|error| error.get("details"))
        .and_then(|details| details.get("issues"))
        .and_then(|issues| match issues {
            JsValue::Array(issues) => Some(issues),
            _ => None,
        })
        .map(|issues| {
            issues
                .iter()
                .filter_map(|issue| issue.get("message").and_then(JsValue::as_string))
                .filter(|message| !message.is_empty())
                .map(lossy)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|details| !details.is_empty());
    Ok(object([
        (
            "error",
            string(match details {
                Some(details) => format!("{message}\n{details}"),
                None => message,
            }),
        ),
        (
            "code",
            error
                .and_then(|error| error.get("code"))
                .cloned()
                .unwrap_or(JsValue::Undefined),
        ),
        ("content", string(content)),
        ("metadata", metadata),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(code: &str) -> JsValue {
        JsValue::Object(object([("code", string(code))]))
    }

    #[test]
    fn read_only_follows_helper_calls_and_bash_literals() {
        // executeCode.spec.ts: plain scripts and read-only bash are read-only.
        assert!(is_read_only(&input("print('hi')")));
        assert!(is_read_only(&input(
            "from pilotdeck_tools import bash\nbash('git status')"
        )));
        assert!(is_read_only(&input("r = read_file('a')\nprint(r)")));
        // Write-capable helpers and non-literal or mutating bash are not.
        assert!(!is_read_only(&input("write_file('a', 'b')")));
        assert!(!is_read_only(&input("edit_file ('a', 'b', 'c')")));
        assert!(!is_read_only(&input("bash('rm -rf x')")));
        assert!(!is_read_only(&input("cmd = 'ls'\nbash(cmd)")));
        // Mentions inside comments and strings do not count.
        assert!(is_read_only(&input(
            "# write_file('a', 'b')\nprint(\"edit_file(1)\")"
        )));
        // `\b`: an identifier that merely ends with the helper name.
        assert!(is_read_only(&input("my_write_file('a')")));
        assert!(!is_read_only(&JsValue::Object(JsObject::new())));
        assert!(!is_read_only(&input("")));
    }

    #[test]
    fn head_tail_keeps_both_ends_and_reports_the_omitted_bytes() {
        let mut stream = HeadTail::new(10);
        stream.push(b"0123456789");
        assert_eq!(stream.finish(), "0123456789");
        let mut stream = HeadTail::new(10);
        stream.push(b"01234");
        stream.push(b"56789abcdefghij");
        assert_eq!(
            stream.finish(),
            "0123\n\n... [OUTPUT TRUNCATED - 10 bytes omitted out of 20 total] ...\n\nefghij"
        );
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(76_000), "76,000");
        assert_eq!(grouped(1_234_567), "1,234,567");
    }

    #[test]
    fn duration_is_rounded_to_two_places_and_printed_as_a_js_number() {
        // (elapsed ms, value, text in `duration_seconds: ...`), as node prints
        // `Math.round((ms / 1000) * 100) / 100`.
        for (elapsed, value, printed) in [
            (0.0, 0.0, "0"),
            (4.0, 0.0, "0"),
            (5.0, 0.01, "0.01"),
            (15.0, 0.02, "0.02"),
            (20.0, 0.02, "0.02"),
            (100.0, 0.1, "0.1"),
            (104.0, 0.1, "0.1"),
            (1000.0, 1.0, "1"),
            (1005.0, 1.0, "1"),
            // 1.005 * 100 and 1.015 * 100 fall just under .5 in binary.
            (1006.0, 1.01, "1.01"),
            (1015.0, 1.01, "1.01"),
            (2675.0, 2.68, "2.68"),
            (1234.0, 1.23, "1.23"),
            (300_004.0, 300.0, "300"),
        ] {
            let seconds = duration_seconds(elapsed);
            assert_eq!(seconds, value, "{elapsed} ms");
            let outcome = Outcome {
                status: "success",
                output: String::new(),
                error: None,
                tool_calls_made: 0.0,
                duration_seconds: seconds,
                tool_call_log: Vec::new(),
            };
            assert_eq!(
                outcome.format(),
                format!("status: success\nduration_seconds: {printed}\ntool_calls_made: 0"),
                "{elapsed} ms"
            );
        }
    }

    #[test]
    fn strip_ansi_removes_csi_sequences() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m plain"), "red plain");
    }

    #[test]
    fn helper_module_lists_only_allowed_helpers() {
        let module = pilotdeck_tools_module(&BASE_ALLOWED_TOOLS);
        assert!(
            module.starts_with("\"\"\"Auto-generated PilotDeck execute_code RPC helpers.\"\"\"\n")
        );
        assert!(module.contains("    return response\n\n\ndef web_fetch("));
        assert!(!module.contains("def web_search("));
        assert!(module.ends_with("    return _call(\"bash\", args)\n"));
        assert_eq!(
            pilotdeck_tools_module(&[]),
            "\"\"\"No PilotDeck helper RPC is available in this strict host sandbox.\"\"\"\n"
        );
    }
}
