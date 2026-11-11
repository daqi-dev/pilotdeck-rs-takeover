//! `agent` (baseline `tool/builtin/agent.ts`): dispatches a bounded subtask
//! to a subagent that runs its own agent loop, and hands the subagent's final
//! report back as the tool result.
//!
//! The tool itself owns only the input contract and the result shape. The
//! fork — child session, sidechain transcript, events — belongs to the host
//! and is reached through `context.subagent` ([`ForkApi`]), the counterpart of
//! the baseline's `PilotDeckSubagentForkApi`. A tool runtime without that port
//! cannot run the tool; the baseline's single-shot fallback (one model call
//! against the legacy presets) exists only for stand-alone runtimes and tests
//! and has no caller in the gateway.
//!
//! The built-in subagent presets (`agent/sub/builtinSubagentTypes.ts`) live
//! here too, because the tool description is rendered from them. Prompt and
//! description texts are the baseline's, verbatim.
use super::{field, object, schema, string, text};
use crate::{
    host::{HostError, modules::capability::ToolDefinition},
    runtime::{
        permission::{PermissionContext, PermissionFuture},
        tools::{ExecutableTool, ToolError, ToolFuture, ToolOutput, ToolRunContext, js_space},
    },
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::{future::Future, pin::Pin, sync::Arc};

/// `DEFAULT_SUBAGENT_TIMEOUT_MS` (`tool/protocol/subagentTimeout.ts`).
pub const DEFAULT_SUBAGENT_TIMEOUT_MS: f64 = 3_600_000.0;

/// `SubagentDefinition`: the fields the built-in presets set.
#[derive(Clone, Debug, PartialEq)]
pub struct SubagentDefinition {
    pub id: &'static str,
    pub description: &'static str,
    /// Canonical tool names; `["*"]` is every parent tool.
    pub allowed_tools: &'static [&'static str],
    pub omit_project_instructions: bool,
    pub omit_git_status: bool,
    pub is_read_only: bool,
    pub system_prompt_suffix: &'static str,
}

const SHARED_PREFIX: &str = "You are a subagent of PilotDeck — a focused agent dispatched by the parent agent to handle a bounded research, planning, or verification task.

Strengths:
- You always have the full context of the parent task and can inspect the parent's tool history.
- You return a single concise final report (no follow-up questions).
- You never ask clarifying questions back; do your best with the information given.

Guidelines:
1. Stay strictly within the directive given by the parent.
2. Do NOT create files unless the directive explicitly asks you to.
3. NEVER proactively create documentation or README files.
4. Run only the tools listed in your allowed tool set; never attempt restricted ones.
5. If the directive asks you to write/save files, do that BEFORE producing your final report.
   File writing is part of the task, not optional.
6. The final assistant message MUST follow the output format below verbatim.
7. Keep the response under 4 KB unless the directive demands more.
8. Use absolute paths when referencing files.
9. Trust the parent's directive: do not re-question its premises.
10. If the directive is impossible with the allowed tools, say so explicitly in the report.
11. Prefer fewer tool calls. Do not use web_fetch unless the directive explicitly requires full page content.
12. When the directive provides specific file paths, trust them and use them directly. Do not spend turns searching for or verifying file paths that are already given.

Output format (mandatory; missing any field fails the run):
Scope: <one sentence describing what you did>
Result: <findings, in markdown if helpful>
Key files: <comma-separated absolute paths or \"none\">
Files changed: <list with rationale, or \"none\">
Issues: <list of caveats / blockers, or \"none\">";

