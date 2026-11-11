//! `send_attachment` (baseline `tool/builtin/sendAttachment.ts`): hands an
//! existing local file to the host as a `file` content block. The tool does
//! not read the bytes; the host's event projection turns the block into the
//! channel's attachment delivery.
use super::{
    StatKind, basename, canonical, cwd, dirname, field, invalid, is_absolute, join, normalize,
    object, resolve_workspace_path, schema, stat, string, text, tool_error, within_root,
};
use crate::{
    host::{
        HostError,
        modules::capability::{RuntimeContext, ToolDefinition},
    },
    runtime::{
        permission::PermissionFuture,
        tools::{ExecutableTool, ToolError, ToolFuture, ToolOutput, ToolRunContext, js_space},
    },
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};

const DESCRIPTION: &str = "Send a local file back to the user through the current host/channel when supported. Use this when the user asks you to send, upload, return, or share an existing local file. Do not call read_file first just to send binary files; this tool sends the file bytes as an attachment without parsing them. Files under PILOTDECK_WORK_DIR or workspace .pilotdeck/work are internal candidates and cannot be sent directly; publish the reviewed final artifact outside the internal work directory first. Workspace files and registered IM attachment files can be sent directly; paths outside the workspace require explicit user permission before execution.";

pub struct SendAttachmentTool {
    fields: JsObject,
}

impl SendAttachmentTool {
    pub fn new() -> Self {
        Self {
            fields: object([
                ("name", string("send_attachment")),
                ("aliases", JsValue::Array(vec![string("send_file")])),
                ("title", string("Send attachment")),
                ("description", string(DESCRIPTION)),
                ("kind", string("filesystem")),
                (
                    "inputSchema",
                    JsValue::Object(schema(
                        r#"{"type":"object","required":["file_path"],"additionalProperties":false,"properties":{"file_path":{"type":"string","description":"Workspace-relative or absolute path to the file to send."},"name":{"type":"string","description":"Optional filename to display to the recipient."},"mime_type":{"type":"string","description":"Optional MIME type for the file."}}}"#,
                    )),
                ),
                (
                    "outputSchema",
                    JsValue::Object(schema(
                        r#"{"type":"object","required":["filePath","name","bytes"],"additionalProperties":false,"properties":{"filePath":{"type":"string"},"name":{"type":"string"},"bytes":{"type":"integer"},"mimeType":{"type":"string"}}}"#,
                    )),
                ),
            ]),
        }
    }
}

impl Default for SendAttachmentTool {
    fn default() -> Self {
        Self::new()
    }
}

fn file_path(input: &JsValue) -> JsString {
    field(input, "file_path")
        .as_string()
        .cloned()
        .unwrap_or_default()
}

/// `path.resolve(path.isAbsolute(p) ? p : path.join(cwd, p))`.
fn absolute(path: &JsString, cwd: &JsString) -> JsString {
    if is_absolute(path) {
        normalize(path)
    } else {
        join(cwd, path)
    }
}

/// `findInternalWorkRoot`: the internal work directory the path lies in, by
/// spelling first and then by real path.
fn internal_work_root(input: &JsString, context: &ToolRunContext) -> Option<JsString> {
    let cwd = cwd(context);
    let candidate = absolute(input, &cwd);
    let mut roots = vec![join(&cwd, &".pilotdeck/work".into())];
    let configured = context
        .fields
        .get("env")
        .and_then(|env| env.get("PILOTDECK_WORK_DIR"))
        .and_then(JsValue::as_string)
        .map(|value| JsString::from(text(value).trim_matches(js_space)))
        .filter(|value| !value.is_empty());
    if let Some(configured) = configured {
        roots.push(absolute(&configured, &cwd));
    }
    if let Some(root) = roots.iter().find(|root| within_root(&candidate, root)) {
        return Some(root.clone());
    }
    let real = canonical(&candidate)?;
    roots
        .iter()
        .find(|root| canonical(root).is_some_and(|root| within_root(&real, &root)))
        .cloned()
}

fn internal_message(input: &JsString, root: &JsString) -> String {
    format!(
        "Cannot send {} directly because it is inside PilotDeck's internal work directory ({}). Internal candidates may be incomplete or unreviewed. Publish the reviewed final artifact outside PILOTDECK_WORK_DIR with the relevant skill's delivery workflow (for DOCX, use docx.sh deliver), then send the delivered path.",
        text(input),
        text(root)
    )
}

