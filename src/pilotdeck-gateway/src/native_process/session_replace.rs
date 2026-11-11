//! `replace_last_turn` / `finalize_last_turn_replacement` storage ports:
//! `replaceLastWebSessionTurn` / `finalizeLastWebSessionTurnReplacement`
//! (web/server/replaceLastTurn.ts) over the node JSONL replacement transaction
//! (session/replacement/NodeProjectSessionReplacementPort.ts), the path
//! `pilotdeck server` takes (no `nativeSessionStorage`, so the bundle hands the
//! request to `sessionDataPlane.replacement`). Startup recovery of leftover
//! transactions is `recover` (`recoverNodeProjectSessionReplacements`, run by
//! createLocalGateway.ts:656 before the gateway is published).
use super::Options;
use crate::runtime::replacement::{ReplacementError, js_whitespace};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::path::{Path, PathBuf};

static UNDEFINED: JsValue = JsValue::Undefined;

fn field<'a>(value: &'a JsValue, key: &str) -> &'a JsValue {
    value.get(key).unwrap_or(&UNDEFINED)
}

fn is(value: &JsValue, text: &str) -> bool {
    matches!(value, JsValue::String(s) if s.equals_str(text))
}

fn coded(code: &'static str, message: impl Into<String>) -> ReplacementError {
    ReplacementError {
        code: Some(code),
        message: message.into(),
        details: None,
    }
}

fn plain(message: impl Into<String>) -> ReplacementError {
    ReplacementError::new(message)
}

/// `typeof value === "string" && value.trim()`.
fn non_blank(value: &JsValue) -> Option<&JsString> {
    match value {
        JsValue::String(text) if !text.units().iter().all(|unit| js_whitespace(*unit)) => {
            Some(text)
        }
        _ => None,
    }
}

/// `/^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i`.
fn is_transaction_id(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    bytes.iter().enumerate().all(|(index, byte)| match index {
        8 | 13 | 18 | 23 => *byte == b'-',
        14 => (b'1'..=b'5').contains(byte),
        19 => matches!(byte.to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b'),
        _ => byte.is_ascii_hexdigit(),
    })
}

fn validate_transaction_id(text: &str) -> Result<(), ReplacementError> {
    if is_transaction_id(text) {
        Ok(())
    } else {
        Err(coded(
            "replace_invalid_transaction",
            "The replacement transaction is invalid.",
        ))
    }
}

fn now_iso() -> String {
    pilotdeck_transport::system_clock::format_epoch_iso(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64,
    )
}

fn stringify(value: &JsValue) -> Result<String, ReplacementError> {
    pilotdeck_protocol::stringify_json(value)
        .map_err(|error| plain(error.to_string()))?
        .ok_or_else(|| plain("transcript entry cannot be serialized"))
}

/// Node's `fs` error text: `ENOENT: no such file or directory, open '<path>'`.
fn node_io_message(error: &std::io::Error, path: &Path) -> String {
    crate::node_io::read_file(error, &path.to_string_lossy())
        .map_or_else(|| error.to_string(), |(_, message)| message)
}

/// `delete object[key]` for each key.
fn without(object: &JsObject, keys: &[&str]) -> JsObject {
    let mut copy = JsObject::new();
    for (key, value) in object.entries() {
        if !keys.iter().any(|name| key.equals_str(name)) {
            copy.insert(key.clone(), value.clone());
        }
    }
    copy
}

/// `projectKey ?? options.projectRoot`, then the bundle's `projectKey ? … :
/// fallbackProjectRoot`; both collapse to the same rule for the chat dir.
fn chat_dir(options: &Options, input: &JsValue) -> PathBuf {
    let key = match field(input, "projectKey") {
        JsValue::String(key) if !key.is_empty() => Some(key.clone()),
        _ => None,
    };
    let project_root = options.project_root_for(key.as_ref());
    crate::session_chat_dir(
        &options.home,
        &project_root,
        &project_root.to_string_lossy(),
    )
}

struct Paths {
    transcript: PathBuf,
    safe_id: String,
    backup: PathBuf,
    journal: PathBuf,
}

fn paths(chat_dir: &Path, session: &JsString, transaction: &str) -> Paths {
    let safe_id = crate::storage::sanitize_session_id(session);
    Paths {
        transcript: chat_dir.join(format!("{safe_id}.jsonl")),
        backup: chat_dir.join(format!(".{safe_id}.{transaction}.replace.bak")),
        journal: chat_dir.join(format!(".{safe_id}.{transaction}.replace.json")),
        safe_id,
    }
}

/// `writeFile(path, body, { mode: 0o600, flag })`.
fn write_file(path: &Path, body: &[u8], exclusive: bool) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).mode(0o600);
    if exclusive {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }
    options.open(path)?.write_all(body)
}

