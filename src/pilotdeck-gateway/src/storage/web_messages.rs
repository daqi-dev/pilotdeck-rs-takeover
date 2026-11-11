//! Web history reader: transcript entries → the `WebMessage[]` page that
//! `read_session_messages` / `read_subagent_messages` answer with.
//!
//! Baseline: resources/PilotDeck/src/web/server/readSessionMessages.ts. The
//! functions here are pure; the caller resolves paths, reads the transcript
//! and looks the session up in the catalog. Line references below are to that
//! file unless another one is named.
use pilotdeck_protocol::{JsObject, JsString, JsValue};

use super::session_projections::{
    WebHistory, field, is, object, positive_number, string_units, subagent_entries, truthy,
};
use super::slice_index;

const DEFAULT_HISTORY_CONTEXT_TOKENS: f64 = 200_000.0;

/// What the gateway composition hands the reader (:52-65).
pub(crate) struct WebReadOptions {
    pub(crate) max_context_tokens: Option<f64>,
    pub(crate) max_output_tokens: Option<f64>,
    /// `options.now().toISOString()`, used where an entry has no timestamp.
    pub(crate) now_iso: String,
    /// `resolve(effectiveProjectRoot) !== resolve(pilotHome)` (:132).
    pub(crate) show_file_artifacts: bool,
}

fn nullish(value: &JsValue) -> bool {
    matches!(value, JsValue::Undefined | JsValue::Null)
}
fn or<'a>(first: &'a JsValue, second: &'a JsValue) -> &'a JsValue {
    if nullish(first) { second } else { first }
}
fn units(value: &JsValue) -> Vec<u16> {
    crate::js_value::to_string_units(value).unwrap_or_default()
}
enum Part<'a> {
    Text(&'a str),
    Value(&'a JsValue),
    Number(usize),
}
/// A template literal: every interpolated value goes through `String(...)`.
fn template(parts: &[Part<'_>]) -> JsValue {
    let mut out = Vec::new();
    for part in parts {
        match part {
            Part::Text(text) => out.extend(text.encode_utf16()),
            Part::Value(value) => out.extend(units(value)),
            Part::Number(value) => out.extend(value.to_string().encode_utf16()),
        }
    }
    string_units(out)
}
fn set(message: &mut JsObject, key: &str, value: JsValue) {
    if !value.is_undefined() {
        message.insert(key.into(), value);
    }
}
fn when(condition: bool, value: impl FnOnce() -> JsValue) -> JsValue {
    if condition {
        value()
    } else {
        JsValue::Undefined
    }
}
fn array_of(value: &JsValue) -> &[JsValue] {
    match value {
        JsValue::Array(items) => items,
        _ => &[],
    }
}
fn message_field<'a>(message: &'a JsObject, key: &str) -> &'a JsValue {
    static UNDEFINED: JsValue = JsValue::Undefined;
    message.get(key).unwrap_or(&UNDEFINED)
}

/// The fields every history message starts with.
struct Scope<'a> {
    session_key: &'a JsValue,
    project_key: &'a JsValue,
}
impl Scope<'_> {
    fn message(&self, id: JsValue, created_at: &JsValue, role: &str, kind: &JsValue) -> JsObject {
        let mut message = JsObject::new();
        set(&mut message, "id", id);
        set(&mut message, "sessionKey", self.session_key.clone());
        set(&mut message, "projectKey", self.project_key.clone());
        set(&mut message, "createdAt", created_at.clone());
        set(&mut message, "provider", "pilotdeck".into());
        set(&mut message, "role", role.into());
        set(&mut message, "kind", kind.clone());
        message
    }
}

/// `toWebMessageImage` (:780-785).
fn web_image(block: &JsValue) -> JsValue {
    let mime = field(block, "mimeType");
    let data = field(block, "data");
    object([
        (
            "data",
            if is(field(block, "source"), "url") {
                data.clone()
            } else {
                template(&[
                    Part::Text("data:"),
                    Part::Value(mime),
                    Part::Text(";base64,"),
                    Part::Value(data),
                ])
            },
        ),
        ("mimeType", mime.clone()),
    ])
}

/// `flattenToolResultBlockText` (model/protocol/toolResultContent.ts:6-27).
/// `Array.prototype.join` renders a nullish element as the empty string.
fn tool_result_text(block: &JsValue) -> JsValue {
    let mut out: Vec<u16> = Vec::new();
    for (index, item) in array_of(field(block, "content")).iter().enumerate() {
        if index > 0 {
            out.push(u16::from(b'\n'));
        }
        let kind = field(item, "type");
        let data_length = || {
            field(item, "data")
                .as_string()
                .map_or(0, JsString::len_utf16)
        };
        let part = if is(kind, "text") {
            let text = field(item, "text");
            if nullish(text) {
                continue;
            }
            text.clone()
        } else if is(kind, "image") {
            if is(field(item, "source"), "url") {
                template(&[
                    Part::Text("[Image: "),
                    Part::Value(field(item, "mimeType")),
                    Part::Text(", "),
                    Part::Value(field(item, "data")),
                    Part::Text("]"),
                ])
            } else {
                template(&[
                    Part::Text("[Image: "),
                    Part::Value(field(item, "mimeType")),
                    Part::Text(", "),
                    Part::Number(data_length()),
                    Part::Text(" base64 characters]"),
                ])
            }
        } else if is(kind, "pdf") {
            let pages = field(item, "pages");
            let mut parts = vec![
                Part::Text("[PDF: "),
                Part::Value(field(item, "mimeType")),
                Part::Text(", "),
                Part::Number(data_length()),
                Part::Text(" base64 characters"),
            ];
            if truthy(pages) {
                parts.extend([Part::Text(", "), Part::Value(pages), Part::Text(" pages")]);
            }
            parts.push(Part::Text("]"));
            template(&parts)
        } else {
            continue;
        };
        out.extend(units(&part));
    }
    string_units(out)
}

