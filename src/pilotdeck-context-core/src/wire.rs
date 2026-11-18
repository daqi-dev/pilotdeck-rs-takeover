//! Canonical wire-message helpers shared by pure algorithms and host adapters.
use crate::tool_pair::is_media_reference_with_id;
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::collections::{HashMap, HashSet};

/// TS `DEFAULT_PROTECTED_TOOL_RESULT_NAMES` (`protectedContext.ts`).
pub const DEFAULT_PROTECTED_TOOL_RESULT_NAMES: &[&str] = &[
    "read_skill",
    "ReadSkill",
    "ask_user_question",
    "AskUserQuestion",
    "todo_write",
    "TodoWrite",
    "structured_output",
    "StructuredOutput",
    "agent",
    "Agent",
    "Task",
    "task_create",
    "TaskCreate",
    "task_list",
    "TaskList",
    "task_output",
    "TaskOutput",
    "task_wait",
    "TaskWait",
    "task_stop",
    "TaskStop",
];

pub static UNDEFINED: JsValue = JsValue::Undefined;

pub fn field<'a>(value: &'a JsValue, key: &str) -> &'a JsValue {
    value.get(key).unwrap_or(&UNDEFINED)
}

pub fn string_field<'a>(value: &'a JsValue, key: &str) -> Option<&'a JsString> {
    field(value, key).as_string()
}

pub fn role(message: &JsValue) -> Option<&JsString> {
    string_field(message, "role")
}

pub fn is_role(message: &JsValue, expected: &str) -> bool {
    role(message).is_some_and(|role| role.equals_str(expected))
}

pub fn content(message: &JsValue) -> &[JsValue] {
    match field(message, "content") {
        JsValue::Array(items) => items,
        _ => &[],
    }
}

pub fn block_type(block: &JsValue) -> Option<&JsString> {
    string_field(block, "type")
}

pub fn is_block(block: &JsValue, expected: &str) -> bool {
    block_type(block).is_some_and(|kind| kind.equals_str(expected))
}

/// JS `String#trim` on the UTF-16 representation, so lone surrogates survive.
pub fn trim_units(units: &[u16]) -> &[u16] {
    trim_end_units(trim_start_units(units))
}

pub fn trim_start_units(mut units: &[u16]) -> &[u16] {
    while let Some((first, rest)) = units.split_first() {
        if !is_js_whitespace(*first) {
            break;
        }
        units = rest;
    }
    units
}

pub fn trim_end_units(mut units: &[u16]) -> &[u16] {
    while let Some((last, rest)) = units.split_last() {
        if !is_js_whitespace(*last) {
            break;
        }
        units = rest;
    }
    units
}

/// ECMAScript WhiteSpace + LineTerminator over UTF-16 code units.
pub fn is_js_whitespace(unit: u16) -> bool {
    matches!(unit, 0x0009..=0x000D | 0x0020 | 0x00A0 | 0x1680 | 0x2000..=0x200A
        | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000 | 0xFEFF)
}

pub fn trim_start(value: &JsString) -> JsString {
    JsString::from_utf16(trim_start_units(value.units()).to_vec())
}

pub fn trim_end(value: &JsString) -> JsString {
    JsString::from_utf16(trim_end_units(value.units()).to_vec())
}

pub fn trim(value: &JsString) -> JsString {
    JsString::from_utf16(trim_units(value.units()).to_vec())
}

pub fn starts_with(value: &JsString, prefix: &str) -> bool {
    let prefix: Vec<u16> = prefix.encode_utf16().collect();
    value.units().len() >= prefix.len() && value.units()[..prefix.len()] == prefix[..]
}

pub fn text_block(text: JsString) -> JsValue {
    object([
        ("type", JsValue::String("text".into())),
        ("text", JsValue::String(text)),
    ])
}

