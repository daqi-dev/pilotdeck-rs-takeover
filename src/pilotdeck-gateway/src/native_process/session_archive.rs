//! `restore_session_transcript`: the `restoreSessionTranscript` callback of
//! createLocalGateway.ts:1315-1370 and `validatePortableSessionArchive`
//! (:1680-1700). `export_session_transcript` (which needs the conversation
//! replay of `replayTranscriptEntries`) is not ported yet.
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

impl Requests {
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