fn non_empty_string(value: &JsValue) -> JsValue {
    when(value.as_string().is_some_and(|s| !s.is_empty()), || {
        value.clone()
    })
}
/// `typeof value === "object" && value` for JSON data.
fn object_like(value: &JsValue) -> bool {
    matches!(value, JsValue::Object(_) | JsValue::Array(_))
}
/// `readPlanData ?? readSearchToolData` (:1221-1249).
fn tool_result_payload(raw: &JsValue) -> JsValue {
    let data = field(raw, "data");
    if !object_like(data) {
        return JsValue::Undefined;
    }
    if field(data, "planFilePath").as_string().is_some() {
        return object([
            ("planFilePath", field(data, "planFilePath").clone()),
            ("planTitle", field(data, "planTitle").clone()),
            ("planSummary", field(data, "planSummary").clone()),
        ]);
    }
    let search = field(raw, "toolName").as_string().is_some_and(|name| {
        let name = String::from_utf16_lossy(name.units()).to_lowercase();
        name == "grep" || name == "glob"
    });
    when(search, || data.clone())
}

struct FlattenInput<'a> {
    index: usize,
    stamp: &'a JsValue,
    entry_id: &'a JsValue,
    fork_unsupported: bool,
}

/// `flattenCanonicalMessage` (:513-602) with `flushBlock` (:604-778).
struct Flattener<'a> {
    scope: &'a Scope<'a>,
    input: &'a FlattenInput<'a>,
    message: &'a JsValue,
    role: &'static str,
    out: Vec<JsObject>,
    text: Vec<u16>,
    text_block_id: JsValue,
    text_timeline: JsValue,
    pending_images: Vec<JsValue>,
}
impl Flattener<'_> {
    fn id(&self, label: &str, tail: Part<'_>, suffix: &str) -> JsValue {
        template(&[
            Part::Value(self.scope.session_key),
            Part::Text(label),
            Part::Number(self.input.index),
            Part::Text("-"),
            tail,
            Part::Text(suffix),
        ])
    }
    fn positional_id(&self, label: &str) -> JsValue {
        self.id(label, Part::Number(self.out.len()), "")
    }
    fn start(&self, id: JsValue, role: &str, kind: &str) -> JsObject {
        self.scope.message(id, self.input.stamp, role, &kind.into())
    }

    fn flush_text(&mut self) {
        if self.text.is_empty() && self.pending_images.is_empty() {
            return;
        }
        let role = self.role;
        let mut message = self.start(self.positional_id("-msg-"), role, "text");
        set(
            &mut message,
            "text",
            string_units(std::mem::take(&mut self.text)),
        );
        let block_id = std::mem::replace(&mut self.text_block_id, JsValue::Undefined);
        set(
            &mut message,
            "blockId",
            when(truthy(&block_id), || block_id),
        );
        let timeline = std::mem::replace(&mut self.text_timeline, JsValue::Undefined);
        set(
            &mut message,
            "timeline",
            when(truthy(&timeline), || timeline),
        );
        let model = field(field(self.message, "metadata"), "model");
        set(
            &mut message,
            "model",
            when(role == "assistant" && model.as_string().is_some(), || {
                model.clone()
            }),
        );
        let images = std::mem::take(&mut self.pending_images);
        set(
            &mut message,
            "images",
            when(!images.is_empty(), || JsValue::Array(images)),
        );
        set(
            &mut message,
            "payload",
            when(self.input.fork_unsupported, || {
                object([
                    ("forkUnsupportedContent", JsValue::Bool(true)),
                    (
                        "forkUnsupportedReason",
                        "This turn contains attachments or media.".into(),
                    ),
                ])
            }),
        );
        set(
            &mut message,
            "entryId",
            when(truthy(self.input.entry_id), || self.input.entry_id.clone()),
        );
        set(&mut message, "source", "history".into());
        self.out.push(message);
    }

    fn attachment(&mut self, block: &JsValue) {
        self.flush_text();
        let mut message = self.start(self.positional_id("-attachment-"), self.role, "status");
        set(
            &mut message,
            "text",
            template(&[
                Part::Text("["),
                Part::Value(field(block, "type")),
                Part::Text(" attachment]"),
            ]),
        );
        set(
            &mut message,
            "payload",
            object([
                ("mimeType", field(block, "mimeType").clone()),
                ("bytes", field(block, "bytes").clone()),
            ]),
        );
        set(&mut message, "source", "history".into());
        self.out.push(message);
    }

    fn flush_block(&mut self, block: &JsValue) {
        let kind = field(block, "type");
        if is(kind, "text") {
            self.text.extend(units(field(block, "text")));
        } else if is(kind, "thinking") {
            self.flush_text();
            let mut message = self.start(self.positional_id("-thinking-"), "assistant", "thinking");
            set(&mut message, "text", field(block, "text").clone());
            let block_id = field(block, "blockId");
            set(
                &mut message,
                "blockId",
                when(truthy(block_id), || block_id.clone()),
            );
            set(&mut message, "source", "history".into());
            self.out.push(message);
        } else if is(kind, "tool_call") {
            self.flush_text();
            let id = self.id("-tool-", Part::Value(field(block, "id")), "");
            let mut message = self.start(id, "tool", "tool_use");
            set(&mut message, "toolCallId", field(block, "id").clone());
            set(&mut message, "toolName", field(block, "name").clone());
            set(&mut message, "payload", field(block, "input").clone());
            set(&mut message, "source", "history".into());
            self.out.push(message);
        } else if is(kind, "tool_result") {
            self.flush_text();
            let raw = field(block, "raw");
            let id = self.id("-tool-", Part::Value(field(block, "toolCallId")), "-result");
            let mut message = self.start(id, "tool", "tool_result");
            set(
                &mut message,
                "toolCallId",
                field(block, "toolCallId").clone(),
            );
            set(
                &mut message,
                "toolName",
                non_empty_string(field(raw, "toolName")),
            );
            set(
                &mut message,
                "ok",
                JsValue::Bool(!truthy(field(block, "isError"))),
            );
            set(&mut message, "text", tool_result_text(block));
            set(
                &mut message,
                "errorCode",
                non_empty_string(field(field(raw, "error"), "code")),
            );
            set(&mut message, "payload", tool_result_payload(raw));
            let images: Vec<JsValue> = array_of(field(block, "content"))
                .iter()
                .filter(|item| is(field(item, "type"), "image"))
                .map(web_image)
                .collect();
            set(
                &mut message,
                "images",
                when(!images.is_empty(), || JsValue::Array(images)),
            );
            set(&mut message, "source", "history".into());
            self.out.push(message);
        } else if is(kind, "tool_result_reference") {
            self.flush_text();
            let id = self.id(
                "-tool-",
                Part::Value(field(block, "toolCallId")),
                "-result-ref",
            );
            let mut message = self.start(id, "tool", "tool_result");
            set(
                &mut message,
                "toolCallId",
                field(block, "toolCallId").clone(),
            );
            set(
                &mut message,
                "ok",
                JsValue::Bool(!truthy(field(block, "isError"))),
            );
            set(&mut message, "text", field(block, "preview").clone());
            set(&mut message, "resultPath", field(block, "path").clone());
            set(
                &mut message,
                "payload",
                object(
                    ["path", "originalBytes", "hasMore", "mimeType", "reason"]
                        .map(|key| (key, field(block, key).clone())),
                ),
            );
            set(&mut message, "source", "history".into());
            self.out.push(message);
        } else if is(kind, "media_reference") {
            self.flush_text();
            let mut message = self.start(self.positional_id("-media-"), "tool", "tool_result");
            set(
                &mut message,
                "toolCallId",
                field(block, "toolCallId").clone(),
            );
            set(&mut message, "ok", JsValue::Bool(true));
            set(&mut message, "text", field(block, "preview").clone());
            set(
                &mut message,
                "payload",
                object(
                    [
                        "path",
                        "originalBytes",
                        "hasMore",
                        "mimeType",
                        "mediaType",
                        "pages",
                        "detail",
                        "reason",
                    ]
                    .map(|key| (key, field(block, key).clone())),
                ),
            );
            set(&mut message, "source", "history".into());
            self.out.push(message);
        } else if is(kind, "image") {
            if self.role == "user" {
                self.pending_images.push(web_image(block));
            } else {
                self.attachment(block);
            }
        } else if is(kind, "pdf") || is(kind, "audio") {
            self.attachment(block);
        }
    }

    fn run(mut self) -> Vec<JsObject> {
        // The tool_result message a following user-role image attaches to.
        let mut last_tool_result: Option<usize> = None;
        for block in array_of(field(self.message, "content")) {
            let kind = field(block, "type");
            let timeline = field(block, "timeline");
            if is(kind, "text") {
                if &self.text_block_id != field(block, "blockId") {
                    self.flush_text();
                }
                self.text_block_id = field(block, "blockId").clone();
                self.text_timeline = timeline.clone();
            }
            if !is(kind, "image") && !is(kind, "tool_result") {
                last_tool_result = None;
            }
            if is(kind, "image")
                && self.role == "user"
                && let Some(target) = last_tool_result
            {
                let mut images = array_of(message_field(&self.out[target], "images")).to_vec();
                images.push(web_image(block));
                self.out[target].insert("images".into(), JsValue::Array(images));
                continue;
            }
            self.flush_block(block);
            if !is(kind, "text")
                && truthy(timeline)
                && let Some(last) = self.out.last_mut()
            {
                last.insert("timeline".into(), timeline.clone());
            }
            if is(kind, "tool_result") {
                last_tool_result = self.out.len().checked_sub(1);
            }
        }
        self.flush_text();
        let metadata = field(self.message, "metadata");
        for key in ["queueItemId", "moduleId"] {
            let value = non_empty_string(field(metadata, key));
            for message in &mut self.out {
                set(message, key, value.clone());
            }
        }
        self.out
    }
}

