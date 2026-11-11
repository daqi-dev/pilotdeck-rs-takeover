//! The session-scoped user-dialog tools (baseline `tool/builtin/
//! requestUserInput.ts`, `requestUserChoice.ts`, `requestUserConfirmation.ts`,
//! `requestUserForm.ts`). The gateway registers one per dialog kind an SDK
//! session enabled (`sdkSessionConfig.userDialogKinds`); each hands its request
//! to the session's gateway user-dialog channel and waits for the answer that
//! `user_dialog_respond` delivers.
use super::{field, object, schema, string, text, truthy};
use crate::{
    host::{HostError, modules::capability::ToolDefinition},
    runtime::tools::{ExecutableTool, ToolError, ToolFuture, ToolOutput, ToolRunContext},
    user_dialog::{
        AbortSignal, ChannelRequest, ConfirmRequest, DialogAnswer, FormRequest,
        GatewayUserDialogChannel, InputRequest, SelectChoice, SelectRequest,
    },
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::{sync::Arc, time::Duration};

pub const REQUEST_USER_INPUT_TOOL_NAME: &str = "request_user_input";
pub const REQUEST_USER_CHOICE_TOOL_NAME: &str = "request_user_choice";
pub const REQUEST_USER_CONFIRMATION_TOOL_NAME: &str = "request_user_confirmation";
pub const REQUEST_USER_FORM_TOOL_NAME: &str = "request_user_form";

/// How often a waiting tool looks at its channel request. The channel settles
/// a request synchronously (an answer, a cancellation or a rejection); this
/// only bounds how late the tool notices.
const POLL: Duration = Duration::from_millis(10);

const INPUT_SCHEMA: &str = r#"{"type":"object","required":["prompt"],"additionalProperties":false,"properties":{"prompt":{"type":"string","minLength":1,"maxLength":4000,"description":"The concise question or request shown to the user."},"placeholder":{"type":"string","maxLength":512,"description":"Optional input placeholder shown by the host UI."},"allowEmpty":{"type":"boolean","description":"Whether the host may submit an empty answer."}}}"#;
const CHOICE_SCHEMA: &str = r#"{"type":"object","required":["prompt","options"],"additionalProperties":false,"properties":{"prompt":{"type":"string","minLength":1,"maxLength":4000,"description":"The concise decision prompt shown to the user."},"options":{"type":"array","minItems":2,"maxItems":12,"description":"Two to twelve mutually exclusive choices for the user.","items":{"type":"object","required":["value"],"additionalProperties":false,"properties":{"value":{"type":"string","minLength":1,"maxLength":512,"description":"Stable value returned to the model."},"label":{"type":"string","maxLength":512,"description":"Optional display label for the host UI."},"description":{"type":"string","maxLength":2000,"description":"Optional explanation of this choice."}}}},"defaultValue":{"type":"string","maxLength":512,"description":"Optional value selected by default. It must occur in options."}}}"#;
const CONFIRMATION_SCHEMA: &str = r#"{"type":"object","required":["prompt"],"additionalProperties":false,"properties":{"prompt":{"type":"string","minLength":1,"maxLength":4000,"description":"The decision shown to the user."},"confirmLabel":{"type":"string","maxLength":128,"description":"Optional affirmative button label."},"cancelLabel":{"type":"string","maxLength":128,"description":"Optional negative button label."},"defaultValue":{"type":"boolean","description":"Optional default boolean selection."}}}"#;
const FORM_SCHEMA: &str = r#"{"type":"object","required":["prompt","schema"],"additionalProperties":false,"properties":{"prompt":{"type":"string","minLength":1,"maxLength":4000,"description":"The concise instruction shown above the form."},"schema":{"type":"object","additionalProperties":true,"description":"A supported JSON object schema that describes the form response."}}}"#;

#[derive(Clone, Copy)]
enum Kind {
    Input,
    Choice,
    Confirmation,
    Form,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Input => REQUEST_USER_INPUT_TOOL_NAME,
            Self::Choice => REQUEST_USER_CHOICE_TOOL_NAME,
            Self::Confirmation => REQUEST_USER_CONFIRMATION_TOOL_NAME,
            Self::Form => REQUEST_USER_FORM_TOOL_NAME,
        }
    }
    /// The phrase of the cancellation message.
    fn noun(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Choice => "choice",
            Self::Confirmation => "confirmation",
            Self::Form => "form",
        }
    }
    fn fields(self) -> JsObject {
        let (alias, description, max_result_bytes, input_schema) = match self {
            Self::Input => (
                "RequestUserInput",
                "Ask the human user for one short free-form answer during this task. Use this only when a concise clarification is required and multiple-choice ask_user_question is unsuitable.",
                16_000.0,
                INPUT_SCHEMA,
            ),
            Self::Choice => (
                "RequestUserChoice",
                "Ask the human user to choose exactly one option during this task. Use this when a bounded decision is needed and the options can be stated clearly.",
                16_000.0,
                CHOICE_SCHEMA,
            ),
            Self::Confirmation => (
                "RequestUserConfirmation",
                "Ask the human user to confirm or decline one explicit decision during this task. This does not grant permission for a tool; it only returns a boolean answer.",
                16_000.0,
                CONFIRMATION_SCHEMA,
            ),
            Self::Form => (
                "RequestUserForm",
                "Ask the human user to fill a structured form during this task. Use a JSON object schema with fields, required entries, nested objects or arrays, and scalar enums.",
                32_000.0,
                FORM_SCHEMA,
            ),
        };
        object([
            ("name", string(self.name())),
            ("aliases", JsValue::Array(vec![string(alias)])),
            ("description", string(description)),
            ("kind", string("session")),
            ("shouldDefer", JsValue::Bool(true)),
            ("maxResultBytes", JsValue::Number(max_result_bytes)),
            ("inputSchema", JsValue::Object(schema(input_schema))),
        ])
    }
}

