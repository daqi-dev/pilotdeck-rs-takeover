//! Restart recovery for user dialogs a previous gateway process left
//! unanswered (`<transcript>.dialogs.json`).
//!
//! Baseline: `ProjectRuntimeRegistry.ts:544-765` (list / acknowledge /
//! recover / clear over `createGatewayUserDialogJournal(storage)`) and the
//! answer text of `formatRecoveredUserDialogAnswer` (`:2093-2116`). This host
//! has no host dialog store, so the journal is the only source.

use super::session_overrides::SessionOverrides;
use crate::runtime::connection::RequestError;
use crate::runtime::session::transcript_locks;
use crate::user_dialog::{
    GatewayUserDialogJournal, JournalOptions, create_gateway_user_dialog_journal,
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

/// `recoveredUserDialogs`: the first `terminatedAt` a request was projected
/// with, so repeated lists report a stable value. Keyed by journal path,
/// then request id.
static TERMINATED_AT: LazyLock<Mutex<HashMap<PathBuf, HashMap<String, JsValue>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// `isEphemeralSession`: `sdkSessionConfigs.get(key)?.persistSession === false`.
pub(crate) fn is_ephemeral(overrides: &SessionOverrides, session: &JsString) -> bool {
    overrides
        .sdk_config(session)
        .and_then(|config| config.get("persistSession").cloned())
        == Some(JsValue::Bool(false))
}

fn journal(
    home: &Path,
    project_root: &Path,
    session: &JsString,
) -> Option<GatewayUserDialogJournal> {
    let transcript = transcript_locks::session_transcript_path(home, project_root, session);
    create_gateway_user_dialog_journal(Some(&transcript), JournalOptions::default())
}

fn request_id(dialog: &JsValue) -> String {
    dialog
        .get("request")
        .and_then(|request| request.get("requestId"))
        .and_then(JsValue::as_string)
        .map(|id| String::from_utf16_lossy(id.units()))
        .unwrap_or_default()
}

/// `loadRecoveredUserDialogs` (ProjectRuntimeRegistry.ts:732-765).
fn load(journal: &GatewayUserDialogJournal) -> Result<Vec<JsValue>, RequestError> {
    let mut recovered = journal
        .recover()
        .map_err(|error| RequestError::new(error.to_string()))?;
    let mut cache = TERMINATED_AT.lock().unwrap_or_else(|e| e.into_inner());
    if recovered.is_empty() {
        cache.remove(journal.path());
        return Ok(recovered);
    }
    let prior = cache.remove(journal.path()).unwrap_or_default();
    let mut next = HashMap::new();
    for dialog in &mut recovered {
        let id = request_id(dialog);
        if let (Some(at), JsValue::Object(object)) = (prior.get(&id), &mut *dialog) {
            object.insert("terminatedAt".into(), at.clone());
        }
        next.insert(
            id,
            dialog
                .get("terminatedAt")
                .cloned()
                .unwrap_or(JsValue::Undefined),
        );
    }
    cache.insert(journal.path().to_path_buf(), next);
    Ok(recovered)
}

/// `listRecoveredUserDialogsForSdk`.
pub(crate) fn list(
    home: &Path,
    project_root: &Path,
    overrides: &SessionOverrides,
    session: &JsString,
) -> Result<Vec<JsValue>, RequestError> {
    if is_ephemeral(overrides, session) {
        return Ok(Vec::new());
    }
    match journal(home, project_root, session) {
        Some(journal) => load(&journal),
        None => Ok(Vec::new()),
    }
}

/// `clearRecoveredUserDialogsForSdk`: a new turn supersedes every dialog the
/// previous process left behind.
pub(crate) fn clear(
    home: &Path,
    project_root: &Path,
    overrides: &SessionOverrides,
    session: &JsString,
) -> Result<(), RequestError> {
    let Some(journal) = journal(home, project_root, session) else {
        return Ok(());
    };
    TERMINATED_AT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(journal.path());
    if is_ephemeral(overrides, session) {
        return Ok(());
    }
    journal
        .clear()
        .map_err(|error| RequestError::new(error.to_string()))
}

/// The answer of a `user_dialog_respond` that found no live dialog.
pub(crate) struct RecoveryAnswer {
    pub behavior_answered: bool,
    pub value: JsValue,
    pub reason: Option<JsString>,
}

/// `recoverUserDialogForSdk` → `writeRecoveredUserDialogAnswer`
/// (ProjectRuntimeRegistry.ts:632-722). `Ok(false)` when the request is not a
/// recovered dialog of this session.
pub(crate) async fn recover(
    home: &Path,
    project_root: &Path,
    overrides: &SessionOverrides,
    recorders: &super::assembly::Recorders,
    session: &JsString,
    request: &JsString,
    answer: RecoveryAnswer,
) -> Result<bool, RequestError> {
    if is_ephemeral(overrides, session) {
        return Ok(false);
    }
    let transcript = transcript_locks::session_transcript_path(home, project_root, session);
    // One recovery write at a time per transcript, like the TS per-session
    // write queue; the fence also excludes the other cold metadata writers.
    let fence = transcript_locks::acquire(&transcript).await?;
    let home = home.to_path_buf();
    let project_root = project_root.to_path_buf();
    let session = session.clone();
    let request = request.clone();
    let recorders = recorders.clone();
    tokio::task::spawn_blocking(move || {
        let _fence = fence;
        let Some(journal) = journal(&home, &project_root, &session) else {
            return Ok(false);
        };
        let dialogs = load(&journal)?;
        let wanted = String::from_utf16_lossy(request.units());
        let Some(dialog) = dialogs.iter().find(|dialog| request_id(dialog) == wanted) else {
            return Ok(false);
        };
        let pending = dialog.get("request").cloned().unwrap_or(JsValue::Undefined);
        if answer.behavior_answered && !accepts(&pending, &answer.value) {
            return Err(RequestError {
                code: "INVALID_USER_DIALOG_RESPONSE".into(),
                message: format!(
                    "answered {} dialog result does not match the recovered dialog contract.",
                    text(&pending, "dialogKind")
                )
                .into(),
                details: None,
                validation: None,
            });
        }
        let purpose = format!("gateway_user_dialog_recovery:{wanted}");
        let persisted = crate::storage::read_transcript(&transcript)
            .map(|read| {
                read.entries.iter().any(|entry| {
                    let kind = entry.get("type").and_then(JsValue::as_string);
                    kind.is_some_and(|kind| {
                        kind.equals_str("durable_message") || kind.equals_str("tool_result_message")
                    }) && entry
                        .get("message")
                        .and_then(|m| m.get("metadata"))
                        .and_then(|m| m.get("purpose"))
                        .and_then(JsValue::as_string)
                        .is_some_and(|p| p.equals_str(&purpose))
                })
            })
            .unwrap_or(false);
        if !persisted {
            write_recovery(
                &home,
                &project_root,
                &recorders,
                &session,
                &pending,
                &answer,
                &purpose,
            )?;
        }
        journal
            .remove(&request)
            .map_err(|error| RequestError::new(error.to_string()))?;
        let mut cache = TERMINATED_AT.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entries) = cache.get_mut(journal.path()) {
            entries.remove(&wanted);
            if entries.is_empty() {
                cache.remove(journal.path());
            }
        }
        Ok(true)
    })
    .await
    .map_err(|error| RequestError::new(error.to_string()))?
}