/// `SUBAGENT_DEFINITIONS`, in the baseline's key order.
pub const SUBAGENT_DEFINITIONS: [SubagentDefinition; 4] = [
    SubagentDefinition {
        id: "general-purpose",
        description: "General-purpose subagent for complex research/synthesis tasks. Has broad parent-tool access; nested delegation follows the configured depth cap.",
        allowed_tools: &["*"],
        omit_project_instructions: false,
        omit_git_status: false,
        is_read_only: false,
        system_prompt_suffix: "You have full access to the parent's tool registry. Use any tool the parent has permission to use, but stay within your directive.",
    },
    SubagentDefinition {
        id: "explore",
        description: "Read-only exploration subagent. Inspects files, runs grep/glob, and may run safe shell commands. Cannot edit files.",
        allowed_tools: &["read_file", "grep", "glob", "bash"],
        omit_project_instructions: true,
        omit_git_status: true,
        is_read_only: true,
        system_prompt_suffix: "Read-only mode: you may inspect files (`read_file`), search (`grep` / `glob`), and run safe shell commands (`bash`), but never write or delete. Prefer `grep` for content search and `glob` for file discovery; do not fall back to `bash` `grep` / `rg` unless the dedicated tools cannot express the task. Do not propose `edit_file` / `write_file` / network calls — those will be rejected.",
    },
    SubagentDefinition {
        id: "plan",
        description: "Read-only planning subagent. Inspects code via read/grep/glob and produces a step-by-step plan.",
        allowed_tools: &["read_file", "grep", "glob"],
        omit_project_instructions: true,
        omit_git_status: true,
        is_read_only: true,
        system_prompt_suffix: "Planning mode: produce a numbered, actionable plan. You may inspect code (`read_file` / `grep` / `glob`) but you may NOT execute shell commands or modify files. Prefer `grep` for content search and `glob` for file discovery instead of describing shell-based search workarounds.",
    },
    SubagentDefinition {
        id: "verify",
        description: "Verification subagent. Inspects generated artifacts (images, HTML, PDFs) for correctness. Can read files, run shell commands, and search code, but cannot modify files.",
        allowed_tools: &["read_file", "grep", "glob", "bash"],
        omit_project_instructions: true,
        omit_git_status: true,
        is_read_only: true,
        system_prompt_suffix: "Verification mode: your job is to **find problems**, not confirm success. Try to break the implementation.

Approach:
1. Read and inspect the generated artifacts (files, images, screenshots, HTML pages).
2. Run validation commands (e.g. check file sizes, run linters, verify encoding).
3. For images: use `read_file` on image files to visually inspect them (if the model supports multimodal input). Look for: missing/garbled text (tofu boxes), layout overflow, color contrast issues, truncated content.
4. For HTML: check that the file is well-formed, that links and assets resolve, and that i18n/RTL is correct.
5. For data files: verify schema, required fields, encoding (UTF-8), and sanity of values.

Output your findings as a structured verdict:
- PASS: all checks passed, no issues found.
- PARTIAL: some checks passed but minor issues exist (list them).
- FAIL: critical issues found (list them with file paths and descriptions).

Be rigorous. A silent pass when issues exist is worse than a false alarm.",
    },
];

/// `getSubagentDefinition`.
pub fn subagent_definition(id: &str) -> Option<&'static SubagentDefinition> {
    SUBAGENT_DEFINITIONS
        .iter()
        .find(|definition| definition.id == id)
}

/// `buildSubagentSystemPrompt`.
pub fn subagent_system_prompt(definition: &SubagentDefinition) -> String {
    format!("{SHARED_PREFIX}\n\n{}", definition.system_prompt_suffix)
}

/// `buildAgentToolDescription(maxSubagentDepth, subagentDepth)`: the
/// description names what the *next* child may do under the depth cap.
pub fn agent_tool_description(max_subagent_depth: f64, subagent_depth: f64) -> String {
    let child_can_delegate = subagent_depth.max(0.0) + 1.0 < max_subagent_depth;
    let types = ["general-purpose", "explore", "plan"]
        .iter()
        .map(|id| {
            let definition = subagent_definition(id).expect("public preset");
            let summary = if *id == "general-purpose" && !child_can_delegate {
                "General-purpose subagent for complex research/synthesis tasks. Has broad parent-tool access except nested subagent launch."
            } else {
                definition.description
            };
            let tools = if definition.allowed_tools.first() == Some(&"*") {
                if child_can_delegate {
                    "all parent tools; nested delegation is available within the configured depth cap".to_owned()
                } else {
                    "all parent tools except nested agent launch".to_owned()
                }
            } else {
                definition.allowed_tools.join(", ")
            };
            format!("- {id}: {summary} Tools: {tools}.")
        })
        .collect::<Vec<_>>()
        .join("\n");
    [
        "Launch a new subagent to handle a focused multi-step task.",
        "",
        "Use this tool when a bounded piece of work would benefit from an autonomous helper instead of keeping every intermediate step in the parent agent's context.",
        "",
        "Provide:",
        "- `description`: a short 3-5 word label for the task.",
        "- `prompt`: the full directive for the subagent. Write it like a complete briefing: include the goal, relevant context, constraints, and what good output looks like.",
        "- `subagent_type` (optional): choose a built-in preset. If omitted, `general-purpose` is used.",
        "",
        "Available built-in subagent types:",
        &types,
        "",
        "The subagent returns one structured report with these sections: `Scope`, `Result`, `Key files`, `Files changed`, and `Issues`.",
        "",
        "Runtime behavior:",
        "- Multiple independent agent calls in one assistant message may run concurrently; batch sibling investigations when their scopes do not depend on each other.",
        "- Inside the AgentLoop, this runs a real forked subagent with its own scoped tool loop.",
        "- In stand-alone runtimes and some tests, it falls back to a single model call that preserves the same high-level subagent intent.",
    ]
    .join("\n")
}