/// One `request_user_*` tool bound to its session's dialog channel.
pub struct RequestUserTool {
    kind: Kind,
    fields: JsObject,
    channel: GatewayUserDialogChannel,
}

impl RequestUserTool {
    fn new(kind: Kind, channel: GatewayUserDialogChannel) -> Self {
        Self {
            kind,
            fields: kind.fields(),
            channel,
        }
    }
}

/// The tools for the enabled dialog kinds, in the baseline's registration
/// order (input, select, confirm, form).
pub fn user_dialog_tools(
    kinds: &[&str],
    channel: &GatewayUserDialogChannel,
) -> Vec<Arc<dyn ExecutableTool>> {
    [
        ("input", Kind::Input),
        ("select", Kind::Choice),
        ("confirm", Kind::Confirmation),
        ("form", Kind::Form),
    ]
    .into_iter()
    .filter(|(name, _)| kinds.contains(name))
    .map(|(_, kind)| {
        Arc::new(RequestUserTool::new(kind, channel.clone())) as Arc<dyn ExecutableTool>
    })
    .collect()
}

impl ToolDefinition for RequestUserTool {
    fn fields(&self) -> &JsObject {
        &self.fields
    }
    fn is_read_only(&self, _input: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(true))
    }
    fn is_concurrency_safe(&self, _input: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(true))
    }
    fn requires_user_interaction(&self, _input: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(false))
    }
}

fn issue(path: &str, message: impl Into<JsString>) -> Option<Vec<JsObject>> {
    Some(vec![object([
        ("path", string(path)),
        ("code", string("invalid_schema")),
        ("message", JsValue::String(message.into())),
    ])])
}

fn trimmed_empty(value: &JsString) -> bool {
    crate::metadata::js_trim(&text(value)).is_empty()
}

/// `!input.prompt.trim()`.
fn prompt_issue(input: &JsValue) -> Result<Option<Vec<JsObject>>, ToolError> {
    match field(input, "prompt") {
        JsValue::String(prompt) if trimmed_empty(prompt) => {
            Ok(issue("prompt", "prompt must be a non-empty string."))
        }
        JsValue::String(_) => Ok(None),
        _ => Err(ToolError::failed("input.prompt.trim is not a function")),
    }
}