fn remove_forced(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// `mergeMetadata(first, second)`: a spread where a nullish `title` /
/// `linkedPullRequest` in `second` keeps the earlier value.
fn merge_metadata(target: &mut JsObject, metadata: &JsValue) {
    let JsValue::Object(metadata) = metadata else {
        return;
    };
    for (key, value) in metadata.entries() {
        if (key.equals_str("title") || key.equals_str("linkedPullRequest"))
            && matches!(value, JsValue::Null | JsValue::Undefined)
        {
            continue;
        }
        target.insert(key.clone(), value.clone());
    }
}

/// `createPreservedMetadataEntry` (replaceLastTurn.ts:84-118).
fn preserved_metadata_entry(
    entries: &[JsValue],
    preserved: &[JsValue],
    now: &str,
) -> Option<JsValue> {
    let mut metadata = JsObject::new();
    for entry in entries {
        if is(field(entry, "type"), "session_metadata") {
            merge_metadata(&mut metadata, field(entry, "metadata"));
        }
    }
    let mut metadata = without(&metadata, &["lastPrompt"]);
    if !preserved
        .iter()
        .any(|entry| is(field(entry, "type"), "accepted_input"))
    {
        metadata = without(&metadata, &["firstPrompt", "aiTitle"]);
    }
    let has_metadata = metadata.entries().iter().any(|(key, value)| {
        !key.equals_str("isSnapshot") && !key.equals_str("updatedAt") && !value.is_undefined()
    });
    if !has_metadata {
        return None;
    }
    let sequence = preserved.iter().fold(0.0_f64, |highest, entry| {
        pilotdeck_protocol::js_math::max(
            highest,
            field(entry, "sequence").as_number().unwrap_or(f64::NAN),
        )
    }) + 1.0;
    let parent = preserved
        .iter()
        .rev()
        .find_map(|entry| match field(entry, "entryId") {
            JsValue::String(id) if !id.is_empty() => Some(JsValue::String(id.clone())),
            _ => None,
        })
        .unwrap_or(JsValue::Null);
    let session = match entries.first().map(|entry| field(entry, "sessionId")) {
        Some(JsValue::Undefined | JsValue::Null) | None => JsValue::String("".into()),
        Some(value) => value.clone(),
    };
    metadata.insert("isSnapshot".into(), JsValue::Bool(true));
    metadata.insert("updatedAt".into(), JsValue::String(now.into()));
    let mut entry = JsObject::new();
    entry.insert("type".into(), "session_metadata".into());
    entry.insert("sessionId".into(), session);
    entry.insert("turnId".into(), "metadata-replace".into());
    entry.insert("sequence".into(), JsValue::Number(sequence));
    entry.insert("createdAt".into(), JsValue::String(now.into()));
    entry.insert(
        "entryId".into(),
        JsValue::String(uuid::Uuid::new_v4().to_string().into()),
    );
    entry.insert("parentEntryId".into(), parent);
    entry.insert("metadata".into(), JsValue::Object(metadata));
    Some(JsValue::Object(entry))
}

/// `removeGeneratedTitleFromPrefix`.
fn without_generated_title(entry: &JsValue) -> JsValue {
    if !is(field(entry, "type"), "session_metadata")
        || field(field(entry, "metadata"), "aiTitle").is_undefined()
    {
        return entry.clone();
    }
    let metadata = without(
        &field(entry, "metadata")
            .as_object()
            .cloned()
            .unwrap_or_default(),
        &["aiTitle"],
    );
    let mut copy = entry.as_object().cloned().unwrap_or_default();
    copy.insert("metadata".into(), JsValue::Object(metadata));
    JsValue::Object(copy)
}

/// `replaceLastWebSessionTurn` with the node replacement port.
pub(super) fn replace_last_turn(
    options: &Options,
    owner: &JsValue,
    input: &JsValue,
) -> Result<JsValue, ReplacementError> {
    let (Some(session), Some(expected), Some(replacement)) = (
        non_blank(field(input, "sessionKey")),
        non_blank(field(input, "expectedTurnId")),
        non_blank(field(input, "replacementTurnId")),
    ) else {
        return Err(coded(
            "replace_invalid_input",
            "sessionKey, expectedTurnId, and replacementTurnId are required.",
        ));
    };
    let chat_dir = chat_dir(options, input);
    let transcript = chat_dir.join(format!(
        "{}.jsonl",
        crate::storage::sanitize_session_id(session)
    ));
    let read =
        crate::storage::read_transcript(&transcript).map_err(|error| plain(error.to_string()))?;
    if read
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == "error")
    {
        return Err(coded(
            "replace_invalid_transcript",
            "The conversation transcript could not be safely rewritten.",
        ));
    }
    let entries = read.entries;
    let now = now_iso();
    // createReplacementPlan
    let Some(latest) = entries
        .iter()
        .rposition(|entry| is(field(entry, "type"), "accepted_input"))
    else {
        return Err(coded(
            "replace_empty_transcript",
            "No user turn is available to replace.",
        ));
    };
    let latest_turn = field(&entries[latest], "turnId");
    if !matches!(latest_turn, JsValue::String(turn) if turn == expected) {
        return Err(coded(
            "replace_turn_conflict",
            "The selected message is no longer the latest user turn.",
        ));
    }
    let prefix = &entries[..latest];
    let replacing_first = !prefix
        .iter()
        .any(|entry| is(field(entry, "type"), "accepted_input"));
    let mut rewritten: Vec<JsValue> = if replacing_first {
        prefix.iter().map(without_generated_title).collect()
    } else {
        prefix.to_vec()
    };
    if let Some(entry) = preserved_metadata_entry(&entries, &rewritten, &now) {
        rewritten.push(entry);
    }
    let transaction = uuid::Uuid::new_v4().to_string();
    prepare(
        &chat_dir,
        session,
        &transaction,
        replacement,
        &now,
        owner,
        &rewritten,
    )?;
    let mut result = JsObject::new();
    result.insert("sessionKey".into(), JsValue::String(session.clone()));
    result.insert("replacedTurnId".into(), latest_turn.clone());
    result.insert(
        "removedEntryCount".into(),
        JsValue::Number((entries.len() - latest) as f64),
    );
    result.insert("transactionId".into(), JsValue::String(transaction.into()));
    Ok(JsValue::Object(result))
}

