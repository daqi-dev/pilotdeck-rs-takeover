//! `export_session_transcript` / `restore_session_transcript`: the
//! `exportSessionTranscript` (createLocalGateway.ts:1162-1182) and
//! `restoreSessionTranscript` (:1315-1370) callbacks, `toPortableSessionArchive`
//! (:1633-1678) and `validatePortableSessionArchive` (:1680-1700). The archive
//! is returned whole in one response, as the baseline does.
use super::requests::Requests;
use crate::runtime::connection::RequestError;
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

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
        let restoring = claim_restore(&session_key)?;
        let archive = field(&params, "archive").clone();
        validate(&archive)?;
        restore_validated(
            self.options.home.clone(),
            project_root,
            session_key,
            archive,
            restoring,
            Arc::new(crate::storage::JsonlTranscriptStore::new()),
        )
        .await
    }
}

fn claim_restore(session_key: &JsString) -> Result<Restoring, RequestError> {
    let mut set = RESTORING.lock().unwrap_or_else(|error| error.into_inner());
    let set = set.get_or_insert_with(HashSet::new);
    if !set.insert(session_key.clone()) {
        return Err(dialog(
            "SESSION_RESTORE_CONFLICT",
            "A transcript restore is already in progress for this session.",
        ));
    }
    Ok(Restoring(session_key.clone()))
}

fn target_conflict() -> RequestError {
    dialog(
        "SESSION_RESTORE_CONFLICT",
        "A persistent transcript already exists for this session.",
    )
}

/// Preserve a genuine storage errno without inferring one from its text.
/// Existing stage wrappers win over the fallback supplied by the actual call
/// site. A Rust-only lease error with no stage keeps its honest diagnostic.
fn restore_io_error(
    error: std::io::Error,
    fallback: Option<(&str, Option<&Path>)>,
) -> RequestError {
    let diagnostic = crate::node_io::staged_diagnostic(&error).or_else(|| {
        fallback.and_then(|(syscall, path)| {
            let path = path.map(|path| path.to_string_lossy());
            crate::node_io::diagnostic(&error, syscall, path.as_deref())
        })
    });
    if let Some((code, message)) = diagnostic {
        dialog(code, &message)
    } else if let Some((code, _)) = crate::node_io::lookup(&error) {
        dialog(code, &error.to_string())
    } else {
        RequestError::new(error.to_string())
    }
}
fn restore_ledger_error(
    error: crate::storage::LedgerError,
    fallback: Option<(&str, Option<&Path>)>,
) -> RequestError {
    match error {
        crate::storage::LedgerError::Io(error) => restore_io_error(error, fallback),
        error => RequestError::new(error.to_string()),
    }
}
fn restore_writer_error(error: crate::storage::AdapterError) -> RequestError {
    match error {
        crate::storage::AdapterError::Store(error) => restore_ledger_error(error, None),
        error => RequestError::new(error.to_string()),
    }
}

/// A directory or symlink is also an occupied target. In particular, a
/// dangling symlink must not be treated as a missing, writable transcript.
fn require_absent(path: &Path) -> Result<(), RequestError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Err(target_conflict()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(restore_io_error(error, Some(("lstat", Some(path))))),
    }
}