/// One visible transcript message with the entry fields the reader keeps.
struct Visible<'a> {
    message: &'a JsValue,
    entry: &'a JsValue,
    fork_unsupported: bool,
}

fn hidden(message: &JsValue) -> bool {
    let metadata = field(message, "metadata");
    truthy(field(metadata, "synthetic"))
        || field(metadata, "compactReplacement") == &JsValue::Bool(true)
}

/// `extractWebVisibleMessages` (:836-916) and, with `subagent`,
/// `extractSubagentExecutionMessages` (:918-974): the sidechain reader skips
/// the fork prelude and only keeps boundaries after the first execution
/// message.
fn visible_messages(entries: &[JsValue], subagent: bool) -> (Vec<Visible<'_>>, Vec<&JsValue>) {
    let mut messages = Vec::new();
    let mut boundaries = Vec::new();
    let mut saw_execution = false;
    for entry in entries {
        let kind = field(entry, "type");
        if is(kind, "accepted_input") {
            if subagent {
                continue;
            }
            let all = array_of(field(entry, "messages"));
            let fork_unsupported = all.iter().any(|message| {
                array_of(field(message, "content"))
                    .iter()
                    .any(|block| !is(field(block, "type"), "text"))
            });
            messages.extend(
                all.iter()
                    .filter(|message| !hidden(message))
                    .map(|message| Visible {
                        message,
                        entry,
                        fork_unsupported,
                    }),
            );
        } else if super::session_projections::is_durable_message_entry(entry) {
            saw_execution = true;
            let message = field(entry, "message");
            if !hidden(message) {
                messages.push(Visible {
                    message,
                    entry,
                    fork_unsupported: false,
                });
            }
        } else if is(kind, "control_boundary")
            && is(field(field(entry, "boundary"), "kind"), "compact")
            && (saw_execution || !subagent)
        {
            boundaries.push(entry);
        }
    }
    (messages, boundaries)
}

