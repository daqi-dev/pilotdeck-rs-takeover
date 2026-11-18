//! Stream retry checkpoint and LiteLLM continuation request construction.
use crate::codec::{
    CodecError,
    limits::append_js_string,
    projection::{field, is, put},
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};

#[derive(Default)]
pub struct Checkpoint {
    partial_text: Vec<u16>,
    has_tool_calls: bool,
    has_reasoning: bool,
    active: Vec<(JsString, JsValue, f64)>,
}
impl Checkpoint {
    /// Accumulated text as original JavaScript UTF-16 code units.
    pub fn partial_text(&self) -> &[u16] {
        &self.partial_text
    }

    /// `StreamingCheckpointManager.onEvent`; `partialText += text` throws
    /// past V8's string limit.
    pub fn on_event(&mut self, event: &JsValue) -> Result<(), CodecError> {
        let tag = field(event, "type");
        let string = |key: &str| {
            JsString::from_utf16(
                crate::codec::js::to_string_units(field(event, key)).unwrap_or_default(),
            )
        };
        if is(tag, "text_delta") {
            append_js_string(&mut self.partial_text, string("text").units())?;
        } else if is(tag, "thinking_delta") {
            self.has_reasoning = true;
        } else if is(tag, "tool_call_start") {
            self.has_tool_calls = true;
            self.set_active(string("id"), field(event, "name").clone(), 0.0);
        } else if is(tag, "tool_call_delta") {
            self.has_tool_calls = true;
            let id = string("id");
            let added = string("delta").len_utf16() as f64;
            match self.active.iter_mut().find(|(k, _, _)| *k == id) {
                Some(entry) => entry.2 += added,
                None => self.active.push((id, "".into(), added)),
            }
        } else if is(tag, "tool_call_end") {
            self.has_tool_calls = true;
            let id = JsString::from_utf16(
                crate::codec::js::to_string_units(field(field(event, "toolCall"), "id"))
                    .unwrap_or_default(),
            );
            self.active.retain(|(k, _, _)| *k != id);
        }
        Ok(())
    }
    fn set_active(&mut self, id: JsString, name: JsValue, chars: f64) {
        match self.active.iter_mut().find(|(k, _, _)| *k == id) {
            Some(entry) => {
                entry.1 = name;
                entry.2 = chars;
            }
            None => self.active.push((id, name, chars)),
        }
    }
    pub fn has_text(&self) -> bool {
        let text = String::from_utf16_lossy(&self.partial_text);
        !crate::codec::metadata::js_trim(&text).is_empty()
    }
    pub fn can_continue_text(&self) -> bool {
        self.has_text() && !self.has_reasoning && !self.has_tool_calls
    }
    pub fn is_empty(&self) -> bool {
        !self.has_tool_calls && !self.has_text() && !self.has_reasoning
    }
    pub fn interruption(&self) -> JsValue {
        let mut out = JsObject::new();
        if self.has_tool_calls {
            out.insert("phase".into(), "tool_call".into());
            let calls = self
                .active
                .iter()
                .map(|(id, name, chars)| {
                    let mut call = JsObject::new();
                    call.insert("id".into(), JsValue::String(id.clone()));
                    call.insert("name".into(), name.clone());
                    call.insert("argumentChars".into(), JsValue::Number(*chars));
                    JsValue::Object(call)
                })
                .collect();
            out.insert("activeToolCalls".into(), JsValue::Array(calls));
        } else if self.has_text() {
            out.insert("phase".into(), "text".into());
        } else if self.has_reasoning {
            out.insert("phase".into(), "reasoning".into());
        } else {
            out.insert("phase".into(), "empty".into());
        }
        JsValue::Object(out)
    }
}

const CONTINUATION_INSTRUCTION: &str = "You are a helpful assistant. You are given a message and you need to respond to it. You are also given a generated content. You need to respond to the message in continuation of the generated content. Do not repeat the same content. Your response should be in continuation of this text:";

fn text_message(role: &str, text: JsValue) -> JsValue {
    let mut block = JsObject::new();
    block.insert("type".into(), "text".into());
    block.insert("text".into(), text);
    let mut message = JsObject::new();
    message.insert("role".into(), role.into());
    message.insert(
        "content".into(),
        JsValue::Array(vec![JsValue::Object(block)]),
    );
    JsValue::Object(message)
}

/// `buildLiteLLMContinuationRequest(original, partialText)`.
pub fn continuation_request(original: &JsValue, partial: &[u16]) -> JsValue {
    let mut messages = match field(original, "messages") {
        JsValue::Array(messages) => messages.clone(),
        _ => Vec::new(),
    };
    // stripLiteLLMContinuationMessages
    if messages.len() >= 2 {
        let last = &messages[messages.len() - 1];
        let second = &messages[messages.len() - 2];
        let content = match field(last, "content") {
            JsValue::Array(content) => content.as_slice(),
            _ => &[],
        };
        if is(field(last, "role"), "user")
            && is(field(second, "role"), "assistant")
            && content.len() == 1
            && is(field(&content[0], "type"), "text")
            && is(field(&content[0], "text"), CONTINUATION_INSTRUCTION)
        {
            messages.truncate(messages.len() - 2);
        }
    }
    messages.push(text_message(
        "assistant",
        JsValue::String(JsString::from_utf16(partial.to_vec())),
    ));
    messages.push(text_message("user", CONTINUATION_INSTRUCTION.into()));
    let mut out = original.clone();
    put(&mut out, "messages", JsValue::Array(messages));
    put(&mut out, "cacheBreakpoints", JsValue::Undefined);
    put(&mut out, "cachePlan", JsValue::Undefined);
    out
}