const INPUT_SCHEMA: &str = r#"{"type":"object","required":["description","prompt"],"additionalProperties":false,"properties":{"description":{"type":"string","description":"Short 3-5 word task summary used to label the subagent run."},"prompt":{"type":"string","description":"Detailed directive for the subagent. Include the goal, relevant context, constraints, and desired output; do not assume the subagent already knows why the task matters."},"subagent_type":{"type":"string","description":"Optional subagent preset. Public built-ins: 'general-purpose' (full tool access), 'explore' (read-only investigation with read_file/grep/glob/bash), or 'plan' (read-only planning with read_file/grep/glob). Some runtimes may also expose additional presets such as 'verify'. Defaults to 'general-purpose' when omitted. Legacy 'general_purpose' is still accepted for compatibility."},"subagentType":{"type":"string","description":"Deprecated legacy alias for subagent_type. Prefer subagent_type."}}}"#;

/// One fork request (`PilotDeckSubagentForkApi.fork` input).
pub struct ForkRequest {
    pub definition_id: JsString,
    pub directive: JsString,
    pub subagent_id: JsString,
    pub tool_call_id: Option<JsString>,
    pub timeout_ms: f64,
}

/// `SubagentReport` as the fork returns it to the tool.
#[derive(Clone, Debug)]
pub struct ForkReport {
    pub markdown: JsString,
    pub usage: JsValue,
    pub turns: JsValue,
    pub duration_ms: f64,
    pub parsed: Option<JsObject>,
    pub subagent_session_id: JsString,
    pub transcript_relative_path: Option<JsString>,
}

pub type ForkFuture = Pin<Box<dyn Future<Output = Result<ForkReport, JsString>> + Send>>;

/// The host's fork port (`PilotDeckSubagentForkApi`), bound to one parent
/// turn. `fork` settles with the child's report or the failure message.
pub trait SubagentFork: Send + Sync {
    fn depth(&self) -> f64;
    fn max_subagent_depth(&self) -> f64;
    /// `listDefinitions().map(d => d.id)`.
    fn definition_ids(&self) -> Vec<JsString>;
    fn is_allowed_definition(&self, id: &JsString) -> bool {
        self.definition_ids().contains(id)
    }
    /// The context is the calling tool's: the fork watches its abort signal.
    fn fork(&self, request: ForkRequest, context: ToolRunContext) -> ForkFuture;
    /// `uuid()`: the new subagent's id.
    fn subagent_id(&self) -> JsString;
}

/// The concrete type stored in `ToolRuntimeContext::subagent`.
pub struct ForkApi(pub Arc<dyn SubagentFork>);

pub struct AgentTool {
    fields: JsObject,
}

impl AgentTool {
    /// `createAgentTool({ maxSubagentDepth, subagentDepth })`.
    pub fn new(max_subagent_depth: f64, subagent_depth: f64) -> Self {
        Self {
            fields: object([
                ("name", string("agent")),
                (
                    "aliases",
                    JsValue::Array(vec![string("Agent"), string("Task")]),
                ),
                (
                    "description",
                    string(agent_tool_description(max_subagent_depth, subagent_depth)),
                ),
                ("kind", string("agent")),
                (
                    "requiredRuntimeCapabilities",
                    JsValue::Array(vec![string("subagent_fork")]),
                ),
                ("inputSchema", JsValue::Object(schema(INPUT_SCHEMA))),
                ("maxResultBytes", JsValue::Number(200_000.0)),
            ]),
        }
    }
}

impl ToolDefinition for AgentTool {
    fn fields(&self) -> &JsObject {
        &self.fields
    }
    fn is_read_only(&self, _input: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(false))
    }
    fn is_concurrency_safe(&self, _input: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(true))
    }
    fn check_permissions<'a>(
        &'a self,
        _input: &'a JsValue,
        _context: &'a PermissionContext,
    ) -> PermissionFuture<'a, Option<JsValue>> {
        Box::pin(async {
            Ok(Some(JsValue::Object(object([
                ("type", string("allow")),
                (
                    "reason",
                    JsValue::Object(object([
                        ("type", string("tool")),
                        ("toolName", string("agent")),
                        (
                            "message",
                            string("Subagent invocation is allowed without prompting."),
                        ),
                    ])),
                ),
            ]))))
        })
    }
    fn required_runtime_capabilities(&self) -> Option<Vec<JsString>> {
        Some(vec!["subagent_fork".into()])
    }
}