fn write_recovery(
    home: &Path,
    project_root: &Path,
    recorders: &super::assembly::Recorders,
    session: &JsString,
    pending: &JsValue,
    answer: &RecoveryAnswer,
    purpose: &str,
) -> Result<(), RequestError> {
    let timestamp: JsValue = JsValue::String(
        pilotdeck_transport::system_clock::format_epoch_iso(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64,
        )
        .into(),
    );
    let turn = JsString::from(format!("gateway:dialog-recovery:{}", uuid::Uuid::new_v4()));
    let mut metadata = JsObject::new();
    metadata.insert("synthetic".into(), JsValue::Bool(true));
    metadata.insert("purpose".into(), JsValue::String(purpose.into()));
    if let Some(tool_call) = pending.get("toolCallId").filter(|v| !v.is_undefined()) {
        metadata.insert("toolCallId".into(), tool_call.clone());
    }
    let message = crate::metadata::object([
        ("role", "user".into()),
        (
            "content",
            JsValue::Array(vec![crate::metadata::object([
                ("type", "text".into()),
                (
                    "text",
                    JsValue::String(format_answer(pending, answer).into()),
                ),
            ])]),
        ),
        ("metadata", JsValue::Object(metadata)),
    ]);
    let mut durable = JsObject::new();
    durable.insert("type".into(), "durable_message".into());
    durable.insert("message".into(), message);
    let result = crate::metadata::object([
        ("type", "aborted".into()),
        ("sessionId", JsValue::String(session.clone())),
        ("turnId", JsValue::String(turn.clone())),
        ("stopReason", "aborted_tools".into()),
        ("usage", crate::metadata::object([])),
        ("permissionDenials", JsValue::Array(Vec::new())),
        ("turns", JsValue::Number(0.0)),
        ("startedAt", timestamp.clone()),
        ("completedAt", timestamp),
    ]);
    let live = || {
        let Some((owner, recorder)) = super::assembly::live_recorder(recorders, session) else {
            return Ok(None);
        };
        let writer = recorder.writer();
        writer
            .record_session_event(session, &turn, durable.clone())
            .and_then(|()| writer.record_turn_result(session, &turn, result.clone()))
            .map_err(|e| RequestError::new(e.to_string()))?;
        Ok(Some(owner))
    };
    transcript_locks::append_metadata_with_lease(home, project_root, session, live, |storage| {
        storage
            .transcript
            .record_session_event(session, &turn, durable.clone())
            .and_then(|()| {
                storage
                    .transcript
                    .record_turn_result(session, &turn, result.clone())
            })
            .map_err(|e| RequestError::new(e.to_string()))
    })
    .map(|_| ())
}

