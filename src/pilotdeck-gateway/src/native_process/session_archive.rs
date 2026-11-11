//! `export_session_transcript` / `restore_session_transcript`: the
//! `exportSessionTranscript` (createLocalGateway.ts:1162-1182) and
//! `restoreSessionTranscript` (:1315-1370) callbacks, `toPortableSessionArchive`
//! (:1633-1678) and `validatePortableSessionArchive` (:1680-1700). The archive
//! is returned whole in one response, as the baseline does.
use super::requests::Requests;
use crate::runtime::connection::RequestError;
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::{collections::HashSet, sync::Mutex};

const MAX_MESSAGES: usize = 10_000;
const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_TITLE_UNITS: usize = 512;

/// `restoringSessionKeys`: one process-wide set, like the closure-level Set.
static RESTORING: Mutex<Option<HashSet<JsString>>> = Mutex::new(None);

static UNDEFINED: JsValue = JsValue::Undefined;

fn field<'a>(value: &'a JsValue, key: &str) -> &'a JsValue {
    value.get(key).unwrap_or(&UNDEFINED)
}

fn dialog(code: &str, message: &str) -> RequestError {
    RequestError {
        code: code.into(),
        message: message.into(),
        details: None,
        validation: None,
    }
}

fn invalid(message: &str) -> RequestError {
    dialog("INVALID_SESSION_ARCHIVE", message)
}

/// JS truthiness for a JSON value.
fn truthy(value: &JsValue) -> bool {
    match value {
        JsValue::Undefined | JsValue::Null => false,
        JsValue::Bool(value) => *value,
        JsValue::Number(value) => *value != 0.0 && !value.is_nan(),
        JsValue::String(value) => !value.is_empty(),
        _ => true,
    }
}

fn is(value: &JsValue, text: &str) -> bool {
    matches!(value, JsValue::String(s) if s.equals_str(text))
}

fn validate(archive: &JsValue) -> Result<(), RequestError> {
    let messages = match field(archive, "messages") {
        JsValue::Array(messages)
            if truthy(archive)
                && field(archive, "schemaVersion").as_number() == Some(1.0)
                && is(field(archive, "format"), "portable_text_messages") =>
        {
            messages
        }
        _ => {
            return Err(invalid(
                "Unsupported or malformed session transcript archive.",
            ));
        }
    };
    if messages.is_empty() || messages.len() > MAX_MESSAGES {
        return Err(invalid(
            "Session transcript archive has an invalid message count.",
        ));
    }
    match field(archive, "title") {
        JsValue::Undefined => {}
        JsValue::String(title) if title.len_utf16() <= MAX_TITLE_UNITS => {}
        _ => return Err(invalid("Session transcript archive title is invalid.")),
    }
    for message in messages {
        if !truthy(message)
            || !(is(field(message, "role"), "user") || is(field(message, "role"), "assistant"))
            || !matches!(field(message, "text"), JsValue::String(_))
        {
            return Err(invalid(
                "Session transcript archive contains an invalid message.",
            ));
        }
    }
    let bytes = pilotdeck_protocol::stringify_json(archive)
        .map_err(|error| RequestError::new(error.to_string()))?
        .map_or(0, |text| text.len());
    if bytes > MAX_BYTES {
        return Err(invalid("Session transcript archive is too large."));
    }
    Ok(())
}

struct Restoring(JsString);
impl Drop for Restoring {
    fn drop(&mut self) {
        if let Some(set) = RESTORING.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            set.remove(&self.0);
        }
    }
}

/// JS `String.prototype.trim` over UTF-16 units.
fn trim_units(units: &[u16]) -> &[u16] {
    let space = crate::runtime::replacement::js_whitespace;
    let start = units.iter().position(|u| !space(*u)).unwrap_or(units.len());
    let end = units
        .iter()
        .rposition(|u| !space(*u))
        .map_or(start, |i| i + 1);
    &units[start..end]
}

/// `stringifyPortableTranscriptValue`: strings as-is, else
/// `JSON.stringify(value, null, 2)` (`undefined` prints as `undefined`).
fn portable_value(value: &JsValue) -> Vec<u16> {
    match value {
        JsValue::String(text) => text.units().to_vec(),
        JsValue::Undefined => "undefined".encode_utf16().collect(),
        _ => crate::runtime::memory::edgeclaw::llm::recall::pretty_json(value)
            .encode_utf16()
            .collect(),
    }
}