/// `compactBoundaryMetadata` (:807-834).
fn compact_boundary_metadata(entry: &JsValue) -> JsObject {
    let mut meta = JsObject::new();
    let boundary = field(entry, "boundary");
    let Some(metadata) = boundary
        .as_object()
        .filter(|_| is(field(boundary, "subtype"), "compact_boundary"))
        .and_then(|boundary| boundary.get("compactMetadata"))
    else {
        return meta;
    };
    let get = |key: &str| field(metadata, key);
    set(
        &mut meta,
        "timeline",
        when(truthy(get("timeline")), || get("timeline").clone()),
    );
    set(
        &mut meta,
        "compactionId",
        non_empty_string(get("compactionId")),
    );
    for key in ["trigger", "preTokens", "postTokens", "messagesSummarized"] {
        set(&mut meta, key, get(key).clone());
    }
    set(
        &mut meta,
        "targetTokens",
        when(get("targetTokens").as_number().is_some(), || {
            get("targetTokens").clone()
        }),
    );
    for key in ["summaryGenerated", "checkpointMerged"] {
        set(
            &mut meta,
            key,
            when(get(key).as_bool().is_some(), || get(key).clone()),
        );
    }
    set(
        &mut meta,
        "finalRatio",
        when(get("finalRatio").as_number().is_some(), || {
            get("finalRatio").clone()
        }),
    );
    for key in ["level", "stage", "stageLabel"] {
        set(&mut meta, key, get(key).clone());
    }
    meta
}

fn finite_sequence(message: &JsObject) -> Option<f64> {
    message_field(message, "sequence")
        .as_number()
        .filter(|n| n.is_finite())
}
fn string_order(value: &JsValue) -> &[u16] {
    value.as_string().map_or(&[], JsString::units)
}

/// `insertWebMessageByTranscriptOrder` (:1001-1022).
fn insert_by_transcript_order(all: &mut Vec<JsObject>, message: JsObject) {
    let at = match finite_sequence(&message) {
        Some(sequence) => all
            .iter()
            .position(|candidate| finite_sequence(candidate).is_some_and(|other| other > sequence))
            .unwrap_or(all.len()),
        None => {
            let created = string_order(message_field(&message, "createdAt"));
            all.iter()
                .rposition(|candidate| {
                    string_order(message_field(candidate, "createdAt")) <= created
                })
                .map_or(if all.is_empty() { all.len() } else { 0 }, |index| {
                    index + 1
                })
        }
    };
    all.insert(at, message);
}

/// `entry.entryId ?? fallback`, plus the trailing `entryId` every injected
/// message carries when the entry has one.
fn entry_identity(entry: &JsValue, fallback: impl FnOnce() -> JsValue) -> (JsValue, JsValue) {
    let entry_id = field(entry, "entryId");
    (
        if nullish(entry_id) {
            fallback()
        } else {
            entry_id.clone()
        },
        when(truthy(entry_id), || entry_id.clone()),
    )
}
fn turn_sequence_id(scope: &Scope<'_>, label: &str, entry: &JsValue) -> JsValue {
    template(&[
        Part::Value(scope.session_key),
        Part::Text(label),
        Part::Value(field(entry, "turnId")),
        Part::Text("-"),
        Part::Value(field(entry, "sequence")),
    ])
}

/// `injectCompactBoundaryMessages` (:980-999).
fn inject_compact_boundaries(scope: &Scope<'_>, boundaries: &[&JsValue], all: &mut Vec<JsObject>) {
    for entry in boundaries {
        let metadata = compact_boundary_metadata(entry);
        let (id, entry_id) = entry_identity(entry, || turn_sequence_id(scope, "-compact-", entry));
        let mut message = scope.message(
            id,
            field(entry, "createdAt"),
            "system",
            &"compact_boundary".into(),
        );
        set(
            &mut message,
            "timeline",
            metadata
                .get("timeline")
                .cloned()
                .unwrap_or(JsValue::Undefined),
        );
        set(&mut message, "turnId", field(entry, "turnId").clone());
        set(&mut message, "sequence", field(entry, "sequence").clone());
        set(&mut message, "text", "Context compacted".into());
        set(&mut message, "payload", JsValue::Object(metadata));
        set(&mut message, "source", "history".into());
        set(&mut message, "entryId", entry_id);
        insert_by_transcript_order(all, message);
    }
}