fn text(value: &JsValue, key: &str) -> String {
    value
        .get(key)
        .and_then(JsValue::as_string)
        .map(|s| String::from_utf16_lossy(s.units()))
        .unwrap_or_else(|| "undefined".into())
}

/// `acceptsRecoveredUserDialogAnswer` (ProjectRuntimeRegistry.ts:2079-2091).
fn accepts(request: &JsValue, value: &JsValue) -> bool {
    let kind = request.get("dialogKind").and_then(JsValue::as_string);
    let is = |name: &str| kind.is_some_and(|kind| kind.equals_str(name));
    if is("input") {
        return matches!(value, JsValue::String(_));
    }
    if is("select") {
        let JsValue::String(value) = value else {
            return false;
        };
        return match request.get("choices") {
            Some(JsValue::Array(choices)) => choices
                .iter()
                .any(|choice| choice.get("value").and_then(JsValue::as_string) == Some(value)),
            _ => false,
        };
    }
    if is("confirm") {
        return matches!(value, JsValue::Bool(_));
    }
    matches!(value, JsValue::Object(_))
        && crate::runtime::tools::builtin::form_dialog::accepts_form_dialog_answer(
            request.get("schema").unwrap_or(&JsValue::Undefined),
            value,
        )
}

/// `formatRecoveredUserDialogAnswer` (ProjectRuntimeRegistry.ts:2093-2116).
fn format_answer(request: &JsValue, answer: &RecoveryAnswer) -> String {
    let mut lines = vec![
        "[Gateway restart dialog recovery]".to_string(),
        format!(
            "The Gateway restarted while waiting for {}.",
            text(request, "toolName")
        ),
        format!("Question: {}", text(request, "prompt")),
    ];
    if !answer.behavior_answered {
        lines.push(match answer.reason.as_ref().filter(|r| !r.is_empty()) {
            Some(reason) => format!(
                "The user cancelled the dialog: {}",
                String::from_utf16_lossy(reason.units())
            ),
            None => "The user cancelled the dialog.".into(),
        });
        return lines.join("\n");
    }
    let kind = text(request, "dialogKind");
    let value_text = || match &answer.value {
        JsValue::String(s) => String::from_utf16_lossy(s.units()),
        _ => String::new(),
    };
    lines.push(match kind.as_str() {
        "input" => format!("The user answered: {}", value_text()),
        "select" => format!("The user selected: {}", value_text()),
        "confirm" => format!(
            "The user confirmed: {}",
            if answer.value == JsValue::Bool(true) {
                "yes"
            } else {
                "no"
            }
        ),
        _ => format!(
            "The user submitted: {}",
            pilotdeck_protocol::stringify_json(&answer.value)
                .ok()
                .flatten()
                .unwrap_or_default()
        ),
    });
    lines.join("\n")
}