/// `portableTranscriptText`.
fn portable_text(message: &JsValue) -> Option<JsString> {
    let mut parts: Vec<Vec<u16>> = Vec::new();
    let blocks = match field(message, "content") {
        JsValue::Array(blocks) => blocks.as_slice(),
        _ => &[],
    };
    for block in blocks {
        let kind = field(block, "type");
        if is(kind, "text") {
            if let JsValue::String(text) = field(block, "text")
                && !text.is_empty()
            {
                parts.push(text.units().to_vec());
            }
        } else if is(kind, "tool_call") {
            let mut part: Vec<u16> = "[Tool call: ".encode_utf16().collect();
            part.extend(
                crate::js_value::to_string_units_checked(field(block, "name")).unwrap_or_default(),
            );
            part.extend("]\n".encode_utf16());
            part.extend(portable_value(field(block, "input")));
            parts.push(part);
        } else if is(kind, "tool_result") {
            let text = crate::runtime::context::compaction::flatten_tool_result_block_text(block);
            let mut part: Vec<u16> = "[Tool result]".encode_utf16().collect();
            if !text.is_empty() {
                part.push(u16::from(b'\n'));
                part.extend_from_slice(text.units());
            }
            parts.push(part);
        } else if is(kind, "tool_result_reference") {
            let mut part: Vec<u16> = "[Tool result reference: ".encode_utf16().collect();
            part.extend(
                crate::js_value::to_string_units_checked(field(block, "toolCallId"))
                    .unwrap_or_default(),
            );
            part.push(u16::from(b']'));
            parts.push(part);
        } else if is(kind, "image") {
            parts.push("[Image]".encode_utf16().collect());
        }
    }
    let joined = parts.join(&[u16::from(b'\n'), u16::from(b'\n')][..]);
    let text = trim_units(&joined);
    (!text.is_empty()).then(|| JsString::from_utf16(text.to_vec()))
}

/// `toPortableSessionArchive`.
fn portable_archive(entries: &[JsValue]) -> Result<JsValue, RequestError> {
    let replay = crate::runtime::session::factory::project_transcript(entries)
        .map_err(|error| RequestError::new(error.message.to_string()))?;
    let mut messages = Vec::new();
    for message in &replay.messages {
        if field(field(message, "metadata"), "synthetic") == &JsValue::Bool(true) {
            continue;
        }
        if let Some(text) = portable_text(message) {
            let mut out = JsObject::new();
            out.insert("role".into(), field(message, "role").clone());
            out.insert("text".into(), JsValue::String(text));
            messages.push(JsValue::Object(out));
        }
    }
    let title = match replay.metadata.get("title") {
        None | Some(JsValue::Undefined | JsValue::Null) => replay.metadata.get("aiTitle"),
        title => title,
    };
    let mut archive = JsObject::new();
    archive.insert("schemaVersion".into(), JsValue::Number(1.0));
    archive.insert("format".into(), "portable_text_messages".into());
    archive.insert("messages".into(), JsValue::Array(messages));
    // `title && title.length <= 512`; only a string title has a length here.
    if let Some(JsValue::String(title)) = title
        && !title.is_empty()
        && title.len_utf16() <= MAX_TITLE_UNITS
    {
        archive.insert("title".into(), JsValue::String(title.clone()));
    }
    Ok(JsValue::Object(archive))
}