/// `prepareNodeProjectSessionReplacement`.
fn prepare(
    chat_dir: &Path,
    session: &JsString,
    transaction: &str,
    replacement: &JsString,
    prepared_at: &str,
    owner: &JsValue,
    entries: &[JsValue],
) -> Result<(), ReplacementError> {
    validate_transaction_id(transaction)?;
    let paths = paths(chat_dir, session, transaction);
    let original = std::fs::read(&paths.transcript)
        .map_err(|error| plain(node_io_message(&error, &paths.transcript)))?;
    let mut body = String::new();
    for entry in entries {
        body.push_str(&stringify(entry)?);
        body.push('\n');
    }
    let temporary = chat_dir.join(format!(
        ".{}.{}.replace.tmp",
        paths.safe_id,
        uuid::Uuid::new_v4()
    ));
    let mut journal = JsObject::new();
    journal.insert("version".into(), JsValue::Number(2.0));
    journal.insert("transactionId".into(), JsValue::String(transaction.into()));
    journal.insert("sessionKey".into(), JsValue::String(session.clone()));
    journal.insert(
        "replacementTurnId".into(),
        JsValue::String(replacement.clone()),
    );
    journal.insert("preparedAt".into(), JsValue::String(prepared_at.into()));
    journal.insert("owner".into(), owner.clone());
    let journal = format!("{}\n", stringify(&JsValue::Object(journal))?);
    let result = (|| -> Result<(), ReplacementError> {
        write_file(&paths.journal, journal.as_bytes(), true)
            .map_err(|error| plain(node_io_message(&error, &paths.journal)))?;
        write_file(&paths.backup, &original, true)
            .map_err(|error| plain(node_io_message(&error, &paths.backup)))?;
        write_file(&temporary, body.as_bytes(), false)
            .map_err(|error| plain(node_io_message(&error, &temporary)))?;
        std::fs::rename(&temporary, &paths.transcript).map_err(|error| plain(error.to_string()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = remove_forced(&temporary);
        let _ = remove_forced(&paths.backup);
        let _ = remove_forced(&paths.journal);
    }
    result
}

/// `finalizeLastWebSessionTurnReplacement` with the node replacement port.
pub(super) fn finalize_last_turn_replacement(
    options: &Options,
    input: &JsValue,
) -> Result<JsValue, ReplacementError> {
    let (Some(session), Some(transaction)) = (
        non_blank(field(input, "sessionKey")),
        non_blank(field(input, "transactionId")),
    ) else {
        return Err(coded(
            "replace_invalid_transaction",
            "sessionKey and transactionId are required.",
        ));
    };
    let transaction_text = String::from_utf16_lossy(transaction.units());
    validate_transaction_id(&transaction_text)?;
    let action = field(input, "action");
    let commit = is(action, "commit");
    if !commit && !is(action, "rollback") {
        return Err(coded(
            "replace_invalid_action",
            "Replacement action must be commit or rollback.",
        ));
    }
    let chat_dir = chat_dir(options, input);
    let paths = paths(&chat_dir, session, &transaction_text);
    if commit {
        remove_forced(&paths.backup).map_err(|error| plain(error.to_string()))?;
        remove_forced(&paths.journal).map_err(|error| plain(error.to_string()))?;
    } else {
        let original = std::fs::read(&paths.backup).map_err(|error| {
            coded(
                "replace_transaction_missing",
                format!(
                    "The replacement transaction could not be restored: {}",
                    node_io_message(&error, &paths.backup)
                ),
            )
        })?;
        let temporary = chat_dir.join(format!(
            ".{}.{}.rollback.tmp",
            paths.safe_id,
            uuid::Uuid::new_v4()
        ));
        let restored = write_file(&temporary, &original, false)
            .and_then(|()| std::fs::rename(&temporary, &paths.transcript))
            .and_then(|()| remove_forced(&paths.backup))
            .and_then(|()| remove_forced(&paths.journal));
        if let Err(error) = restored {
            let _ = remove_forced(&temporary);
            return Err(plain(error.to_string()));
        }
    }
    let mut result = JsObject::new();
    result.insert("sessionKey".into(), JsValue::String(session.clone()));
    result.insert("transactionId".into(), JsValue::String(transaction.clone()));
    result.insert("action".into(), action.clone());
    Ok(JsValue::Object(result))
}

/// `recoverNodeProjectSessionReplacements` result.
#[derive(Default, Debug, PartialEq, Eq)]
pub(super) struct Recovery {
    pub committed: usize,
    pub rolled_back: usize,
    pub failures: Vec<(PathBuf, String)>,
}

struct Artifact {
    transaction: String,
    backup: PathBuf,
    journal: PathBuf,
}

/// The fields of a journal that `readReplacementJournalSync` accepts.
struct Journal {
    replacement_turn: JsString,
    prepared_at: Option<f64>,
    owner_pid: Option<i32>,
}

/// `readReplacementJournalSync`.
fn read_journal(path: &Path, transaction: &str) -> Option<Journal> {
    let text = std::fs::read_to_string(path).ok()?;
    let parsed = pilotdeck_protocol::parse_json(&text).ok()?;
    let version = field(&parsed, "version").as_number();
    if !matches!(version, Some(v) if v == 1.0 || v == 2.0)
        || !is(field(&parsed, "transactionId"), transaction)
        || non_blank(field(&parsed, "sessionKey")).is_none()
        || !matches!(field(&parsed, "preparedAt"), JsValue::String(_))
    {
        return None;
    }
    let replacement_turn = non_blank(field(&parsed, "replacementTurnId"))?.clone();
    let owner = field(&parsed, "owner");
    let owner_pid = if version == Some(2.0) {
        let pid = field(owner, "pid").as_number()?;
        non_blank(field(owner, "instanceId"))?;
        if pid.fract() != 0.0 || pid <= 0.0 || pid > 9_007_199_254_740_991.0 {
            return None;
        }
        Some(i32::try_from(pid as i64).unwrap_or(i32::MAX))
    } else {
        None
    };
    let prepared_at = field(&parsed, "preparedAt")
        .as_string()
        .and_then(crate::runtime::model::stats::parse_date_ms)
        .filter(|ms| ms.is_finite());
    Some(Journal {
        replacement_turn,
        prepared_at,
        owner_pid,
    })
}

/// `acceptedTurnIds`: turn ids of the parseable `accepted_input` lines.
fn accepted_turn_ids(body: &str) -> Vec<JsString> {
    let mut ids = Vec::new();
    for line in body.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if !line.contains("\"type\":\"accepted_input\"") || !line.contains("\"turnId\"") {
            continue;
        }
        if let Ok(entry) = pilotdeck_protocol::parse_json(line)
            && is(field(&entry, "type"), "accepted_input")
            && let JsValue::String(turn) = field(&entry, "turnId")
            && !ids.contains(turn)
        {
            ids.push(turn.clone());
        }
    }
    ids
}

fn mtime_ms(path: &Path) -> Option<f64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let since = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(since.as_secs_f64() * 1000.0)
}

/// `process.kill(pid, 0)` succeeding or failing with EPERM.
fn process_alive(pid: i32) -> bool {
    let Some(pid) = rustix::process::Pid::from_raw(pid) else {
        return false;
    };
    match rustix::process::test_kill_process(pid) {
        Ok(()) => true,
        Err(error) => error == rustix::io::Errno::PERM,
    }
}

/// `^\.(.+)\.([0-9a-f-]{36})\.replace\.(?:bak|json)$` plus `isTransactionId`:
/// the safe id and the transaction of an artifact file name.
fn artifact_name(name: &str) -> Option<(&str, &str)> {
    let stem = name
        .strip_prefix('.')?
        .strip_suffix(".replace.bak")
        .or_else(|| name[1..].strip_suffix(".replace.json"))?;
    let split = stem.len().checked_sub(37)?;
    if split == 0 || !stem.is_char_boundary(split) || stem.as_bytes()[split] != b'.' {
        return None;
    }
    let (safe_id, transaction) = (&stem[..split], &stem[split + 1..]);
    is_transaction_id(transaction).then_some((safe_id, transaction))
}

/// `recoverNodeProjectSessionReplacements(pilotHome)`: settle the replacement
/// transactions a previous gateway left behind, before this one is published
/// (createLocalGateway.ts:655-670).
pub(super) fn recover(pilot_home: &Path) -> Recovery {
    let mut recovery = Recovery::default();
    let chat_dirs: Vec<PathBuf> = match std::fs::read_dir(pilot_home.join("projects")) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.path().join("chats"))
            .collect(),
        Err(_) => Vec::new(),
    };
    // (transcript, safe id, artifacts) in discovery order, like the Map.
    let mut groups: Vec<(PathBuf, String, Vec<Artifact>)> = Vec::new();
    for chat_dir in &chat_dirs {
        let Ok(names) = std::fs::read_dir(chat_dir) else {
            continue;
        };
        for name in names.filter_map(Result::ok) {
            let name = name.file_name().to_string_lossy().into_owned();
            let Some((safe_id, transaction)) = artifact_name(&name) else {
                continue;
            };
            let transcript = chat_dir.join(format!("{safe_id}.jsonl"));
            let index = match groups.iter().position(|(path, _, _)| *path == transcript) {
                Some(index) => index,
                None => {
                    groups.push((transcript, safe_id.to_owned(), Vec::new()));
                    groups.len() - 1
                }
            };
            let artifacts = &mut groups[index].2;
            if !artifacts.iter().any(|a| a.transaction == transaction) {
                artifacts.push(Artifact {
                    transaction: transaction.to_owned(),
                    backup: chat_dir.join(format!(".{safe_id}.{transaction}.replace.bak")),
                    journal: chat_dir.join(format!(".{safe_id}.{transaction}.replace.json")),
                });
            }
        }
    }
    for (transcript, safe_id, artifacts) in groups {
        match recover_group(&transcript, &safe_id, &artifacts) {
            Ok(Some(true)) => recovery.committed += 1,
            Ok(Some(false)) => recovery.rolled_back += 1,
            Ok(None) => {}
            Err(message) => recovery.failures.push((transcript, message)),
        }
    }
    recovery
}

