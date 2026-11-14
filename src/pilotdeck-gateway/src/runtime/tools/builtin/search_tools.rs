//! `search_tools` (baseline `tool/builtin/searchTools.ts`): the session-local
//! catalog of the SDK `deferredTools`, hidden from the initial schema. A
//! matching search reveals the tools in the session registry, so the next
//! model request and the tool runtime see them through their ordinary paths.
use super::{object, string, text, tool_error};
use crate::{
    host::{HostError, modules::capability::ToolDefinition},
    runtime::tools::{
        ExecutableTool, ToolFuture, ToolOutput, ToolRegistry, ToolRunContext,
        registry::WeakToolRegistry,
    },
};
use pilotdeck_protocol::{JsObject, JsString, JsValue, stringify_json};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

pub const SEARCH_TOOLS_TOOL_NAME: &str = "search_tools";

/// `DeferredToolSearchEntry`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeferredToolEntry {
    pub name: JsString,
    pub description: JsString,
    pub search_hint: Option<JsString>,
}

/// The names a session's searches revealed. The native session composes a
/// registry per turn, so the reveals outlive the registry they were made in.
pub type RevealedTools = Arc<Mutex<BTreeSet<JsString>>>;

/// `collectDeferredTools` + the hide / `search_tools` registration
/// (cli/ProjectSessionRuntimeBundle.ts:348-370,745-775): every requested
/// name must be a tool of this session (by name, not alias); the entries are
/// unique (the last request for a name wins its hint) and sorted with
/// `localeCompare`. Each deferred tool is hidden unless an earlier search of
/// this session revealed it, and `search_tools` joins the registry.
pub fn apply_deferred_tools(
    registry: &ToolRegistry,
    requested: &[(JsString, Option<JsString>)],
    revealed: &RevealedTools,
) -> Result<(), String> {
    if requested.is_empty() {
        return Ok(());
    }
    let tools = registry.list_all().map_err(|error| error.to_string())?;
    let mut entries: Vec<DeferredToolEntry> = Vec::new();
    for (name, hint) in requested {
        let Some(tool) = tools.iter().find(|tool| {
            tool.fields()
                .get("name")
                .and_then(JsValue::as_string)
                .is_some_and(|candidate| candidate == name)
        }) else {
            return Err(format!(
                "SDK deferred tool {} is not available in this session.",
                text(name)
            ));
        };
        let entry = DeferredToolEntry {
            name: name.clone(),
            description: tool
                .fields()
                .get("description")
                .and_then(JsValue::as_string)
                .cloned()
                .unwrap_or_default(),
            search_hint: hint.clone().filter(|hint| !hint.is_empty()),
        };
        match entries
            .iter_mut()
            .find(|existing| existing.name == entry.name)
        {
            Some(existing) => *existing = entry,
            None => entries.push(entry),
        }
    }
    entries
        .sort_by(|left, right| super::grep::locale_compare(&text(&left.name), &text(&right.name)));
    if registry.has(&JsString::from(SEARCH_TOOLS_TOOL_NAME)) {
        return Err(
            "Deferred SDK tools conflict with an existing search_tools registration.".into(),
        );
    }
    {
        let revealed = revealed.lock().unwrap_or_else(|e| e.into_inner());
        for entry in &entries {
            if !revealed.contains(&entry.name) {
                registry.hide(&entry.name);
            }
        }
    }
    registry
        .register(Arc::new(SearchToolsTool::new(
            registry.downgrade(),
            entries,
            Arc::clone(revealed),
        )))
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub struct SearchToolsTool {
    fields: JsObject,
    registry: WeakToolRegistry,
    entries: Vec<DeferredToolEntry>,
    revealed: RevealedTools,
}

impl SearchToolsTool {
    /// `entries` are already unique and sorted by name (`collectDeferredTools`).
    pub fn new(
        registry: WeakToolRegistry,
        entries: Vec<DeferredToolEntry>,
        revealed: RevealedTools,
    ) -> Self {
        Self {
            fields: object([
                ("name", string(SEARCH_TOOLS_TOOL_NAME)),
                (
                    "description",
                    string(
                        "Search deferred tools by capability. Matching tools become available for the next model request.",
                    ),
                ),
                ("kind", string("session")),
                (
                    "inputSchema",
                    JsValue::Object(super::schema(
                        r#"{"type":"object","required":["query"],"additionalProperties":false,"properties":{"query":{"type":"string","description":"The capability or task to search for."}}}"#,
                    )),
                ),
            ]),
            registry,
            entries,
            revealed,
        }
    }
}

impl ToolDefinition for SearchToolsTool {
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

impl ExecutableTool for SearchToolsTool {
    fn execute(&self, input: JsValue, _context: ToolRunContext) -> ToolFuture<ToolOutput> {
        let query = match input.get("query") {
            Some(JsValue::String(query)) => {
                JsString::from_utf16(crate::runtime::memory::js_trim_units(query.units()).to_vec())
            }
            _ => JsString::default(),
        };
        let result = if query.is_empty() {
            Err(tool_error(
                "invalid_tool_input",
                "search_tools requires a non-empty query.",
            ))
        } else {
            let matches = find_matches(&self.entries, &query);
            {
                let mut revealed = self.revealed.lock().unwrap_or_else(|e| e.into_inner());
                let registry = self.registry.upgrade();
                for entry in &matches {
                    if let Some(registry) = &registry {
                        registry.reveal(&entry.name);
                    }
                    revealed.insert(entry.name.clone());
                }
            }
            Ok(output(&query, &matches))
        };
        Box::pin(async move { result })
    }
}

fn output(query: &JsString, matches: &[&DeferredToolEntry]) -> ToolOutput {
    let quoted = stringify_json(&JsValue::String(query.clone()))
        .ok()
        .flatten()
        .unwrap_or_default();
    let text_out = if matches.is_empty() {
        JsString::from(format!("No deferred tools matched {quoted}."))
    } else {
        let mut units: Vec<u16> = format!(
            "Loaded {} deferred tool{}:",
            matches.len(),
            if matches.len() == 1 { "" } else { "s" }
        )
        .encode_utf16()
        .collect();
        for entry in matches {
            units.extend("\n- ".encode_utf16());
            units.extend_from_slice(entry.name.units());
            units.extend(": ".encode_utf16());
            units.extend_from_slice(entry.description.units());
        }
        units.extend("\nUse the loaded tool names in a subsequent tool call.".encode_utf16());
        JsString::from_utf16(units)
    };
    let tools = matches
        .iter()
        .map(|entry| {
            let mut tool = object([
                ("name", JsValue::String(entry.name.clone())),
                ("description", JsValue::String(entry.description.clone())),
            ]);
            if let Some(hint) = &entry.search_hint {
                tool.insert("searchHint".into(), JsValue::String(hint.clone()));
            }
            JsValue::Object(tool)
        })
        .collect();
    ToolOutput {
        content: vec![JsValue::Object(object([
            ("type", string("text")),
            ("text", JsValue::String(text_out)),
        ]))],
        supplemental_messages: None,
        data: Some(JsValue::Object(object([
            ("query", JsValue::String(query.clone())),
            ("tools", JsValue::Array(tools)),
        ]))),
        metadata: None,
    }
}

/// Preserve isolated surrogates while retaining the previous lowercase
/// behavior for valid Unicode runs.
fn lower_units(value: &[u16]) -> Vec<u16> {
    let mut result = Vec::new();
    let mut run = String::new();
    for scalar in char::decode_utf16(value.iter().copied()) {
        match scalar {
            Ok(ch) => run.push(ch),
            Err(error) => {
                result.extend(run.to_lowercase().encode_utf16());
                run.clear();
                result.push(error.unpaired_surrogate());
            }
        }
    }
    result.extend(run.to_lowercase().encode_utf16());
    result
}

fn contains_units(haystack: &[u16], needle: &[u16]) -> bool {
    needle.is_empty()
        || haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// `findMatches`: whole-query substring first, then all whitespace tokens.
fn find_matches<'a>(
    entries: &'a [DeferredToolEntry],
    query: &JsString,
) -> Vec<&'a DeferredToolEntry> {
    let normalized = lower_units(query.units());
    let exact: Vec<_> = entries
        .iter()
        .filter(|entry| contains_units(&search_text(entry), &normalized))
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    let tokens: Vec<&[u16]> = normalized
        .split(|unit| crate::runtime::memory::js_trim_units(std::slice::from_ref(unit)).is_empty())
        .filter(|token| !token.is_empty())
        .collect();
    if tokens.is_empty() {
        return Vec::new();
    }
    entries
        .iter()
        .filter(|entry| {
            let text = search_text(entry);
            tokens.iter().all(|token| contains_units(&text, token))
        })
        .collect()
}

fn search_text(entry: &DeferredToolEntry) -> Vec<u16> {
    let mut units = entry.name.units().to_vec();
    units.push(u16::from(b' '));
    units.extend_from_slice(entry.description.units());
    units.push(u16::from(b' '));
    if let Some(hint) = &entry.search_hint {
        units.extend_from_slice(hint.units());
    }
    lower_units(&units)
}

#[cfg(test)]
mod utf16_tests {
    use super::*;
    use crate::host::modules::tool_context::{ToolFileState, ToolRuntimeContext};

    fn context() -> ToolRunContext {
        ToolRunContext::new(Arc::new(ToolRuntimeContext {
            fields: JsObject::new(),
            files: ToolFileState {
                read_file_state: Arc::default(),
                write_snapshots: Arc::default(),
                allowed_read_files: vec![],
            },
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

    #[tokio::test]
    async fn actual_tool_search_preserves_utf16_and_exact_reveals() {
        // Fixed TS createDeferredToolSearchTool, two identical actual runs:
        // _tmp/20261114-search-tools-utf16/ts-second.json.
        let units = |prefix: &str, unit: u16, suffix: &str| {
            JsString::from_utf16(
                prefix
                    .encode_utf16()
                    .chain([unit])
                    .chain(suffix.encode_utf16())
                    .collect(),
            )
        };
        let entries = [("raw", 0xd800), ("replacement", 0xfffd), ("raw2", 0xd801)]
            .into_iter()
            .map(|(name, value)| DeferredToolEntry {
                name: name.into(),
                description: units("prefix", value, "tail"),
                search_hint: None,
            })
            .collect::<Vec<_>>();
        for (input, expected_unit, expected_name) in [
            (vec![0xd800], 0xd800, Some("raw")),
            (vec![0xd801], 0xd801, Some("raw2")),
            (vec![0xfffd], 0xfffd, Some("replacement")),
            (vec![32, 32, 0xd800, 32, 32], 0xd800, Some("raw")),
            (vec![0xd802], 0xd802, None),
        ] {
            let registry = ToolRegistry::new(None, None);
            let revealed = RevealedTools::default();
            let tool =
                SearchToolsTool::new(registry.downgrade(), entries.clone(), revealed.clone());
            let result = tool
                .execute(
                    JsValue::Object(object([(
                        "query",
                        JsValue::String(JsString::from_utf16(input)),
                    )])),
                    context(),
                )
                .await
                .unwrap();
            let data = result.data.as_ref().unwrap();
            assert_eq!(
                data.get("query")
                    .and_then(JsValue::as_string)
                    .unwrap()
                    .units(),
                &[expected_unit]
            );
            let Some(JsValue::Array(tools)) = data.get("tools") else {
                panic!("tools array")
            };
            let names = tools
                .iter()
                .map(|v| {
                    v.get("name")
                        .and_then(JsValue::as_string)
                        .unwrap()
                        .to_utf8()
                        .unwrap()
                })
                .collect::<Vec<_>>();
            assert_eq!(names, expected_name.into_iter().collect::<Vec<_>>());
            assert_eq!(
                revealed.lock().unwrap().iter().cloned().collect::<Vec<_>>(),
                expected_name
                    .into_iter()
                    .map(JsString::from)
                    .collect::<Vec<_>>()
            );
            let content = result.content[0]
                .get("text")
                .and_then(JsValue::as_string)
                .unwrap();
            if let Some(name) = expected_name {
                let expected = units(
                    &format!("Loaded 1 deferred tool:\n- {name}: prefix"),
                    expected_unit,
                    "tail\nUse the loaded tool names in a subsequent tool call.",
                );
                assert_eq!(content, &expected);
            } else {
                assert!(content.equals_str(r#"No deferred tools matched "\ud802"."#));
            }
        }
    }
    #[tokio::test]
    async fn scalar_text_matches_fixed_ts_output() {
        let registry = ToolRegistry::new(None, None);
        let revealed = RevealedTools::default();
        let tool = SearchToolsTool::new(
            registry.downgrade(),
            vec![DeferredToolEntry {
                name: "build".into(),
                description: "a needle".into(),
                search_hint: None,
            }],
            revealed.clone(),
        );
        let result = tool
            .execute(
                JsValue::Object(object([("query", JsValue::from("  NeEdLe  "))])),
                context(),
            )
            .await
            .unwrap();
        assert_eq!(
            result.content,
            vec![parse_expected(
                r#"{"type":"text","text":"Loaded 1 deferred tool:\n- build: a needle\nUse the loaded tool names in a subsequent tool call."}"#,
            )]
        );
        assert_eq!(
            result.data,
            Some(parse_expected(
                r#"{"query":"NeEdLe","tools":[{"name":"build","description":"a needle"}]}"#,
            ))
        );
        assert_eq!(
            revealed.lock().unwrap().iter().cloned().collect::<Vec<_>>(),
            vec![JsString::from("build")]
        );
    }

    fn parse_expected(value: &str) -> JsValue {
        pilotdeck_protocol::parse_json(value).unwrap()
    }
}