/// `checkSendAttachmentPermission` / `checkReadFilePermission`: the two
/// tools share one shape, differing only in the tool name.
pub(super) fn check_read_path_permission(
    tool: &str,
    input: &JsString,
    context: &ToolRunContext,
) -> JsValue {
    let deny = |error: ToolError| {
        JsValue::Object(object([
            ("type", string("deny")),
            (
                "reason",
                JsValue::Object(object([
                    ("type", string("safety")),
                    ("message", string(error.message.clone())),
                ])),
            ),
            ("message", string(error.message)),
        ]))
    };
    match resolve_workspace_path(input, context, true, false) {
        Ok(_) => return JsValue::Object(object([("type", string("passthrough"))])),
        Err(error) if error.code != "path_not_allowed" => return deny(error),
        Err(_) => {}
    }
    let outside = match resolve_workspace_path(input, context, false, true) {
        Ok(resolved) => resolved.absolute,
        Err(error) => return deny(error),
    };
    let directory = dirname(&outside);
    let pattern = join(&directory, &"*".into());
    let rule = JsValue::Object(object([
        ("source", string("session")),
        ("behavior", string("allow")),
        ("toolName", string(tool)),
        ("pattern", string(pattern.clone())),
    ]));
    let reason = JsValue::Object(object([
        ("type", string("tool")),
        ("toolName", string(tool)),
        (
            "message",
            string(format!("{tool} targets a path outside the workspace.")),
        ),
    ]));
    let option = |id: &str, label: &str| object([("id", string(id)), ("label", string(label))]);
    let mut allow_session = option("allow_session", "Allow this folder for this session");
    allow_session.insert("rules".into(), JsValue::Array(vec![rule]));
    let summary = crate::js_value::json_stringify(&JsValue::Object(object([(
        "file_path",
        string(outside.clone()),
    )])))
    .ok()
    .flatten()
    .unwrap_or_default();
    JsValue::Object(object([
        ("type", string("ask")),
        ("reason", reason.clone()),
        (
            "request",
            JsValue::Object(object([
                ("toolCallId", string("")),
                ("toolName", string(tool)),
                ("inputSummary", string(summary)),
                ("reason", reason),
                (
                    "options",
                    JsValue::Array(vec![
                        JsValue::Object(option("allow_once", "Allow once")),
                        JsValue::Object(allow_session),
                        JsValue::Object(option("deny", "Deny")),
                        JsValue::Object(option("cancel", "Cancel")),
                    ]),
                ),
                (
                    "metadata",
                    JsValue::Object(object([
                        ("externalPath", string(outside)),
                        ("allowedDirectory", string(directory)),
                        ("pattern", string(pattern)),
                    ])),
                ),
            ])),
        ),
    ]))
}

/// `sanitizeAttachmentName`.
fn sanitize_name(name: &JsValue) -> Option<String> {
    let cleaned: String = text(name.as_string()?)
        .trim_matches(js_space)
        .chars()
        .map(|c| {
            if matches!(c, '\\' | '/' | '\0') {
                '_'
            } else {
                c
            }
        })
        .collect();
    (!cleaned.is_empty()).then_some(cleaned)
}

impl ToolDefinition for SendAttachmentTool {
    fn fields(&self) -> &JsObject {
        &self.fields
    }
    fn is_read_only(&self, _input: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(true))
    }
    fn is_concurrency_safe(&self, _input: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(false))
    }
    fn check_runtime_permissions<'a>(
        &'a self,
        input: &'a JsValue,
        runtime: &'a RuntimeContext,
    ) -> Option<PermissionFuture<'a, Option<JsValue>>> {
        let context = runtime.downcast_ref::<ToolRunContext>()?;
        let result = check_read_path_permission("send_attachment", &file_path(input), context);
        Some(Box::pin(async move { Ok(Some(result)) }))
    }
}

impl ExecutableTool for SendAttachmentTool {
    fn validate_input(
        &self,
        input: JsValue,
        context: ToolRunContext,
    ) -> ToolFuture<Option<Vec<JsObject>>> {
        Box::pin(async move {
            let path = file_path(&input);
            Ok(internal_work_root(&path, &context).map(|root| {
                vec![object([
                    ("path", string("file_path")),
                    ("code", string("invalid_schema")),
                    ("message", string(internal_message(&path, &root))),
                ])]
            }))
        })
    }

    fn execute(&self, input: JsValue, context: ToolRunContext) -> ToolFuture<ToolOutput> {
        Box::pin(async move {
            let path = file_path(&input);
            if let Some(root) = internal_work_root(&path, &context) {
                let mut error = invalid(internal_message(&path, &root));
                error.details = Some(object([
                    ("filePath", string(path)),
                    ("internalWorkRoot", string(root)),
                ]));
                return Err(error);
            }
            let allowed = context
                .fields
                .get("currentPermissionDecision")
                .and_then(|decision| decision.get("type"))
                == Some(&"allow".into());
            let resolved = resolve_workspace_path(&path, &context, true, allowed)?;
            let file = stat(&resolved.absolute)?;
            if !matches!(file.kind, StatKind::File) {
                return Err(tool_error(
                    "invalid_tool_input",
                    format!("Path {} is not a file.", text(&path)),
                ));
            }
            let name = sanitize_name(field(&input, "name"))
                .unwrap_or_else(|| text(&basename(&resolved.absolute)));
            let mime_type = field(&input, "mime_type")
                .as_string()
                .filter(|mime| !mime.is_empty())
                .cloned();
            let mut data = object([
                ("filePath", string(resolved.absolute.clone())),
                ("name", string(name.as_str())),
                ("bytes", JsValue::Number(file.size as f64)),
            ]);
            let mut block = object([
                ("type", string("file")),
                ("path", string(resolved.absolute)),
            ]);
            if let Some(mime_type) = mime_type {
                data.insert("mimeType".into(), string(mime_type.clone()));
                block.insert("mimeType".into(), string(mime_type));
            }
            block.insert(
                "description".into(),
                string(format!("Attachment requested for channel delivery: {name}")),
            );
            Ok(ToolOutput {
                content: vec![
                    JsValue::Object(object([
                        ("type", string("text")),
                        ("text", string(format!("Sending attachment: {name}"))),
                    ])),
                    JsValue::Object(block),
                ],
                data: Some(JsValue::Object(data)),
                metadata: Some(object([("attachmentDelivery", JsValue::Bool(true))])),
                supplemental_messages: None,
            })
        })
    }
}
