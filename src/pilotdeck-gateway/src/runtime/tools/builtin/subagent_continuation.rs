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
    /// Context-aware admission. Existing ports keep their original followup
    /// implementation; native ports can retain the exact caller abort signal.
    fn followup_with_context(
        &self,
        child_session_id: JsString,
        message: JsString,
        _context: ToolRunContext,
    ) -> AdmissionFuture {
        self.followup(child_session_id, message)
    }
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
        #[cfg(test)]
        admission_tests::capture_context(&_context);
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
        #[cfg(test)]
        admission_tests::capture_context(&_context);
        let admission = self.port.followup_with_context(
            input_string(&input, "subagent_id"),
            input_string(&input, "message"),
            _context,
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
    fn call_context(signal: crate::host::Cancellation) -> ToolRunContext {
        use crate::host::modules::tool_context::{ToolFileState, ToolRuntimeContext};
        ToolRunContext::new(Arc::new(ToolRuntimeContext {
            fields: JsObject::new(),
            files: ToolFileState {
                read_file_state: Arc::default(),
                write_snapshots: Arc::default(),
                allowed_read_files: vec![],
            },
            abort_signal: Some(Arc::new(signal)),
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
    struct LegacyPort;
    impl ContinuationPort for LegacyPort {
        fn start(&self, _: JsString, _: JsString, _: JsString) -> AdmissionFuture {
            panic!("send_message must not start a new child");
        }
        fn followup(&self, child: JsString, message: JsString) -> AdmissionFuture {
            assert_eq!(child, "legacy-child".into());
            assert_eq!(message, "legacy-message".into());
            Box::pin(async {
                Ok(ContinuationAdmission {
                    child_session_id: "legacy-child".into(),
                    item_id: "item".into(),
                    turn_id: "turn".into(),
                })
            })
        }
    }
    #[tokio::test]
    async fn send_message_legacy_port_keeps_contextless_implementation_compatible() {
        let tool = SendMessageTool::new(Arc::new(LegacyPort));
        let output = tool
            .execute(
                JsValue::Object(schema(
                    r#"{"subagent_id":"legacy-child","message":"legacy-message"}"#,
                )),
                call_context(crate::host::Cancellation::default()),
            )
            .await
            .unwrap();
        assert_eq!(
            output.data.as_ref().and_then(|d| d.get("messageId")),
            Some(&"item".into())
        );
        assert_eq!(
            output.content[0].get("text"),
            Some(&"accepted message item for subagent legacy-child".into())
        );
    }
    struct ContextPort {
        expected: crate::host::Cancellation,
    }
    impl ContinuationPort for ContextPort {
        fn start(&self, _: JsString, _: JsString, _: JsString) -> AdmissionFuture {
            panic!("unexpected start")
        }
        fn followup(&self, _: JsString, _: JsString) -> AdmissionFuture {
            panic!("context-aware tool must carry caller signal")
        }
        fn followup_with_context(
            &self,
            child: JsString,
            message: JsString,
            context: ToolRunContext,
        ) -> AdmissionFuture {
            assert_eq!(child, "live-child".into());
            assert_eq!(message, "pending".into());
            let signal = context
                .host
                .abort_signal
                .as_ref()
                .unwrap()
                .downcast_ref::<crate::host::Cancellation>()
                .unwrap()
                .clone();
            assert!(!signal.is_cancelled());
            assert!(!self.expected.is_cancelled());
            Box::pin(async move {
                signal.cancelled().await;
                assert_eq!(signal.reason(), Some("fixture_cancel".into()));
                Err("continuable subagent follow-up aborted: fixture_cancel".into())
            })
        }
    }
    #[tokio::test]
    async fn send_message_context_port_observes_live_caller_abort_and_preserves_tool_error() {
        let signal = crate::host::Cancellation::default();
        let tool = SendMessageTool::new(Arc::new(ContextPort {
            expected: signal.clone(),
        }));
        let pending = tool.execute(
            JsValue::Object(schema(
                r#"{"subagent_id":"live-child","message":"pending"}"#,
            )),
            call_context(signal.clone()),
        );
        signal.cancel(Some("fixture_cancel".into()));
        let error = tokio::time::timeout(std::time::Duration::from_secs(1), pending)
            .await
            .expect("the exact caller signal reaches the port")
            .unwrap_err();
        assert_eq!(error.code, "tool_execution_failed");
        assert_eq!(
            error.message,
            "continuable subagent follow-up aborted: fixture_cancel".into()
        );
    }
}

#[cfg(test)]
#[path = "continuation_admission_tests.rs"]
pub(crate) mod admission_tests;