/// `validateRequestUserConfirmationInput`.
fn validate_confirmation(input: &JsValue) -> Result<Option<Vec<JsObject>>, ToolError> {
    if let Some(issues) = prompt_issue(input)? {
        return Ok(Some(issues));
    }
    for key in ["confirmLabel", "cancelLabel"] {
        let ok = match field(input, key) {
            JsValue::Undefined => true,
            JsValue::String(value) => !trimmed_empty(value) && value.units().len() <= 128,
            _ => false,
        };
        if !ok {
            return Ok(issue(
                key,
                format!("{key} must be a non-empty string no longer than 128 characters."),
            ));
        }
    }
    if !matches!(
        field(input, "defaultValue"),
        JsValue::Undefined | JsValue::Bool(_)
    ) {
        return Ok(issue("defaultValue", "defaultValue must be a boolean."));
    }
    Ok(None)
}

/// `validateRequestUserChoiceInput`.
fn validate_choice(input: &JsValue) -> Result<Option<Vec<JsObject>>, ToolError> {
    if let Some(issues) = prompt_issue(input)? {
        return Ok(Some(issues));
    }
    let options = match field(input, "options") {
        JsValue::Array(options) if (2..=12).contains(&options.len()) => options,
        _ => return Ok(issue("options", "options must contain 2-12 choices.")),
    };
    let mut values: Vec<&JsString> = Vec::new();
    for (index, option) in options.iter().enumerate() {
        let value = match field(option, "value") {
            JsValue::String(value) if !trimmed_empty(value) => value,
            _ => {
                return Ok(issue(
                    &format!("options[{index}].value"),
                    "each option value must be a non-empty string.",
                ));
            }
        };
        let too_long = |key: &str, limit: usize| match field(option, key) {
            JsValue::Undefined => false,
            JsValue::String(value) => value.units().len() > limit,
            _ => true,
        };
        if value.units().len() > 512 || too_long("label", 512) || too_long("description", 2_000) {
            return Ok(issue(
                &format!("options[{index}]"),
                "choice values, labels, and descriptions exceed their limits.",
            ));
        }
        if values.contains(&value) {
            return Ok(issue(
                &format!("options[{index}].value"),
                "choice values must be unique.",
            ));
        }
        values.push(value);
    }
    match field(input, "defaultValue") {
        JsValue::Undefined => {}
        JsValue::String(default) if values.contains(&default) && default.units().len() <= 512 => {}
        _ => {
            return Ok(issue(
                "defaultValue",
                "defaultValue must match one of the choice values.",
            ));
        }
    }
    Ok(None)
}

fn optional_string(input: &JsValue, key: &str) -> Option<JsString> {
    field(input, key).as_string().cloned()
}

/// Waits for the channel to settle the request. The call's abort signal is
/// forwarded to the channel once, which cancels a registered dialog.
async fn wait(
    mut request: ChannelRequest,
    abort: AbortSignal,
    context: &ToolRunContext,
) -> Result<DialogAnswer, String> {
    let mut forwarded = false;
    loop {
        if let Some(result) = request.try_result() {
            return result;
        }
        if forwarded {
            tokio::time::sleep(POLL).await;
            continue;
        }
        tokio::select! {
            _ = tokio::time::sleep(POLL) => {}
            _ = context.cancelled() => {
                forwarded = true;
                abort.abort();
            }
        }
    }
}

fn text_output(message: JsString, data: JsObject) -> ToolOutput {
    ToolOutput {
        content: vec![JsValue::Object(object([
            ("type", string("text")),
            ("text", JsValue::String(message)),
        ]))],
        supplemental_messages: None,
        data: Some(JsValue::Object(data)),
        metadata: None,
    }
}

fn concat(prefix: &str, value: &JsString) -> JsString {
    let mut units: Vec<u16> = prefix.encode_utf16().collect();
    units.extend_from_slice(value.units());
    JsString::from_utf16(units)
}

impl ExecutableTool for RequestUserTool {
    fn validate_input(
        &self,
        input: JsValue,
        _context: ToolRunContext,
    ) -> ToolFuture<Option<Vec<JsObject>>> {
        let kind = self.kind;
        Box::pin(async move {
            match kind {
                Kind::Input => Ok(None),
                Kind::Choice => validate_choice(&input),
                Kind::Confirmation => validate_confirmation(&input),
                Kind::Form => super::form_dialog::validate_input(&input),
            }
        })
    }