/// `attachSubagentIds` + `recoverCompletedSubagentToolResults` (:1029-1097).
fn link_subagents(started: &[JsValue], completed: &[JsValue], all: &mut Vec<JsObject>) {
    let mut tool_uses: Vec<(JsValue, JsObject)> = Vec::new();
    let mut queue = started.iter().map(|entry| field(entry, "subagentId"));
    let mut next = queue.next();
    for message in all.iter_mut() {
        let Some(subagent_id) = next else { break };
        if !is(message_field(message, "kind"), "tool_use") {
            continue;
        }
        let name = String::from_utf16_lossy(&units(or(
            message_field(message, "toolName"),
            &JsValue::String(JsString::default()),
        )))
        .to_lowercase();
        if name != "agent" && name != "task" {
            continue;
        }
        message.insert("subagentId".into(), subagent_id.clone());
        tool_uses.retain(|(id, _)| id != subagent_id);
        tool_uses.push((subagent_id.clone(), message.clone()));
        next = queue.next();
    }
    let result_key = |turn: &JsValue, call: &JsValue| {
        (
            if nullish(turn) {
                Vec::new()
            } else {
                units(turn)
            },
            units(call),
        )
    };
    let mut existing: Vec<_> = all
        .iter()
        .filter(|message| {
            is(message_field(message, "kind"), "tool_result")
                && truthy(message_field(message, "toolCallId"))
        })
        .map(|message| {
            result_key(
                message_field(message, "turnId"),
                message_field(message, "toolCallId"),
            )
        })
        .collect();
    for entry in completed {
        if field(entry, "errored") == &JsValue::Bool(true) {
            continue;
        }
        let Some((_, tool_use)) = tool_uses
            .iter()
            .find(|(id, _)| id == field(entry, "subagentId"))
        else {
            continue;
        };
        let get = |key: &str| message_field(tool_use, key);
        if !truthy(get("toolCallId")) {
            continue;
        }
        let key = result_key(get("turnId"), get("toolCallId"));
        if existing.contains(&key) {
            continue;
        }
        let mut message = JsObject::new();
        set(
            &mut message,
            "id",
            template(&[Part::Value(get("id")), Part::Text("-subagent-result")]),
        );
        set(&mut message, "sessionKey", get("sessionKey").clone());
        set(&mut message, "projectKey", get("projectKey").clone());
        set(&mut message, "createdAt", field(entry, "createdAt").clone());
        set(&mut message, "provider", "pilotdeck".into());
        set(&mut message, "role", "tool".into());
        set(&mut message, "kind", "tool_result".into());
        set(
            &mut message,
            "turnId",
            or(get("turnId"), field(entry, "turnId")).clone(),
        );
        set(&mut message, "sequence", field(entry, "sequence").clone());
        set(&mut message, "toolCallId", get("toolCallId").clone());
        set(&mut message, "toolName", get("toolName").clone());
        set(&mut message, "ok", JsValue::Bool(true));
        set(&mut message, "text", field(entry, "summaryPreview").clone());
        set(&mut message, "source", "history".into());
        let entry_id = field(entry, "entryId");
        set(
            &mut message,
            "entryId",
            when(truthy(entry_id), || entry_id.clone()),
        );
        insert_by_transcript_order(all, message);
        existing.push(key);
    }
}

/// `injectFileArtifactMessages` (:1099-1127).
fn inject_file_artifacts(scope: &Scope<'_>, entries: &[JsValue], all: &mut Vec<JsObject>) {
    for entry in entries {
        let (id, entry_id) =
            entry_identity(entry, || turn_sequence_id(scope, "-file-artifacts-", entry));
        let mut message = scope.message(
            id,
            field(entry, "createdAt"),
            "assistant",
            &"file_artifacts".into(),
        );
        set(&mut message, "turnId", field(entry, "turnId").clone());
        set(&mut message, "sequence", field(entry, "sequence").clone());
        set(&mut message, "artifacts", field(entry, "artifacts").clone());
        set(
            &mut message,
            "payload",
            object([("turnId", field(entry, "turnId").clone())]),
        );
        set(&mut message, "source", "history".into());
        set(&mut message, "entryId", entry_id);
        insert_by_transcript_order(all, message);
    }
}

/// `isI18nDescriptor` (:1197-1201).
fn i18n_descriptor(value: &JsValue) -> JsValue {
    when(field(value, "key").as_string().is_some(), || value.clone())
}

/// `injectAgentStatusMessages` (:1170-1195).
fn inject_agent_statuses(scope: &Scope<'_>, entries: &[JsValue], all: &mut Vec<JsObject>) {
    for entry in entries {
        let (id, _) = entry_identity(entry, || turn_sequence_id(scope, "-agent-status-", entry));
        let kind = field(entry, "kind");
        let detail = field(entry, "detail");
        let role = if is(kind, "error") { "error" } else { "system" };
        let mut message = scope.message(id, field(entry, "createdAt"), role, kind);
        set(&mut message, "turnId", field(entry, "turnId").clone());
        set(&mut message, "sequence", field(entry, "sequence").clone());
        set(&mut message, "text", field(entry, "text").clone());
        set(
            &mut message,
            "contentI18n",
            i18n_descriptor(field(detail, "messageI18n")),
        );
        set(
            &mut message,
            "userHintI18n",
            i18n_descriptor(field(detail, "userHintI18n")),
        );
        set(
            &mut message,
            "payload",
            object([
                ("event", field(entry, "event").clone()),
                ("detail", when(truthy(detail), || detail.clone())),
            ]),
        );
        set(&mut message, "source", "history".into());
        insert_by_transcript_order(all, message);
    }
}

