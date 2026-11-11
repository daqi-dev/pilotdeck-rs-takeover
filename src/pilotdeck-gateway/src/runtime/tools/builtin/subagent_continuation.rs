//! `subagent` and `send_message` (baseline `tool/builtin/subagentContinuation.ts`):
//! start a continuable background subagent, or send a follow-up to one. Both
//! return as soon as the message is durably admitted to the child's inbox;
//! the child runs on its own and its settlement reaches the parent session as
//! a follow-up turn of its own.
//!
//! The tools own only the input contract and the admission result. The child
//! sessions, their inboxes and the parent notices belong to the host and are
//! reached through the [`ContinuationPort`] the tool is built with (the
//! baseline binds the port to one exact parent agent at registration).
use super::{object, schema, string, text};
use crate::{
    host::{HostError, modules::capability::ToolDefinition},
    runtime::tools::{ExecutableTool, ToolError, ToolFuture, ToolOutput, ToolRunContext},
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::{future::Future, pin::Pin, sync::Arc};

/// `SUBAGENT_DEFINITION_IDS`.
const DEFINITION_IDS: &str = "general-purpose, explore, plan, verify";

/// `SubagentContinuationAdmission`.
#[derive(Clone, Debug, PartialEq)]
pub struct ContinuationAdmission {
    pub child_session_id: JsString,
    pub item_id: JsString,
    pub turn_id: JsString,
}

pub type AdmissionFuture =
    Pin<Box<dyn Future<Output = Result<ContinuationAdmission, JsString>> + Send>>;

/// `SubagentContinuationPort`, bound to one parent session. A rejection is
/// the message of the error the baseline throws.
pub trait ContinuationPort: Send + Sync {
    fn start(&self, label: JsString, definition_id: JsString, prompt: JsString) -> AdmissionFuture;
    fn followup(&self, child_session_id: JsString, message: JsString) -> AdmissionFuture;
}

const OUTPUT_SCHEMA: &str = r#"{"type":"object","required":["subagentId","messageId","turnId"],"additionalProperties":false,"properties":{"subagentId":{"type":"string"},"messageId":{"type":"string"},"turnId":{"type":"string"}}}"#;

fn definition(name: &str, aliases: &[&str], description: String, input: String) -> JsObject {
    let mut fields = object([("name", string(name))]);
    if !aliases.is_empty() {
        fields.insert(
            "aliases".into(),
            JsValue::Array(aliases.iter().map(|alias| string(*alias)).collect()),
        );
    }
    for (key, value) in [
        ("description", string(description)),
        ("kind", string("agent")),
        (
            "requiredRuntimeCapabilities",
            JsValue::Array(vec![string("subagent_fork")]),
        ),
        ("inputSchema", JsValue::Object(schema(&input))),
        ("outputSchema", JsValue::Object(schema(OUTPUT_SCHEMA))),
    ] {
        fields.insert(key.into(), value);
    }
    fields
}

/// `admissionOutput`.
fn admission_output(admission: ContinuationAdmission) -> ToolOutput {
    let text = format!(
        "accepted message {} for subagent {}",
        text(&admission.item_id),
        text(&admission.child_session_id)
    );
    ToolOutput {
        content: vec![JsValue::Object(object([
            ("type", string("text")),
            ("text", string(text)),
        ]))],
        supplemental_messages: None,
        data: Some(JsValue::Object(object([
            ("subagentId", string(admission.child_session_id)),
            ("messageId", string(admission.item_id)),
            ("turnId", string(admission.turn_id)),
        ]))),
        metadata: None,
    }
}

fn input_string(input: &JsValue, key: &str) -> JsString {
    input
        .get(key)
        .and_then(JsValue::as_string)
        .cloned()
        .unwrap_or_default()
}

/// `createSubagentContinuationTool(port)`.
pub struct SubagentTool {
    fields: JsObject,
    port: Arc<dyn ContinuationPort>,
}

impl SubagentTool {
    pub fn new(port: Arc<dyn ContinuationPort>) -> Self {
        Self {
            fields: definition(
                "subagent",
                &["continuable_subagent"],
                format!(
                    "Start a continuable background subagent. Available types: {DEFINITION_IDS}. The call returns after the initial message is durably admitted."
                ),
                format!(
                    r#"{{"type":"object","required":["description","prompt"],"additionalProperties":false,"properties":{{"description":{{"type":"string","description":"Short label for the delegated task."}},"prompt":{{"type":"string","description":"Initial directive for the background subagent."}},"subagent_type":{{"type":"string","description":"Subagent definition. Available: {DEFINITION_IDS}."}}}}}}"#
                ),
            ),
            port,
        }
    }
}

/// `createSendMessageTool(port)`.
pub struct SendMessageTool {
    fields: JsObject,
    port: Arc<dyn ContinuationPort>,
}

impl SendMessageTool {
    pub fn new(port: Arc<dyn ContinuationPort>) -> Self {
        Self {
            fields: definition(
                "send_message",
                &[],
                "Send a follow-up message to a continuable subagent. The call returns after durable admission.".into(),
                r#"{"type":"object","required":["subagent_id","message"],"additionalProperties":false,"properties":{"subagent_id":{"type":"string","description":"Stable continuable subagent id."},"message":{"type":"string","description":"Message to enqueue for the subagent."}}}"#.into(),
            ),
            port,
        }
    }
}

macro_rules! continuation_definition {
    ($tool:ty) => {
        impl ToolDefinition for $tool {
            fn fields(&self) -> &JsObject {
                &self.fields
            }
            fn is_read_only(&self, _input: &JsValue) -> Result<JsValue, HostError> {
                Ok(JsValue::Bool(false))
            }
            fn is_concurrency_safe(&self, _input: &JsValue) -> Result<JsValue, HostError> {
                Ok(JsValue::Bool(true))
            }
            fn required_runtime_capabilities(&self) -> Option<Vec<JsString>> {
                Some(vec!["subagent_fork".into()])
            }
        }
    };
}
continuation_definition!(SubagentTool);
continuation_definition!(SendMessageTool);

impl ExecutableTool for SubagentTool {
    fn execute(&self, input: JsValue, _context: ToolRunContext) -> ToolFuture<ToolOutput> {
        // `input.subagent_type ?? "general-purpose"`.
        let definition_id = match input.get("subagent_type") {
            None | Some(JsValue::Undefined | JsValue::Null) => "general-purpose".into(),
            Some(value) => value.as_string().cloned().unwrap_or_default(),
        };
        let admission = self.port.start(
            input_string(&input, "description"),
            definition_id,
            input_string(&input, "prompt"),
        );
        Box::pin(async move {
            admission
                .await
                .map(admission_output)
                .map_err(ToolError::failed)
        })
    }
}

impl ExecutableTool for SendMessageTool {
    fn execute(&self, input: JsValue, _context: ToolRunContext) -> ToolFuture<ToolOutput> {
        let admission = self.port.followup(
            input_string(&input, "subagent_id"),
            input_string(&input, "message"),
        );
        Box::pin(async move {
            admission
                .await
                .map(admission_output)
                .map_err(ToolError::failed)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_text_names_the_item_and_the_child() {
        let output = admission_output(ContinuationAdmission {
            child_session_id: "c".into(),
            item_id: "i".into(),
            turn_id: "t".into(),
        });
        assert_eq!(
            output.content[0].get("text"),
            Some(&"accepted message i for subagent c".into())
        );
        assert_eq!(
            output.data.as_ref().and_then(|data| data.get("turnId")),
            Some(&"t".into())
        );
    }
}