    fn execute(&self, input: JsValue, context: ToolRunContext) -> ToolFuture<ToolOutput> {
        let kind = self.kind;
        let channel = self.channel.clone();
        Box::pin(async move {
            // `context.currentToolCallId ?? context.turnId`.
            let fields = JsValue::Object(context.fields.clone());
            let tool_call_id = field(&fields, "currentToolCallId")
                .as_string()
                .or_else(|| field(&fields, "turnId").as_string())
                .cloned()
                .unwrap_or_default();
            let tool_name: JsString = kind.name().into();
            let prompt = optional_string(&input, "prompt").unwrap_or_default();
            let abort = AbortSignal::new();
            if context.is_aborted() {
                abort.abort();
            }
            let request = match kind {
                Kind::Input => channel.request_input(&InputRequest {
                    tool_call_id,
                    tool_name,
                    prompt,
                    // `...(input.placeholder ? { placeholder } : {})`.
                    placeholder: optional_string(&input, "placeholder")
                        .filter(|value| !value.units().is_empty()),
                    allow_empty: field(&input, "allowEmpty") == &JsValue::Bool(true),
                    abort: Some(abort.clone()),
                }),
                Kind::Choice => channel.request_select(&SelectRequest {
                    tool_call_id,
                    tool_name,
                    prompt,
                    choices: match field(&input, "options") {
                        JsValue::Array(options) => options
                            .iter()
                            .map(|option| SelectChoice {
                                value: optional_string(option, "value").unwrap_or_default(),
                                label: optional_string(option, "label"),
                                description: optional_string(option, "description"),
                            })
                            .collect(),
                        _ => Vec::new(),
                    },
                    default_value: optional_string(&input, "defaultValue"),
                    abort: Some(abort.clone()),
                }),
                Kind::Confirmation => channel.request_confirm(&ConfirmRequest {
                    tool_call_id,
                    tool_name,
                    prompt,
                    confirm_label: optional_string(&input, "confirmLabel"),
                    cancel_label: optional_string(&input, "cancelLabel"),
                    default_value: field(&input, "defaultValue").as_bool(),
                    abort: Some(abort.clone()),
                }),
                Kind::Form => channel.request_form(&FormRequest {
                    tool_call_id,
                    tool_name,
                    prompt,
                    schema: field(&input, "schema").clone(),
                    abort: Some(abort.clone()),
                }),
            };
            let answer = wait(request, abort, &context)
                .await
                .map_err(ToolError::failed)?;
            let value = match answer {
                DialogAnswer::Cancelled(reason) => {
                    let mut message = format!("The user cancelled the {} request", kind.noun());
                    match reason.filter(|reason| truthy(&JsValue::String(reason.clone()))) {
                        Some(reason) => {
                            message.push_str(": ");
                            message.push_str(&text(&reason));
                        }
                        None => message.push('.'),
                    }
                    return Err(ToolError::new("unsupported_tool", message));
                }
                DialogAnswer::Answered(value) => value,
            };
            match kind {
                Kind::Input => {
                    let answer = value.as_string().cloned().unwrap_or_default();
                    if field(&input, "allowEmpty") != &JsValue::Bool(true) && trimmed_empty(&answer)
                    {
                        return Err(ToolError::failed(
                            "The user submitted an empty response for a required input request.",
                        ));
                    }
                    Ok(text_output(
                        concat("The user answered: ", &answer),
                        object([("answer", JsValue::String(answer.clone()))]),
                    ))
                }
                Kind::Choice => {
                    let selected = value.as_string().cloned().unwrap_or_default();
                    Ok(text_output(
                        concat("The user selected: ", &selected),
                        object([("selected", JsValue::String(selected.clone()))]),
                    ))
                }
                Kind::Confirmation => {
                    let confirmed = value.as_bool().unwrap_or(false);
                    Ok(text_output(
                        format!(
                            "The user confirmed: {}",
                            if confirmed { "yes" } else { "no" }
                        )
                        .into(),
                        object([("confirmed", JsValue::Bool(confirmed))]),
                    ))
                }
                Kind::Form => Ok(ToolOutput {
                    content: vec![JsValue::Object(object([
                        ("type", string("json")),
                        ("value", value.clone()),
                    ]))],
                    supplemental_messages: None,
                    data: Some(JsValue::Object(object([("values", value)]))),
                    metadata: None,
                }),
            }
        })
    }
}