/// `injectErrorTurnMessages` (:1135-1168).
fn inject_turn_errors(scope: &Scope<'_>, entries: &[&JsValue], all: &mut Vec<JsObject>) {
    for entry in entries {
        let result = field(entry, "result");
        let stop_reason = field(result, "stopReason");
        let mut text: Vec<u16> = Vec::new();
        for message in array_of(field(result, "errors"))
            .iter()
            .map(|error| field(error, "message"))
            .filter(|message| truthy(message))
        {
            if !text.is_empty() {
                text.push(u16::from(b'\n'));
            }
            text.extend(units(message));
        }
        let text = if text.is_empty() {
            template(&[Part::Text("Turn failed: "), Part::Value(stop_reason)])
        } else {
            string_units(text)
        };
        let id = template(&[
            Part::Value(scope.session_key),
            Part::Text("-turn-error-"),
            Part::Value(field(entry, "turnId")),
        ]);
        let mut message = scope.message(id, field(entry, "createdAt"), "error", &"error".into());
        set(&mut message, "turnId", field(entry, "turnId").clone());
        set(&mut message, "sequence", field(entry, "sequence").clone());
        set(&mut message, "text", text);
        set(
            &mut message,
            "payload",
            object([
                ("code", stop_reason.clone()),
                ("recoverable", JsValue::Bool(false)),
            ]),
        );
        set(&mut message, "source", "history".into());
        insert_by_transcript_order(all, message);
    }
}

fn flatten_all(scope: &Scope<'_>, visible: &[Visible<'_>], now: &JsValue) -> Vec<JsObject> {
    let mut all = Vec::new();
    for (index, item) in visible.iter().enumerate() {
        let created = field(item.entry, "createdAt");
        let input = FlattenInput {
            index,
            stamp: or(created, now),
            entry_id: field(item.entry, "entryId"),
            fork_unsupported: item.fork_unsupported,
        };
        let flattened = Flattener {
            scope,
            input: &input,
            message: item.message,
            role: if is(field(item.message, "role"), "user") {
                "user"
            } else {
                "assistant"
            },
            out: Vec::new(),
            text: Vec::new(),
            text_block_id: JsValue::Undefined,
            text_timeline: JsValue::Undefined,
            pending_images: Vec::new(),
        }
        .run();
        for mut message in flattened {
            set(&mut message, "turnId", field(item.entry, "turnId").clone());
            set(
                &mut message,
                "sequence",
                field(item.entry, "sequence").clone(),
            );
            all.push(message);
        }
    }
    all
}

fn ceil(value: f64) -> JsValue {
    JsValue::Number(value.ceil())
}

/// `tokenUsageFromProjection` (:168-218) and
/// `tokenUsageFromCompactBoundary` (:220-254).
fn token_usage(projection: &JsObject, options: &WebReadOptions) -> JsValue {
    let get = |key: &str| message_field(projection, key);
    let budget = get("latestContextBudget");
    let compact = get("latestCompactBudget");
    let positive = |value: Option<f64>| value.filter(|n| n.is_finite() && *n > 0.0);
    let max_context = positive(options.max_context_tokens);
    let max_output = positive(options.max_output_tokens);
    let number = JsValue::Number;
    if truthy(compact)
        && (!truthy(budget)
            || field(compact, "index").as_number() > field(budget, "index").as_number())
    {
        let previous = field(budget, "usage");
        let used = field(compact, "postTokens")
            .as_number()
            .unwrap_or(f64::NAN)
            .ceil();
        let total = positive_number(field(previous, "total"))
            .or(max_context)
            .unwrap_or(DEFAULT_HISTORY_CONTEXT_TOKENS);
        let reserved = positive_number(field(previous, "reservedOutputTokens"))
            .or(max_output)
            .unwrap_or(0.0);
        let effective = positive_number(field(previous, "effectiveTotal"))
            .unwrap_or_else(|| (total - reserved).max(1.0));
        let ratio = used / effective;
        let state = if ratio >= 0.95 {
            "blocking"
        } else if ratio >= 0.8 {
            "warning"
        } else {
            "ok"
        };
        return object([
            ("used", number(used)),
            ("displayUsed", number(used)),
            ("budgetUsed", number(used)),
            ("total", number(total)),
            ("effectiveTotal", number(effective)),
            ("reservedOutputTokens", number(reserved)),
            ("ratio", number(ratio)),
            ("state", state.into()),
            ("source", "compact".into()),
            ("exact", JsValue::Bool(false)),
            ("compacted", JsValue::Bool(true)),
            ("preCompactUsed", field(compact, "preTokens").clone()),
            (
                "messagesSummarized",
                field(compact, "messagesSummarized").clone(),
            ),
        ]);
    }
    if truthy(budget) {
        return field(budget, "usage").clone();
    }
    let turn = get("latestTurnUsage");
    if !truthy(turn) {
        return JsValue::Undefined;
    }
    let input = positive_number(field(turn, "inputTokens"));
    let output = positive_number(field(turn, "outputTokens")).unwrap_or(0.0);
    let cache_read = positive_number(field(turn, "cacheReadTokens")).unwrap_or(0.0);
    let cache_write = positive_number(field(turn, "cacheWriteTokens")).unwrap_or(0.0);
    let total_tokens = positive_number(field(turn, "totalTokens"));
    let used = match (input, total_tokens) {
        (Some(input), _) => (input + cache_read + cache_write).ceil(),
        (None, Some(total)) => (total - output).ceil().max(0.0),
        (None, None) => return JsValue::Undefined,
    };
    if used <= 0.0 {
        return JsValue::Undefined;
    }
    let total = max_context.unwrap_or(DEFAULT_HISTORY_CONTEXT_TOKENS);
    let reserved = max_output.unwrap_or(0.0);
    object([
        ("used", number(used)),
        ("total", number(total)),
        ("effectiveTotal", number((total - reserved).max(1.0))),
        ("reservedOutputTokens", number(reserved)),
        ("source", "history".into()),
        ("exact", JsValue::Bool(true)),
        (
            "breakdown",
            object([
                ("input", number(input.unwrap_or(0.0))),
                ("cacheRead", number(cache_read)),
                ("cacheWrite", number(cache_write)),
                ("output", number(output)),
                (
                    "total",
                    total_tokens.map_or_else(|| ceil(used + output), number),
                ),
            ]),
        ),
    ])
}