/// `normalizeRequestedSubagentType`: the trimmed value, with the spellings of
/// the built-in presets folded to their ids. Blank is "not given".
fn normalize_requested_type(value: &JsValue) -> Option<String> {
    let trimmed = text(value.as_string()?).trim_matches(js_space).to_owned();
    if trimmed.is_empty() {
        return None;
    }
    Some(match trimmed.to_lowercase().as_str() {
        "general-purpose" | "general_purpose" | "general purpose" => "general-purpose".to_owned(),
        "explore" | "explorer" => "explore".to_owned(),
        "plan" => "plan".to_owned(),
        "verify" => "verify".to_owned(),
        _ => trimmed,
    })
}

/// `parseSummary`: the five report fields, or nothing when any is missing.
/// A field's value runs to the next line that starts another field.
pub fn parse_summary(report: &str) -> Option<JsObject> {
    const FIELDS: [&str; 5] = ["Scope", "Result", "Key files", "Files changed", "Issues"];
    let lines: Vec<&str> = report.split('\n').collect();
    let starts_field = |line: &str, name: &str| {
        line.strip_prefix(name)
            .is_some_and(|rest| rest.starts_with(':'))
    };
    let mut summary = JsObject::new();
    for name in FIELDS {
        let index = lines.iter().position(|line| starts_field(line, name))?;
        let mut value = lines[index][name.len() + 1..]
            .trim_matches(js_space)
            .to_owned();
        for next in &lines[index + 1..] {
            if FIELDS.iter().any(|other| starts_field(next, other)) {
                break;
            }
            value.push('\n');
            value.push_str(next);
        }
        summary.insert(name.into(), string(value.trim_matches(js_space)));
    }
    Some(summary)
}

fn number_field(context: &ToolRunContext, key: &str) -> Option<f64> {
    context.fields.get(key).and_then(JsValue::as_number)
}