struct RestoreCancellation(Arc<AtomicBool>);
impl Drop for RestoreCancellation {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
fn check_restore_cancelled(cancelled: &AtomicBool) -> Result<(), RequestError> {
    if cancelled.load(Ordering::Acquire) {
        Err(RequestError::new(
            "Session transcript restore was cancelled.",
        ))
    } else {
        Ok(())
    }
}

async fn restore_validated(
    home: PathBuf,
    project_root: PathBuf,
    session_key: JsString,
    archive: JsValue,
    restoring: Restoring,
    store: Arc<dyn crate::storage::TranscriptStore>,
) -> Result<JsValue, RequestError> {
    let supplied = crate::runtime::session::transcript_locks::session_transcript_path(
        &home,
        &project_root,
        &session_key,
    );
    let target = tokio::task::spawn_blocking(move || {
        // Check before canonicalization so even a dangling leaf is a conflict.
        require_absent(&supplied)?;
        crate::storage::canonical_transcript_path(&supplied)
            .map_err(|error| restore_ledger_error(error, Some(("realpath", Some(&supplied)))))
    })
    .await
    .map_err(|error| RequestError::new(error.to_string()))??;
    let fence = crate::runtime::session::transcript_locks::acquire_key(target.clone()).await;
    let cancelled = Arc::new(AtomicBool::new(false));
    let _cancellation = RestoreCancellation(cancelled.clone());
    tokio::task::spawn_blocking(move || {
        // Cancellation of the awaiting request cannot release these while
        // the blocking writer still owns its stage and could publish it.
        let _restoring = restoring;
        let _fence = fence;
        let Some(_lease) = crate::runtime::session::factory::try_writer_lease_storage(&target)
            .map_err(|error| restore_ledger_error(error, None))?
        else {
            return Err(crate::runtime::session::transcript_locks::writer_busy_error());
        };
        write_restored(
            &home,
            &project_root,
            &session_key,
            &archive,
            &target,
            &cancelled,
            store,
        )
    })
    .await
    .map_err(|error| RequestError::new(error.to_string()))?
}

fn write_restored(
    home: &Path,
    project_root: &Path,
    session_key: &JsString,
    archive: &JsValue,
    target: &Path,
    cancelled: &AtomicBool,
    store: Arc<dyn crate::storage::TranscriptStore>,
) -> Result<JsValue, RequestError> {
    require_absent(target)?;
    check_restore_cancelled(cancelled)?;
    let (temporary_file, temporary) = crate::storage::create_temporary(target)
        .map_err(|error| restore_ledger_error(error, Some(("open", Some(target)))))?;
    let writer = crate::storage::TranscriptWriter::for_store(
        store,
        crate::storage::TranscriptKey {
            project_root: project_root.to_path_buf(),
            pilot_home: home.to_path_buf(),
            session_id: session_key.clone(),
            transcript_path: temporary.path().to_path_buf(),
        },
    );
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
        writer
            .record_session_metadata(session_key, &turn, JsValue::Object(metadata))
            .map_err(restore_writer_error)?;
    }
    check_restore_cancelled(cancelled)?;
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
    writer
        .record_accepted_input(session_key, &turn, canonical, Some(metadata))
        .map_err(restore_writer_error)?;
    check_restore_cancelled(cancelled)?;
    temporary_file
        .sync_all()
        .map_err(|error| restore_io_error(error, Some(("fsync", None))))?;
    check_restore_cancelled(cancelled)?;
    // Same-directory link atomically publishes the complete inode and fails
    // if anything already occupies the target. Never fall back to replacing
    // rename: an external creator need not participate in our writer lease.
    // A cancellation racing this commit may observe a complete published
    // archive; it can never expose a partially imported transcript.
    std::fs::hard_link(temporary.path(), target).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            target_conflict()
        } else {
            let mut failure = restore_io_error(error, Some(("link", Some(temporary.path()))));
            if failure.code != "gateway_request_failed" {
                let suffix = format!(" -> '{}'", target.display());
                failure.message = JsString::from_utf16(
                    failure
                        .message
                        .units()
                        .iter()
                        .copied()
                        .chain(suffix.encode_utf16())
                        .collect(),
                );
            }
            failure
        }
    })?;
    let mut result = JsObject::new();
    result.insert("sessionKey".into(), JsValue::String(session_key.clone()));
    result.insert(
        "importedMessages".into(),
        JsValue::Number(messages.len() as f64),
    );
    Ok(JsValue::Object(result))
}

#[cfg(test)]
#[path = "session_archive_s1_tests.rs"]
mod s1_tests;