/// `Number.parseInt(String(value), 10)`.
fn parse_int(value: &JsValue) -> f64 {
    let text = String::from_utf16_lossy(&units(value));
    let text = crate::metadata::js_trim_start(&text);
    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, text.strip_prefix('+').unwrap_or(text)),
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    if end == 0 {
        return f64::NAN;
    }
    sign * digits[..end].parse::<f64>().unwrap_or(f64::NAN)
}
/// `parseCursor` (:480-484).
fn parse_cursor(cursor: &JsValue) -> f64 {
    if !truthy(cursor) {
        return 0.0;
    }
    let parsed = parse_int(cursor);
    if parsed.is_finite() && parsed >= 0.0 {
        parsed
    } else {
        0.0
    }
}
/// `offset + limit` where `limit` came off the wire untyped: a string
/// concatenates and the slice bound converts the result back to a number.
fn offset_plus(offset: f64, limit: &JsValue) -> f64 {
    match limit {
        JsValue::Number(limit) => offset + limit,
        JsValue::Bool(limit) => offset + f64::from(u8::from(*limit)),
        JsValue::String(limit) => {
            let mut text = crate::js_value::number_string(offset);
            text.push_str(&String::from_utf16_lossy(limit.units()));
            crate::js_value::string_to_number(&text)
        }
        _ => f64::NAN,
    }
}

/// `sessionInfoFromEntries` (:440-478): the catalog fallback.
pub(crate) fn session_info_from_entries(
    session_id: &JsValue,
    entries: &[JsValue],
    project_root: &JsValue,
) -> JsValue {
    let (Some(first), Some(last)) = (entries.first(), entries.last()) else {
        return JsValue::Undefined;
    };
    let mut merged = JsObject::new();
    let mut first_prompt = JsValue::Undefined;
    let mut last_prompt = JsValue::Undefined;
    for entry in entries {
        if is(field(entry, "type"), "accepted_input") {
            for message in array_of(field(entry, "messages")) {
                let text = array_of(field(message, "content"))
                    .iter()
                    .find(|block| is(field(block, "type"), "text"))
                    .and_then(|block| field(block, "text").as_string())
                    .map(|text| String::from_utf16_lossy(text.units()));
                let Some(text) = text
                    .as_deref()
                    .map(crate::metadata::js_trim)
                    .filter(|text| !text.is_empty())
                else {
                    continue;
                };
                if nullish(&first_prompt) {
                    first_prompt = text.into();
                }
                last_prompt = text.into();
            }
        } else if is(field(entry, "type"), "session_metadata") {
            let metadata = field(entry, "metadata");
            for key in [
                "title",
                "aiTitle",
                "tag",
                "parentSessionId",
                "forkedFromTurnId",
            ] {
                if !nullish(field(metadata, key)) {
                    merged.insert(key.into(), field(metadata, key).clone());
                }
            }
            first_prompt = or(field(metadata, "firstPrompt"), &first_prompt).clone();
            last_prompt = or(field(metadata, "lastPrompt"), &last_prompt).clone();
        }
    }
    let get = |key: &str| message_field(&merged, key).clone();
    let parse = |entry: &JsValue| {
        field(entry, "createdAt")
            .as_string()
            .and_then(|text| pilotdeck_transport::system_clock::parse_node22_date(text).ok())
            .flatten()
            .filter(|ms| ms.is_finite())
    };
    let summary = [
        get("title"),
        get("aiTitle"),
        last_prompt,
        first_prompt.clone(),
    ]
    .into_iter()
    .find(|value| !nullish(value))
    .unwrap_or_else(|| session_id.clone());
    object([
        ("sessionId", session_id.clone()),
        ("summary", summary),
        ("lastModified", JsValue::Number(parse(last).unwrap_or(0.0))),
        ("customTitle", get("title")),
        ("aiTitle", get("aiTitle")),
        ("firstPrompt", first_prompt),
        ("cwd", project_root.clone()),
        ("tag", get("tag")),
        (
            "createdAt",
            parse(first).map_or(JsValue::Undefined, JsValue::Number),
        ),
        ("parentSessionId", get("parentSessionId")),
        ("forkedFromTurnId", get("forkedFromTurnId")),
    ])
}

