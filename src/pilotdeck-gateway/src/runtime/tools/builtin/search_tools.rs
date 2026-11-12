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
            Some(JsValue::String(query)) => js_trim(&text(query)).to_owned(),
            _ => String::new(),
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

fn js_trim(value: &str) -> &str {
    crate::metadata::js_trim(value)
}

fn output(query: &str, matches: &[&DeferredToolEntry]) -> ToolOutput {
    let quoted = stringify_json(&JsValue::from(query))
        .ok()
        .flatten()
        .unwrap_or_default();
    let text_out = if matches.is_empty() {
        format!("No deferred tools matched {quoted}.")
    } else {
        let mut lines = vec![format!(
            "Loaded {} deferred tool{}:",
            matches.len(),
            if matches.len() == 1 { "" } else { "s" }
        )];
        for entry in matches {
            lines.push(format!(
                "- {}: {}",
                text(&entry.name),
                text(&entry.description)
            ));
        }
        lines.push("Use the loaded tool names in a subsequent tool call.".to_owned());
        lines.join("\n")
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
            ("text", string(text_out)),
        ]))],
        supplemental_messages: None,
        data: Some(JsValue::Object(object([
            ("query", JsValue::from(query)),
            ("tools", JsValue::Array(tools)),
        ]))),
        metadata: None,
    }
}

/// `findMatches`: every entry whose name, description and hint contain the
/// whole query; otherwise those containing every whitespace-separated token.
fn find_matches<'a>(entries: &'a [DeferredToolEntry], query: &str) -> Vec<&'a DeferredToolEntry> {
    let normalized = query.to_lowercase();
    let exact: Vec<_> = entries
        .iter()
        .filter(|entry| search_text(entry).contains(&normalized))
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    let tokens: Vec<&str> = normalized.split_whitespace().collect();
    if tokens.is_empty() {
        return Vec::new();
    }
    entries
        .iter()
        .filter(|entry| {
            let text = search_text(entry);
            tokens.iter().all(|token| text.contains(token))
        })
        .collect()
}

fn search_text(entry: &DeferredToolEntry) -> String {
    format!(
        "{} {} {}",
        text(&entry.name),
        text(&entry.description),
        entry.search_hint.as_ref().map(text).unwrap_or_default()
    )
    .to_lowercase()
}