impl Requests {
    pub(in crate::native_process) async fn export_session_transcript(
        &self,
        params: JsValue,
    ) -> Result<JsValue, RequestError> {
        let project_root = self.dialog_project(&params)?;
        let session_key = super::methods::require_text(
            &params,
            "sessionKey",
            "INVALID_SESSION_KEY",
            "sessionKey is required.",
        )?;
        if self.owner.has_active_turn(&session_key) {
            return Err(dialog(
                "SESSION_BUSY",
                "Cannot export a transcript while its session has an active turn.",
            ));
        }
        let path = crate::session_chat_dir(
            &self.options.home,
            &project_root,
            &project_root.to_string_lossy(),
        )
        .join(format!(
            "{}.jsonl",
            crate::storage::sanitize_session_id(&session_key)
        ));
        let read = tokio::task::spawn_blocking(move || crate::storage::read_transcript(&path))
            .await
            .map_err(|error| RequestError::new(error.to_string()))?
            .map_err(|error| RequestError::new(error.to_string()))?;
        if read
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "transcript_missing")
        {
            return Err(dialog(
                "SESSION_NOT_FOUND",
                &format!(
                    "Session not found: {}",
                    String::from_utf16_lossy(session_key.units())
                ),
            ));
        }
        if read
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == "error")
        {
            return Err(dialog(
                "SESSION_TRANSCRIPT_INVALID",
                "Cannot export a transcript with invalid entries.",
            ));
        }
        let archive = portable_archive(&read.entries)?;
        // validatePortableSessionArchive runs before the emptiness check, so an
        // empty export is rejected with the archive's own message count error
        // (createLocalGateway.ts:1176-1179, 1686) and SESSION_TRANSCRIPT_EMPTY
        // is never reached.
        validate(&archive)?;
        Ok(archive)
    }

    pub(in crate::native_process) async fn restore_session_transcript(
        &self,
        params: JsValue,
    ) -> Result<JsValue, RequestError> {
        let project_root = self.dialog_project(&params)?;
        let session_key = super::methods::require_text(
            &params,
            "sessionKey",
            "INVALID_SESSION_KEY",
            "sessionKey is required.",
        )?;
        self.owner.sweep_idle();
        if self.owner.has_active_turn(&session_key)
            || self.owner.current_handle(&session_key).is_some()
        {
            return Err(dialog(
                "SESSION_BUSY",
                "Cannot restore over an active Gateway session.",
            ));
        }
        let _restoring = {
            let mut set = RESTORING.lock().unwrap_or_else(|e| e.into_inner());
            let set = set.get_or_insert_with(HashSet::new);
            if set.contains(&session_key) {
                return Err(dialog(
                    "SESSION_RESTORE_CONFLICT",
                    "A transcript restore is already in progress for this session.",
                ));
            }
            // TS adds the key only after the archive and the target pass;
            // the window is synchronous there, so claiming here is the same.
            set.insert(session_key.clone());
            Restoring(session_key.clone())
        };
        let archive = field(&params, "archive").clone();
        validate(&archive)?;
        let home = self.options.home.clone();
        tokio::task::spawn_blocking(move || {
            write_restored(&home, &project_root, &session_key, &archive)
        })
        .await
        .map_err(|error| RequestError::new(error.to_string()))?
    }
}

fn write_restored(
    home: &std::path::Path,
    project_root: &std::path::Path,
    session_key: &JsString,
    archive: &JsValue,
) -> Result<JsValue, RequestError> {
    let home2 = home.to_path_buf();
    let adapter = crate::storage::AsyncTranscriptAdapter::new(
        std::sync::Arc::new(crate::storage::JsonlTranscriptStore::new()),
        std::sync::Arc::new(move |k: &crate::storage::ProjectKey| {
            crate::session_chat_dir(&home2, &k.project_root, &k.project_root.to_string_lossy())
        }),
    );
    let storage = adapter
        .create_session_storage(crate::storage::SessionStorageInput {
            project_root: project_root.to_path_buf(),
            pilot_home: home.to_path_buf(),
            session_id: session_key.clone(),
            now: None,
        })
        .map_err(|error| RequestError::new(error.to_string()))?;
    if storage.transcript_path.is_file() {
        return Err(dialog(
            "SESSION_RESTORE_CONFLICT",
            "A persistent transcript already exists for this session.",
        ));
    }
    let turn: JsString = format!("sdk:restore:{}", uuid::Uuid::new_v4()).into();
    let messages = match field(archive, "messages") {
        JsValue::Array(messages) => messages.clone(),
        _ => Vec::new(),
    };
    if let JsValue::String(title) = field(archive, "title")
        && !title.is_empty()
    {
        let mut metadata = JsObject::new();
        metadata.insert("title".into(), JsValue::String(title.clone()));
        metadata.insert(
            "updatedAt".into(),
            JsValue::String(
                pilotdeck_transport::system_clock::format_epoch_iso(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as i64,
                )
                .into(),
            ),
        );
        storage
            .transcript
            .record_session_metadata(session_key, &turn, JsValue::Object(metadata))
            .map_err(|error| RequestError::new(error.to_string()))?;
    }
    let canonical = messages
        .iter()
        .map(|message| {
            let mut block = JsObject::new();
            block.insert("type".into(), "text".into());
            block.insert("text".into(), field(message, "text").clone());
            let mut out = JsObject::new();
            out.insert("role".into(), field(message, "role").clone());
            out.insert(
                "content".into(),
                JsValue::Array(vec![JsValue::Object(block)]),
            );
            JsValue::Object(out)
        })
        .collect();
    let mut metadata = JsObject::new();
    metadata.insert("restoredFromGatewayArchive".into(), JsValue::Bool(true));
    metadata.insert("archiveSchemaVersion".into(), JsValue::Number(1.0));
    storage
        .transcript
        .record_accepted_input(session_key, &turn, canonical, Some(metadata))
        .map_err(|error| RequestError::new(error.to_string()))?;
    let mut result = JsObject::new();
    result.insert("sessionKey".into(), JsValue::String(session_key.clone()));
    result.insert(
        "importedMessages".into(),
        JsValue::Number(messages.len() as f64),
    );
    Ok(JsValue::Object(result))
}
