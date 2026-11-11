//! Run-mode gates that the tool runtime applies before input validation and
//! before the permission decision (ToolRuntime.ts:129-150, 706-764;
//! planModeConstraints.ts; askModeConstraints.ts).
//!
//! - Plan mode (`context.permissionMode === "plan"`): only the plan allow
//!   list may run; `bash` only with a read-only command; `write_file` /
//!   `edit_file` only on a `.md` path under the plan directory.
//! - Ask run mode (`context.runMode === "ask"`): only the ask allow list (an
//!   MCP tool counts when `isReadOnly({})` is truthy); `bash` only with a
//!   read-only command; any other tool except `agent` must report its input as
//!   read-only.
//!
//! These checks are independent of the permission mode, so
//! `bypassPermissions` does not lift them, and they apply to nested calls
//! (`execute_code`) and subagents through the same runtime context.
use super::{ExecutableTool, ToolRunContext, truthy};
use crate::host::HostError;
use crate::runtime::permission;
use crate::runtime::tools::builtin::bash::is_read_only_shell_command;
use pilotdeck_protocol::{JsObject, JsString, JsValue};

/// `PLAN_MODE_ALLOWED_TOOLS` (planModeConstraints.ts:11-30).
const PLAN_MODE_ALLOWED_TOOLS: [&str; 18] = [
    "read_file",
    "get_current_time",
    "grep",
    "glob",
    "web_search",
    "web_fetch",
    "ask_user_question",
    "todo_write",
    "exit_plan_mode",
    "read_skill",
    "structured_output",
    "agent",
    "bash",
    "task_list",
    "task_output",
    "task_wait",
    "write_file",
    "edit_file",
];

/// `ASK_MODE_ALLOWED_TOOLS` (askModeConstraints.ts:8-24).
const ASK_MODE_ALLOWED_TOOLS: [&str; 15] = [
    "read_file",
    "get_current_time",
    "grep",
    "glob",
    "web_search",
    "web_fetch",
    "ask_user_question",
    "read_skill",
    "structured_output",
    "agent",
    "execute_code",
    "bash",
    "task_list",
    "task_output",
    "task_wait",
];

fn listed(list: &[&str], name: &JsString) -> bool {
    list.iter().any(|entry| name.equals_str(entry))
}

/// `readStringProperty(input, key)`: the value only when it is a string.
fn string_property(input: &JsValue, key: &str) -> Option<JsString> {
    match input {
        JsValue::Object(object) => object.get(key).and_then(JsValue::as_string).cloned(),
        _ => None,
    }
}

fn is_read_only_command(command: Option<&JsString>) -> bool {
    command.is_some_and(|command| {
        !command.is_empty()
            && is_read_only_shell_command(&String::from_utf16_lossy(command.units()))
    })
}

/// `getPlanModeViolation` (ToolRuntime.ts:706-737).
pub(super) fn plan_mode_violation(
    name: &JsString,
    input: &JsValue,
    context: &ToolRunContext,
) -> Result<Option<JsString>, HostError> {
    if context.fields.get("permissionMode") != Some(&JsValue::from("plan")) {
        return Ok(None);
    }
    if !listed(&PLAN_MODE_ALLOWED_TOOLS, name) {
        return Ok(Some(permission::plan_mode_tool_violation_message(name)));
    }
    if name.equals_str("bash") {
        let command = string_property(input, "command");
        if !is_read_only_command(command.as_ref()) {
            return Ok(Some(permission::plan_mode_bash_violation_message(
                &command.unwrap_or_default(),
            )));
        }
        return Ok(None);
    }
    if name.equals_str("write_file") || name.equals_str("edit_file") {
        let file_path =
            string_property(input, "file_path").or_else(|| string_property(input, "filePath"));
        let plan_directory = context
            .host
            .plan_directory
            .as_ref()
            .map(|directory| &directory.path)
            .filter(|path| !path.is_empty());
        let cwd = context
            .fields
            .get("cwd")
            .and_then(JsValue::as_string)
            .cloned()
            .unwrap_or_default();
        let allowed = match (file_path.filter(|path| !path.is_empty()), plan_directory) {
            (Some(file_path), Some(plan_directory)) => {
                permission::is_plan_markdown_path(&file_path, plan_directory, &cwd)?
            }
            _ => false,
        };
        if !allowed {
            return Ok(Some(permission::plan_mode_tool_violation_message(name)));
        }
    }
    Ok(None)
}

