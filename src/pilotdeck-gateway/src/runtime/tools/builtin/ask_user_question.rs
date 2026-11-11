//! `ask_user_question` (baseline `tool/builtin/askUserQuestion.ts`): hands a
//! batch of multiple-choice questions to the host's elicitation channel and
//! returns the user's answers to the model.
use super::{field, object, schema, string, text};
use crate::{
    host::{HostError, modules::capability::ToolDefinition},
    runtime::tools::{
        ElicitationRequest, ExecutableTool, ToolElicitation, ToolError, ToolFuture, ToolOutput,
        ToolRunContext,
    },
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::sync::LazyLock;

pub const ASK_USER_QUESTION_TOOL_NAME: &str = "ask_user_question";
/// `ASK_USER_QUESTION_HEADER_MAX`.
const HEADER_MAX: usize = 12;

const DESCRIPTION: &str = concat!(
    "Use this tool when you need to ask the user one or more multiple-choice questions during execution. ",
    "It is suitable for gathering preferences or requirements, clarifying ambiguous instructions, ",
    "getting decisions on implementation choices, or offering the user concrete directions to choose from. ",
    "Usage notes: provide 1-4 questions, each with 2-4 options; set multiSelect to true when a question ",
    "should allow multiple selections; phrase the choices yourself instead of using this tool for open-ended ",
    "free-form clarification. In plan mode, use ask_user_question to clarify requirements or choose between ",
    "approaches before finalizing your plan. Do not use it to ask for plan approval; use exit_plan_mode for that.",
);

const INPUT_SCHEMA: &str = r#"{
  "type": "object",
  "required": ["questions"],
  "additionalProperties": false,
  "properties": {
    "questions": {
      "type": "array",
      "minItems": 1,
      "maxItems": 4,
      "description": "Questions to ask the user. Provide 1-4 multiple-choice questions in one batch.",
      "items": {
        "type": "object",
        "required": ["question", "header", "options"],
        "additionalProperties": false,
        "properties": {
          "question": {
            "type": "string",
            "description": "The full question shown to the user. Keep it clear and specific; if multiSelect is true, phrase it as a multi-select question."
          },
          "header": {
            "type": "string",
            "maxLength": 12,
            "description": "Very short chip/tag label for the question (max 12 chars)."
          },
          "options": {
            "type": "array",
            "minItems": 2,
            "maxItems": 4,
            "description": "Available choices for this question. Provide 2-4 distinct options; they should be mutually exclusive unless multiSelect is true.",
            "items": {
              "type": "object",
              "required": ["label", "description"],
              "additionalProperties": false,
              "properties": {
                "label": {
                  "type": "string",
                  "description": "Short display text for the option. This is what the user selects."
                },
                "description": {
                  "type": "string",
                  "description": "Explanation of what the option means or what choosing it implies."
                },
                "preview": {
                  "type": "string",
                  "description": "Optional host-specific preview content associated with this option. Hosts may surface it alongside the choice and may echo the selected preview back in annotations."
                }
              }
            }
          },
          "multiSelect": {
            "type": "boolean",
            "description": "Set to true to allow the user to select multiple options for this question instead of just one."
          }
        }
      }
    },
    "answers": {
      "type": "object",
      "description": "Optional pre-supplied answers keyed by question text. Values may be a single string or an array of strings for multi-select questions."
    },
    "annotations": {
      "type": "object",
      "description": "Optional per-question annotation data keyed by question text, such as selected preview text or free-form user notes returned by the host."
    },
    "metadata": {
      "type": "object",
      "additionalProperties": false,
      "description": "Optional metadata forwarded to the host with the elicitation request. Not displayed to the user.",
      "properties": {
        "source": {
          "type": "string",
          "description": "Optional identifier describing why this question was asked, for host-side analytics or routing."
        }
      }
    }
  }
}"#;

pub struct AskUserQuestionTool {
    fields: JsObject,
}

