//! `structured_output` (baseline `tool/builtin/structuredOutput.ts`): echoes
//! the model-supplied `value` back as JSON content and as the result data.
use super::{field, object, schema, string};
use crate::{
    host::{HostError, modules::capability::ToolDefinition},
    runtime::tools::{ExecutableTool, ToolFuture, ToolOutput, ToolRunContext},
};
use pilotdeck_protocol::{JsObject, JsValue};

pub struct StructuredOutputTool {
    fields: JsObject,
}

impl StructuredOutputTool {
    pub fn new() -> Self {
        Self {
            fields: object([
                ("name", string("structured_output")),
                ("aliases", JsValue::Array(vec![string("StructuredOutput")])),
                (
                    "description",
                    string("Return a final structured output payload for non-interactive hosts."),
                ),
                ("kind", string("structured_output")),
                (
                    "inputSchema",
                    JsValue::Object(schema(
                        r#"{"type":"object","required":["value"],"additionalProperties":false,"properties":{"value":{"type":["object","array","string","number","boolean"],"description":"The structured payload to return (any JSON-serializable value)."}}}"#,
                    )),
                ),
            ]),
        }
    }
}

impl Default for StructuredOutputTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolDefinition for StructuredOutputTool {
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

impl ExecutableTool for StructuredOutputTool {
    fn execute(&self, input: JsValue, _context: ToolRunContext) -> ToolFuture<ToolOutput> {
        Box::pin(async move {
            let value = field(&input, "value").clone();
            Ok(ToolOutput {
                content: vec![JsValue::Object(object([
                    ("type", string("json")),
                    ("value", value.clone()),
                ]))],
                data: Some(value),
                metadata: Some(object([("structuredOutput", JsValue::Bool(true))])),
                supplemental_messages: None,
            })
        })
    }
}