/// `getAskModeViolation` (askModeConstraints.ts:62-82), applied only when
/// `context.runMode === "ask"`.
pub(super) fn ask_mode_violation(
    tool: &dyn ExecutableTool,
    name: &JsString,
    input: &JsValue,
    context: &ToolRunContext,
) -> Result<Option<JsString>, HostError> {
    if context.fields.get("runMode") != Some(&JsValue::from("ask")) {
        return Ok(None);
    }
    // isAskModeAllowedTool: an MCP tool is judged by isReadOnly({}).
    let allowed = if tool
        .fields()
        .get("kind")
        .is_some_and(|kind| kind == &JsValue::from("mcp"))
    {
        truthy(&tool.is_read_only(&JsValue::Object(JsObject::new()))?)
    } else {
        listed(&ASK_MODE_ALLOWED_TOOLS, name)
    };
    if !allowed {
        return Ok(Some(ask_mode_tool_violation_message(name)));
    }
    if name.equals_str("bash") {
        let command = string_property(input, "command");
        if !is_read_only_command(command.as_ref()) {
            return Ok(Some(ask_mode_bash_violation_message(
                &command.unwrap_or_default(),
            )));
        }
        return Ok(None);
    }
    if !truthy(&tool.is_read_only(input)?) && !name.equals_str("agent") {
        return Ok(Some(ask_mode_tool_violation_message(name)));
    }
    Ok(None)
}

fn concat(parts: &[&[u16]]) -> JsString {
    JsString::from_utf16(parts.concat())
}