impl AskUserQuestionTool {
    pub fn new() -> Self {
        Self {
            fields: object([
                ("name", string(ASK_USER_QUESTION_TOOL_NAME)),
                ("aliases", JsValue::Array(vec![string("AskUserQuestion")])),
                ("description", string(DESCRIPTION)),
                ("kind", string("session")),
                (
                    "requiredRuntimeCapabilities",
                    JsValue::Array(vec![string("user_interaction")]),
                ),
                ("shouldDefer", JsValue::Bool(true)),
                ("maxResultBytes", JsValue::Number(100_000.0)),
                ("inputSchema", JsValue::Object(schema(INPUT_SCHEMA))),
            ]),
        }
    }
}

impl Default for AskUserQuestionTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolDefinition for AskUserQuestionTool {
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
        Ok(JsValue::Bool(true))
    }
}

fn issue(path: &str, message: String) -> Option<Vec<JsObject>> {
    Some(vec![object([
        ("path", string(path)),
        ("code", string("invalid_schema")),
        ("message", string(message)),
    ])])
}

/// The value as a template literal would print it, for the texts quoted in
/// the validation messages.
fn shown(value: &JsValue) -> String {
    crate::js_value::to_string_units(value)
        .map(|units| String::from_utf16_lossy(&units))
        .unwrap_or_default()
}

static FULL_DOCUMENT: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)<\s*(html|body|!doctype)(?-u:\b)").expect("regex"));
static EXECUTABLE_TAG: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)<\s*(script|style)(?-u:\b)").expect("regex"));
static ANY_TAG: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"<[a-zA-Z][^>]*>").expect("regex"));

/// `validateHtmlPreview`: `None` when the preview is acceptable.
fn html_preview_error(preview: &JsValue) -> Option<&'static str> {
    let preview = match preview {
        JsValue::Undefined => return None,
        JsValue::String(value) => text(value),
        other => shown(other),
    };
    if FULL_DOCUMENT.is_match(&preview) {
        return Some(
            "preview must be an HTML fragment, not a full document (no <html>, <body>, or <!DOCTYPE>)",
        );
    }
    if EXECUTABLE_TAG.is_match(&preview) {
        return Some(
            "preview must not contain <script> or <style> tags. Use inline styles via the style attribute if needed.",
        );
    }
    if !ANY_TAG.is_match(&preview) {
        return Some(
            "preview must contain HTML (previewFormat is set to \"html\"). Wrap content in a tag like <div> or <pre>.",
        );
    }
    None
}

fn validate(input: &JsValue) -> Option<Vec<JsObject>> {
    let questions = match field(input, "questions") {
        JsValue::Array(questions) if !questions.is_empty() => questions,
        _ => return issue("questions", "Provide 1-4 questions".into()),
    };
    if questions.len() > 4 {
        return issue("questions", "At most 4 questions allowed".into());
    }
    for question in questions {
        let count = match field(question, "options") {
            JsValue::Array(options) => options.len(),
            _ => 0,
        };
        if !(2..=4).contains(&count) {
            return issue(
                "questions[].options",
                format!(
                    "Question \"{}\" must have 2-4 options",
                    shown(field(question, "question"))
                ),
            );
        }
    }
    let mut seen_questions: Vec<&JsValue> = Vec::new();
    for question in questions {
        let title = field(question, "question");
        if seen_questions.contains(&title) {
            return issue(
                "questions",
                format!("Question texts must be unique: \"{}\"", shown(title)),
            );
        }
        seen_questions.push(title);
        let header_units = field(question, "header")
            .as_string()
            .map_or(0, |header| header.units().len());
        if header_units > HEADER_MAX {
            return issue(
                "questions[].header",
                format!("header for \"{}\" exceeds {HEADER_MAX} chars", shown(title)),
            );
        }
        let JsValue::Array(options) = field(question, "options") else {
            continue;
        };
        let mut seen_labels: Vec<&JsValue> = Vec::new();
        for option in options {
            let label = field(option, "label");
            if seen_labels.contains(&label) {
                return issue(
                    "questions[].options",
                    format!(
                        "Option labels must be unique within question \"{}\"",
                        shown(title)
                    ),
                );
            }
            seen_labels.push(label);
            if let Some(error) = html_preview_error(field(option, "preview")) {
                return issue(
                    "questions[].options[].preview",
                    format!(
                        "Option \"{}\" in question \"{}\": {error}",
                        shown(label),
                        shown(title)
                    ),
                );
            }
        }
    }
    None
}