/// One transcript's artifacts: `Some(true)` committed, `Some(false)` rolled
/// back, `None` skipped (the owner is still alive).
fn recover_group(
    transcript: &Path,
    safe_id: &str,
    artifacts: &[Artifact],
) -> Result<Option<bool>, String> {
    let mut states: Vec<(f64, Option<Journal>, bool, &Artifact)> = artifacts
        .iter()
        .map(|artifact| {
            let journal = read_journal(&artifact.journal, &artifact.transaction);
            let backup = mtime_ms(&artifact.backup);
            let order = [
                journal.as_ref().and_then(|j| j.prepared_at).unwrap_or(0.0),
                backup.unwrap_or(0.0),
                mtime_ms(&artifact.journal).unwrap_or(0.0),
            ]
            .into_iter()
            .fold(f64::NEG_INFINITY, f64::max);
            (order, journal, backup.is_some(), artifact)
        })
        .collect();
    // Array.prototype.sort is stable; descending by order.
    states.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let Some((_, journal, has_backup, latest)) = states.first() else {
        return Ok(None);
    };
    if let Some(pid) = journal.as_ref().and_then(|j| j.owner_pid)
        && process_alive(pid)
    {
        return Ok(None);
    }
    let current = std::fs::read_to_string(transcript)
        .map(|body| accepted_turn_ids(&body))
        .unwrap_or_default();
    let committed = match journal {
        Some(journal) => current.contains(&journal.replacement_turn),
        None => match std::fs::read_to_string(&latest.backup) {
            Ok(body) => {
                let original = accepted_turn_ids(&body);
                current.iter().any(|turn| !original.contains(turn))
            }
            Err(_) => false,
        },
    };
    let clean = || -> Result<(), String> {
        for artifact in artifacts {
            remove_forced(&artifact.backup).map_err(|e| e.to_string())?;
            remove_forced(&artifact.journal).map_err(|e| e.to_string())?;
        }
        Ok(())
    };
    if committed {
        clean()?;
        return Ok(Some(true));
    }
    if !has_backup {
        return Err(format!(
            "Newest replacement transaction {} has no backup to restore.",
            latest.transaction
        ));
    }
    let original =
        std::fs::read(&latest.backup).map_err(|error| node_io_message(&error, &latest.backup))?;
    let directory = transcript.parent().unwrap_or(Path::new("/"));
    let temporary = directory.join(format!(".{safe_id}.{}.recovery.tmp", uuid::Uuid::new_v4()));
    let restored = write_file(&temporary, &original, false)
        .and_then(|()| std::fs::rename(&temporary, transcript));
    let _ = remove_forced(&temporary);
    restored.map_err(|error| error.to_string())?;
    clean()?;
    Ok(Some(false))
}

/// Startup half of createLocalGateway.ts:656-670, including its two console
/// lines (stdout for the counts, stderr for each failure).
pub(super) fn recover_and_report(pilot_home: &Path) {
    let recovery = recover(pilot_home);
    if recovery.committed > 0 || recovery.rolled_back > 0 {
        println!(
            "[pilotdeck] Recovered last-turn replacements: committed={} rolledBack={}.",
            recovery.committed, recovery.rolled_back
        );
    }
    for (scope, message) in &recovery.failures {
        eprintln!(
            "[pilotdeck] Could not recover replacement transaction for {}: {message}",
            scope.display()
        );
    }
}