pub fn object(fields: impl IntoIterator<Item = (&'static str, JsValue)>) -> JsValue {
    let mut value = JsObject::new();
    for (key, child) in fields {
        value.insert(key.into(), child);
    }
    JsValue::Object(value)
}

/// TS `protectedToolNameSet`: `undefined` selects the default set, a supplied
/// iterable replaces it verbatim (including the empty set).
#[derive(Clone, Debug)]
pub struct ProtectedNames {
    names: HashSet<String>,
}

impl ProtectedNames {
    pub fn from_options(names: Option<Vec<String>>) -> Self {
        match names {
            Some(names) => Self {
                names: names.into_iter().collect(),
            },
            None => Self {
                names: DEFAULT_PROTECTED_TOOL_RESULT_NAMES
                    .iter()
                    .map(|name| (*name).to_string())
                    .collect(),
            },
        }
    }

    pub fn empty() -> Self {
        Self {
            names: HashSet::new(),
        }
    }

    pub fn contains(&self, name: &JsString) -> bool {
        name.to_utf8().is_ok_and(|name| self.names.contains(&name))
    }
}

/// `Map<toolCallId, toolName>` gathered from assistant `tool_call` blocks.
pub fn collect_tool_names_by_call_id(messages: &[JsValue]) -> HashMap<JsString, JsString> {
    let mut names = HashMap::new();
    for message in messages {
        if !is_role(message, "assistant") {
            continue;
        }
        for block in content(message) {
            if is_block(block, "tool_call")
                && let Some(id) = string_field(block, "id")
                && let Some(name) = string_field(block, "name")
            {
                names.insert(id.clone(), name.clone());
            }
        }
    }
    names
}

pub fn replace_content(message: &JsValue, blocks: Vec<JsValue>) -> JsValue {
    let mut copy = match message {
        JsValue::Object(object) => object.clone(),
        _ => JsObject::new(),
    };
    copy.insert("content".into(), JsValue::Array(blocks));
    JsValue::Object(copy)
}

/// TS `isToolResultOnly` from `SnipEngine.ts` (media references with a call id
/// also count as tool results).
pub fn is_tool_result_only(message: &JsValue) -> bool {
    let blocks = content(message);
    if blocks.is_empty() {
        return false;
    }
    blocks.iter().all(|block| {
        is_block(block, "tool_result")
            || is_block(block, "tool_result_reference")
            || is_media_reference_with_id(block)
    })
}

/// TS `isToolResultOnly` from `protectedContext.ts` (no media references).
pub fn is_tool_result_only_strict(message: &JsValue) -> bool {
    let blocks = content(message);
    if blocks.is_empty() {
        return false;
    }
    blocks
        .iter()
        .all(|block| is_block(block, "tool_result") || is_block(block, "tool_result_reference"))
}

pub fn has_memory_context(message: &JsValue) -> bool {
    content(message).iter().any(|block| {
        is_block(block, "text")
            && string_field(block, "text")
                .map(trim_start)
                .is_some_and(|text| starts_with(&text, "<memory-context>"))
    })
}

pub fn is_protected_context_message(
    message: &JsValue,
    protected: &ProtectedNames,
    names_by_call_id: &HashMap<JsString, JsString>,
) -> bool {
    if has_memory_context(message) {
        return true;
    }
    content(message).iter().any(|block| {
        if is_block(block, "tool_call")
            && string_field(block, "name").is_some_and(|name| protected.contains(name))
        {
            return true;
        }
        if (is_block(block, "tool_result") || is_block(block, "tool_result_reference"))
            && string_field(block, "toolCallId").is_some_and(|id| {
                names_by_call_id
                    .get(id)
                    .is_some_and(|name| protected.contains(name))
            })
        {
            return true;
        }
        false
    })
}

pub fn is_protected_tool_call_id(
    id: &JsString,
    names_by_call_id: &HashMap<JsString, JsString>,
    protected: &ProtectedNames,
) -> bool {
    names_by_call_id
        .get(id)
        .is_some_and(|name| protected.contains(name))
}

/// JS Math.max/min preserve NaN and signed-zero behavior.
pub use pilotdeck_protocol::js_math::{max as js_max, min as js_min};