fn js_truthy(value: &JsValue) -> bool {
    super::truthy(value)
}

/// `formatAnswersForModel`: the phrasing the model reads as "the user has
/// answered".
fn format_answers(answers: &JsValue, annotations: &JsValue) -> Result<JsString, ToolError> {
    let entries: Vec<(&JsString, &JsValue)> = match answers {
        JsValue::Object(answers) => answers.own_entries(),
        JsValue::Undefined | JsValue::Null => {
            return Err(ToolError::failed(
                "Cannot convert undefined or null to object",
            ));
        }
        _ => Vec::new(),
    };
    let mut units: Vec<u16> = "User has answered your questions: "
        .encode_utf16()
        .collect();
    for (index, (question, answer)) in entries.into_iter().enumerate() {
        if index > 0 {
            units.extend(", ".encode_utf16());
        }
        units.push(b'"' as u16);
        units.extend_from_slice(question.units());
        units.extend("\"=\"".encode_utf16());
        match answer {
            JsValue::Array(items) => {
                for (position, item) in items.iter().enumerate() {
                    if position > 0 {
                        units.extend(", ".encode_utf16());
                    }
                    // `Array.prototype.join` prints null and undefined as "".
                    if !matches!(item, JsValue::Null | JsValue::Undefined) {
                        units.extend(crate::js_value::to_string_units(item).unwrap_or_default());
                    }
                }
            }
            other => units.extend(crate::js_value::to_string_units(other).unwrap_or_default()),
        }
        units.push(b'"' as u16);
        let annotation = annotations.as_object().and_then(|annotations| {
            annotations
                .entries()
                .iter()
                .find(|(key, _)| key == question)
                .map(|(_, value)| value)
        });
        for (key, label) in [
            ("preview", "selected preview:\n"),
            ("notes", "user notes: "),
        ] {
            if let Some(value) = annotation
                .and_then(|annotation| annotation.get(key))
                .filter(|value| js_truthy(value))
            {
                units.push(b' ' as u16);
                units.extend(label.encode_utf16());
                units.extend(crate::js_value::to_string_units(value).unwrap_or_default());
            }
        }
    }
    units.extend(". You can now continue with the user's answers in mind.".encode_utf16());
    Ok(JsString::from_utf16(units))
}

fn answered(
    questions: &JsValue,
    answers: &JsValue,
    annotations: &JsValue,
) -> Result<ToolOutput, ToolError> {
    let text = format_answers(answers, annotations)?;
    let mut data = object([
        ("questions", questions.clone()),
        ("answers", answers.clone()),
    ]);
    if js_truthy(annotations) {
        data.insert("annotations".into(), annotations.clone());
    }
    Ok(ToolOutput {
        content: vec![JsValue::Object(object([
            ("type", string("text")),
            ("text", JsValue::String(text)),
        ]))],
        data: Some(JsValue::Object(data)),
        metadata: None,
        supplemental_messages: None,
    })
}

impl ExecutableTool for AskUserQuestionTool {
    fn validate_input(
        &self,
        input: JsValue,
        _context: ToolRunContext,
    ) -> ToolFuture<Option<Vec<JsObject>>> {
        Box::pin(async move { Ok(validate(&input)) })
    }