fn units(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// `buildAskModeViolationMessage` (askModeConstraints.ts:42-50).
fn ask_mode_tool_violation_message(name: &JsString) -> JsString {
    concat(&[
        &units("[ASK_MODE_VIOLATION] Tool \""),
        name.units(),
        &units(
            "\" is BLOCKED in ask mode.\n\nAsk mode is read-only. Tools may inspect files, search, ask questions, fetch/read external content, or launch read-only subagents, but they cannot modify files or create side effects.\n\nDo NOT retry this tool. It will fail again while ask mode is active.",
        ),
    ])
}

/// `buildAskModeBashViolationMessage` (askModeConstraints.ts:53-60).
fn ask_mode_bash_violation_message(command: &JsString) -> JsString {
    concat(&[
        &units("[ASK_MODE_VIOLATION] bash command \""),
        permission::truncate_command_120(command).units(),
        &units(
            "\" is BLOCKED because it is not read-only.\n\nIn ask mode, bash is restricted to read-only commands only (pwd, ls, cat, head, wc, git status, git diff, git log, git show, find without write actions, etc.).",
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::modules::{capability::ToolDefinition, tool_context::ToolRuntimeContext};
    use crate::runtime::tools::{ToolFuture, ToolOutput};
    use std::sync::Arc;

    struct Fake {
        fields: JsObject,
        read_only_empty: bool,
        read_only_input: bool,
    }
    impl ToolDefinition for Fake {
        fn fields(&self) -> &JsObject {
            &self.fields
        }
        fn is_read_only(&self, input: &JsValue) -> Result<JsValue, HostError> {
            let empty = input.as_object().is_some_and(|o| o.entries().is_empty());
            Ok(JsValue::Bool(if empty {
                self.read_only_empty
            } else {
                self.read_only_input
            }))
        }
    }
    impl ExecutableTool for Fake {
        fn execute(&self, _input: JsValue, _context: ToolRunContext) -> ToolFuture<ToolOutput> {
            unreachable!("the gate never executes")
        }
    }
    fn tool(name: &str, kind: &str, read_only_empty: bool, read_only_input: bool) -> Fake {
        let mut fields = JsObject::new();
        fields.insert("name".into(), name.into());
        fields.insert("kind".into(), kind.into());
        Fake {
            fields,
            read_only_empty,
            read_only_input,
        }
    }
    fn context(permission_mode: &str, run_mode: &str) -> ToolRunContext {
        let mut fields = JsObject::new();
        fields.insert("cwd".into(), "/w".into());
        fields.insert("permissionMode".into(), permission_mode.into());
        fields.insert("runMode".into(), run_mode.into());
        ToolRunContext::new(Arc::new(ToolRuntimeContext {
            files: crate::host::modules::tool_context::HostToolCheckpoint::new(None, &[])
                .unwrap()
                .tool_context_state(),
            fields,
            abort_signal: None,
            model: None,
            elicitation: None,
            user_dialog: None,
            audit_recorder: None,
            file_history: None,
            file_update_notifier: None,
            now: None,
            progress: None,
            subagent: None,
            plan_todo: None,
            goal: None,
            plan_directory: None,
        }))
    }
    fn input(key: &str, value: &str) -> JsValue {
        let mut object = JsObject::new();
        object.insert(key.into(), value.into());
        JsValue::Object(object)
    }
    fn text(value: &JsString) -> String {
        String::from_utf16_lossy(value.units())
    }

    /// askModeConstraints.ts:36-41: an MCP tool is admitted by
    /// `isReadOnly({})`, then still needs `isReadOnly(input)`.
    #[test]
    fn ask_mode_judges_mcp_tools_by_their_read_only_predicates() {
        let ask = context("default", "ask");
        let name: JsString = "mcp__srv__op".into();
        let arguments = input("x", "a");
        let refused = tool("mcp__srv__op", "mcp", false, true);
        let message = ask_mode_violation(&refused, &name, &arguments, &ask)
            .unwrap()
            .expect("an MCP tool that is not read-only for {} is blocked");
        assert!(
            text(&message)
                .starts_with("[ASK_MODE_VIOLATION] Tool \"mcp__srv__op\" is BLOCKED in ask mode.")
        );
        let writes = tool("mcp__srv__op", "mcp", true, false);
        assert!(
            ask_mode_violation(&writes, &name, &arguments, &ask)
                .unwrap()
                .is_some()
        );
        let reads = tool("mcp__srv__op", "mcp", true, true);
        assert_eq!(
            ask_mode_violation(&reads, &name, &arguments, &ask).unwrap(),
            None
        );
        // Outside ask mode the gate is inert; plan mode's allow list has no
        // MCP exception.
        assert_eq!(
            ask_mode_violation(&refused, &name, &arguments, &context("default", "agent")).unwrap(),
            None
        );
        assert!(
            plan_mode_violation(&name, &arguments, &context("plan", "plan"))
                .unwrap()
                .is_some()
        );
    }

    /// askModeConstraints.ts:77: `agent` passes even when it reports a write.
    #[test]
    fn ask_mode_admits_agent_regardless_of_read_only() {
        let agent = tool("agent", "session", false, false);
        assert_eq!(
            ask_mode_violation(
                &agent,
                &"agent".into(),
                &input("x", "a"),
                &context("default", "ask")
            )
            .unwrap(),
            None
        );
    }

    /// planModeConstraints.ts:51 / askModeConstraints.ts:54: commands over
    /// 120 UTF-16 units are cut to 120 and suffixed with an ellipsis; a
    /// missing command quotes as empty.
    #[test]
    fn bash_violation_messages_truncate_long_commands() {
        let arguments = input("command", &format!("touch {}", "é".repeat(130)));
        let quoted = format!("\"touch {}…\"", "é".repeat(114));
        let bash = tool("bash", "shell", false, false);
        let ask = ask_mode_violation(
            &bash,
            &"bash".into(),
            &arguments,
            &context("default", "ask"),
        )
        .unwrap()
        .unwrap();
        assert!(text(&ask).contains(&quoted), "{}", text(&ask));
        let plan = plan_mode_violation(&"bash".into(), &arguments, &context("plan", "agent"))
            .unwrap()
            .unwrap();
        assert!(text(&plan).contains(&quoted), "{}", text(&plan));
        let empty = plan_mode_violation(&"bash".into(), &JsValue::Null, &context("plan", "agent"))
            .unwrap()
            .unwrap();
        assert!(text(&empty).starts_with("[PLAN_MODE_VIOLATION] bash command \"\" is BLOCKED"));
    }
}