/// `isBackgroundTaskInput` (:348-355).
pub(crate) fn is_background_task_input(input: &JsValue) -> bool {
    is(field(input, "sessionKind"), "background_task")
        && field(input, "relativeTranscriptPath")
            .as_string()
            .is_some_and(|path| !path.is_empty())
}

/// `readWebSessionMessages` (:69-166) once the transcript is in hand.
/// `session_info` is the catalog row (or the entries fallback); `undefined`
/// for a background task.
pub(crate) fn web_session_messages(
    input: &JsValue,
    entries: &[JsValue],
    session_info: &JsValue,
    options: &WebReadOptions,
) -> JsValue {
    let session_key = field(input, "sessionKey");
    let scope = Scope {
        session_key,
        project_key: field(input, "projectKey"),
    };
    let now: JsValue = options.now_iso.as_str().into();
    let (started, completed) = subagent_entries(entries);
    let history = WebHistory::project(entries);
    let (visible, boundaries) = visible_messages(entries, false);
    let mut all = flatten_all(&scope, &visible, &now);
    inject_compact_boundaries(&scope, &boundaries, &mut all);
    link_subagents(&started, &completed, &mut all);
    if options.show_file_artifacts {
        inject_file_artifacts(&scope, &history.file_artifacts(), &mut all);
    }
    inject_agent_statuses(&scope, &history.agent_statuses, &mut all);
    inject_turn_errors(&scope, &history.turn_errors(), &mut all);
    let incomplete = history.incomplete_turn_ids();
    if !incomplete.is_empty() {
        let mut id = units(session_key);
        id.extend("-incomplete-turn-status-".encode_utf16());
        for (index, turn) in incomplete.iter().enumerate() {
            if index > 0 {
                id.push(u16::from(b'-'));
            }
            id.extend(units(turn));
        }
        let mut message = scope.message(string_units(id), &now, "system", &"status".into());
        set(
            &mut message,
            "text",
            "本轮记录尚未写入最终结果，已恢复当时产生的工具调用和输出。".into(),
        );
        set(
            &mut message,
            "payload",
            object([("incompleteTurnIds", JsValue::Array(incomplete))]),
        );
        set(&mut message, "source", "history".into());
        all.push(message);
    }

    let total = all.len();
    let offset = parse_cursor(field(input, "cursor"));
    let limit = field(input, "limit");
    let end = if nullish(limit) || limit == &JsValue::Number(0.0) {
        total
    } else {
        slice_index(offset_plus(offset, limit), total)
    };
    let start = slice_index(offset, total);
    let page: Vec<JsValue> = all
        .into_iter()
        .skip(start)
        .take(end.saturating_sub(start))
        .map(JsValue::Object)
        .collect();
    let next = offset + page.len() as f64;
    let next_cursor = when(truthy(limit) && next < total as f64, || {
        crate::js_value::number_string(next).as_str().into()
    });

    let info = |key: &str| field(session_info, key).clone();
    let background = is_background_task_input(input);
    object([
        ("messages", JsValue::Array(page)),
        ("nextCursor", next_cursor),
        ("total", JsValue::Number(total as f64)),
        ("tokenUsage", token_usage(&history.token_usage, options)),
        (
            "session",
            object([
                ("sessionId", or(&info("sessionId"), session_key).clone()),
                ("sessionKey", session_key.clone()),
                ("summary", or(&info("summary"), session_key).clone()),
                (
                    "lastModified",
                    or(&info("lastModified"), &JsValue::Number(0.0)).clone(),
                ),
                ("fileSize", info("fileSize")),
                ("customTitle", info("customTitle")),
                ("aiTitle", info("aiTitle")),
                ("firstPrompt", info("firstPrompt")),
                ("cwd", info("cwd")),
                ("tag", info("tag")),
                ("createdAt", info("createdAt")),
                ("sessionKind", when(background, || "background_task".into())),
                (
                    "parentSessionId",
                    or(field(input, "parentSessionId"), &info("parentSessionId")).clone(),
                ),
                (
                    "relativeTranscriptPath",
                    field(input, "relativeTranscriptPath").clone(),
                ),
                ("forkedFromTurnId", info("forkedFromTurnId")),
            ]),
        ),
    ])
}

/// `readSubagentWebMessages` (:262-346) once the sidechain transcript is in
/// hand: execution messages only, synthetic ones dropped, compact boundaries
/// injected. Message ids are scoped `<sessionKey>::sub::<subagentId>`.
pub(crate) fn web_subagent_messages(
    input: &JsValue,
    entries: &[JsValue],
    now_iso: &str,
) -> JsValue {
    let session_key = template(&[
        Part::Value(field(input, "sessionKey")),
        Part::Text("::sub::"),
        Part::Value(field(input, "subagentId")),
    ]);
    let scope = Scope {
        session_key: &session_key,
        project_key: field(input, "projectKey"),
    };
    let (visible, boundaries) = visible_messages(entries, true);
    let mut all = flatten_all(&scope, &visible, &now_iso.into());
    inject_compact_boundaries(&scope, &boundaries, &mut all);
    let total = all.len();
    object([
        (
            "messages",
            JsValue::Array(all.into_iter().map(JsValue::Object).collect()),
        ),
        ("total", JsValue::Number(total as f64)),
    ])
}