    fn execute(&self, input: JsValue, context: ToolRunContext) -> ToolFuture<ToolOutput> {
        Box::pin(async move {
            let channel = context
                .host
                .elicitation
                .clone()
                .and_then(|channel| channel.downcast::<ToolElicitation>().ok())
                .ok_or_else(|| {
                    ToolError::new(
                        "unsupported_tool",
                        "ask_user_question requires a host elicitation channel (none registered).",
                    )
                })?;
            let questions = field(&input, "questions");
            // Pre-supplied answers are returned as they are.
            let supplied = field(&input, "answers");
            if supplied
                .as_object()
                .is_some_and(|answers| !answers.own_entries().is_empty())
            {
                return answered(questions, supplied, field(&input, "annotations"));
            }
            let turn_id = field(&JsValue::Object(context.fields.clone()), "turnId")
                .as_string()
                .cloned()
                .unwrap_or_default();
            let answer = channel
                .0
                .ask_user(
                    ElicitationRequest {
                        tool_call_id: turn_id,
                        tool_name: ASK_USER_QUESTION_TOOL_NAME.into(),
                        questions: questions.clone(),
                        metadata: Some(field(&input, "metadata").clone()).filter(js_truthy),
                    },
                    context.clone(),
                )
                .await?;
            if field(&answer, "type") == &JsValue::from("cancelled") {
                let reason = field(&answer, "reason");
                let mut message = String::from("User declined to answer questions");
                if js_truthy(reason) {
                    message.push_str(&format!(" ({})", shown(reason)));
                }
                return Err(ToolError::new("unsupported_tool", message));
            }
            answered(
                questions,
                field(&answer, "answers"),
                field(&answer, "annotations"),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilotdeck_protocol::parse_json;

    fn messages(input: &str) -> Vec<String> {
        validate(&parse_json(input).expect("json"))
            .unwrap_or_default()
            .iter()
            .map(|issue| shown(issue.get("message").expect("message")))
            .collect()
    }

    /// `validateInput` (askUserQuestion.ts:185-281), one issue per rule.
    #[test]
    fn validation_reports_the_first_broken_rule() {
        let option = |label: &str| format!(r#"{{"label":"{label}","description":"d"}}"#);
        let question = |text: &str, header: &str, options: &[String]| {
            format!(
                r#"{{"question":"{text}","header":"{header}","options":[{}]}}"#,
                options.join(",")
            )
        };
        let two = [option("A"), option("B")];
        assert_eq!(messages(r#"{"questions":[]}"#), ["Provide 1-4 questions"]);
        let five: Vec<_> = (0..5)
            .map(|index| question(&format!("q{index}"), "h", &two))
            .collect();
        assert_eq!(
            messages(&format!(r#"{{"questions":[{}]}}"#, five.join(","))),
            ["At most 4 questions allowed"]
        );
        assert_eq!(
            messages(&format!(
                r#"{{"questions":[{}]}}"#,
                question("Only one?", "h", &[option("A")])
            )),
            ["Question \"Only one?\" must have 2-4 options"]
        );
        assert_eq!(
            messages(&format!(
                r#"{{"questions":[{0},{0}]}}"#,
                question("Same?", "h", &two)
            )),
            ["Question texts must be unique: \"Same?\""]
        );
        assert_eq!(
            messages(&format!(
                r#"{{"questions":[{}]}}"#,
                question("Long?", "thirteen-char", &two)
            )),
            ["header for \"Long?\" exceeds 12 chars"]
        );
        assert_eq!(
            messages(&format!(
                r#"{{"questions":[{}]}}"#,
                question("Twice?", "h", &[option("A"), option("A")])
            )),
            ["Option labels must be unique within question \"Twice?\""]
        );
        assert!(
            messages(&format!(
                r#"{{"questions":[{}]}}"#,
                question("Fine?", "h", &two)
            ))
            .is_empty()
        );
    }

    /// `validateHtmlPreview`, in its three refusals.
    #[test]
    fn html_preview_must_be_a_harmless_fragment() {
        let check = |preview: &str| html_preview_error(&JsValue::from(preview));
        assert_eq!(check("<div>ok</div>"), None);
        assert_eq!(html_preview_error(&JsValue::Undefined), None);
        assert!(check("< HTML><p>x</p>").unwrap().contains("full document"));
        assert!(
            check("<p>x</p><script>1</script>")
                .unwrap()
                .contains("<script> or <style>")
        );
        assert!(check("plain text").unwrap().contains("must contain HTML"));
    }

    /// `formatAnswersForModel`: multi-select answers are joined, annotations
    /// follow their answer.
    #[test]
    fn answers_are_phrased_for_the_model() {
        let answers = parse_json(r#"{"Which?":"Blue","Many?":["a","b"]}"#).unwrap();
        let annotations = parse_json(r#"{"Which?":{"preview":"<b>p</b>","notes":"n"}}"#).unwrap();
        assert_eq!(
            text(&format_answers(&answers, &annotations).unwrap()),
            "User has answered your questions: \"Which?\"=\"Blue\" selected preview:\n<b>p</b> user notes: n, \"Many?\"=\"a, b\". You can now continue with the user's answers in mind."
        );
    }
}