impl ExecutableTool for AgentTool {
    fn execute(&self, input: JsValue, context: ToolRunContext) -> ToolFuture<ToolOutput> {
        Box::pin(async move {
            let explicit = [
                field(&input, "subagent_type"),
                field(&input, "subagentType"),
            ]
            .into_iter()
            .find(|value| !matches!(value, JsValue::Undefined | JsValue::Null))
            .and_then(normalize_requested_type);
            let mut requested = explicit.unwrap_or_else(|| "general-purpose".to_owned());
            let plan_mode = context
                .fields
                .get("permissionContext")
                .and_then(|permission| permission.get("mode"))
                == Some(&"plan".into());
            let ask_mode = context.fields.get("runMode") == Some(&"ask".into());
            if (plan_mode || ask_mode) && requested == "general-purpose" {
                requested = "explore".to_owned();
            }
            let Some(fork) = context
                .host
                .subagent
                .clone()
                .and_then(|port| port.downcast::<ForkApi>().ok())
            else {
                return Err(ToolError::new(
                    "unsupported_tool",
                    "agent tool requires a model client. Configure dependencies.model on AgentRuntimeDependencies, pass createAgentTool({ model }), or wire context.subagent for full-fork mode.",
                ));
            };
            let fork = fork.0.clone();
            let requested_id = JsString::from(requested.as_str());
            if !fork.is_allowed_definition(&requested_id) {
                let allowed = fork
                    .definition_ids()
                    .iter()
                    .map(text)
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(ToolError::new(
                    "invalid_tool_input",
                    format!("Unknown subagent_type \"{requested}\". Available: {allowed}."),
                ));
            }
            let depth = number_field(&context, "subagentDepth").unwrap_or_else(|| fork.depth());
            let max_depth = fork.max_subagent_depth();
            if depth >= max_depth {
                let mut error = ToolError::failed(format!(
                    "subagent_depth_exceeded (depth={depth}, max={max_depth}); nested fork rejected."
                ));
                error.details = Some(object([("errorCode", string("subagent_depth_exceeded"))]));
                return Err(error);
            }
            let aborted =
                || ToolError::new("tool_aborted", "agent subagent aborted before completion.");
            let subagent_id = fork.subagent_id();
            let description = field(&input, "description").clone();
            let request = ForkRequest {
                definition_id: requested_id.clone(),
                directive: field(&input, "prompt")
                    .as_string()
                    .cloned()
                    .unwrap_or_default(),
                subagent_id: subagent_id.clone(),
                tool_call_id: context
                    .fields
                    .get("currentToolCallId")
                    .and_then(JsValue::as_string)
                    .cloned(),
                timeout_ms: number_field(&context, "subagentTimeoutMs")
                    .unwrap_or(DEFAULT_SUBAGENT_TIMEOUT_MS),
            };
            let report = match fork.fork(request, context.clone()).await {
                Ok(report) => report,
                Err(_) if context.is_aborted() => return Err(aborted()),
                Err(message) => {
                    let mut error =
                        ToolError::failed(format!("agent subagent failed: {}", text(&message)));
                    error.details =
                        Some(object([("errorCode", string("subagent_execution_failed"))]));
                    return Err(error);
                }
            };
            if context.is_aborted() {
                return Err(aborted());
            }
            let mut output = object([
                ("subagentType", string(requested_id.clone())),
                ("description", description.clone()),
                ("text", string(report.markdown.clone())),
                ("usage", report.usage.clone()),
                ("turns", report.turns.clone()),
                ("durationMs", JsValue::Number(report.duration_ms)),
            ]);
            if let Some(parsed) = &report.parsed {
                output.insert("parsed".into(), JsValue::Object(parsed.clone()));
            }
            let mut metadata = object([
                ("subagent", string(requested_id)),
                ("subagentId", string(subagent_id)),
                ("forkMode", string("full")),
                ("turns", report.turns.clone()),
                ("durationMs", JsValue::Number(report.duration_ms)),
            ]);
            for fields in [&mut output, &mut metadata] {
                if !report.subagent_session_id.is_empty() {
                    fields.insert(
                        "subagentSessionId".into(),
                        string(report.subagent_session_id.clone()),
                    );
                }
                if let Some(path) = report
                    .transcript_relative_path
                    .as_ref()
                    .filter(|path| !path.is_empty())
                {
                    fields.insert("transcriptRelativePath".into(), string(path.clone()));
                }
            }
            let label = format!(
                "[{requested}] {}\n\n{}",
                description.as_string().map(text).unwrap_or_default(),
                text(&report.markdown)
            );
            Ok(ToolOutput {
                content: vec![
                    JsValue::Object(object([("type", string("text")), ("text", string(label))])),
                    JsValue::Object(object([
                        ("type", string("json")),
                        ("value", JsValue::Object(output.clone())),
                    ])),
                ],
                supplemental_messages: None,
                data: Some(JsValue::Object(output)),
                metadata: Some(metadata),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_needs_all_five_fields_and_keeps_continuation_lines() {
        let parsed = parse_summary(
            "Scope: planned.\nResult: 1. do it\n2. check it\nKey files: none\nFiles changed: none\nIssues: none",
        )
        .expect("five fields");
        assert_eq!(parsed.get("Result"), Some(&"1. do it\n2. check it".into()));
        assert_eq!(parsed.get("Scope"), Some(&"planned.".into()));
        assert!(parse_summary("I found one yaml file.").is_none());
        assert!(parse_summary("Scope: a\nResult: b\nKey files: c\nFiles changed: d").is_none());
    }

    #[test]
    fn requested_type_folds_preset_spellings_and_keeps_unknown_ones() {
        let normalize = |value: &str| normalize_requested_type(&value.into());
        assert_eq!(normalize(" Plan "), Some("plan".into()));
        assert_eq!(normalize("general_purpose"), Some("general-purpose".into()));
        assert_eq!(normalize("Explorer"), Some("explore".into()));
        assert_eq!(normalize(" Nope "), Some("Nope".into()));
        assert_eq!(normalize("   "), None);
    }

    #[test]
    fn description_follows_the_depth_cap() {
        let capped = agent_tool_description(1.0, 0.0);
        assert!(capped.contains("- general-purpose: General-purpose subagent for complex research/synthesis tasks. Has broad parent-tool access except nested subagent launch. Tools: all parent tools except nested agent launch."));
        assert!(capped.contains("- plan: Read-only planning subagent. Inspects code via read/grep/glob and produces a step-by-step plan. Tools: read_file, grep, glob."));
        let nested = agent_tool_description(2.0, 0.0);
        assert!(nested.contains("Tools: all parent tools; nested delegation is available within the configured depth cap."));
    }
}
