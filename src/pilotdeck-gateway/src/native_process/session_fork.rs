//! `fork_session` storage port: `forkWebSession` (web/server/forkSession.ts)
//! over the node fork transaction (session/fork/NodeProjectSessionForkPort.ts),
//! the path `pilotdeck server` takes (no `nativeSessionStorage`, no session
//! execution provider, so neither `retargetExecutionEntry` nor
//! `executionStorageFork` is configured).
use super::Options;
use crate::runtime::replacement::ReplacementError;
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::path::{Path, PathBuf};

static UNDEFINED: JsValue = JsValue::Undefined;

fn field<'a>(value: &'a JsValue, key: &str) -> &'a JsValue {
    value.get(key).unwrap_or(&UNDEFINED)
}

fn is(value: &JsValue, text: &str) -> bool {
    matches!(value, JsValue::String(s) if s.equals_str(text))
}

fn fork_error(code: &'static str, message: impl Into<String>) -> ReplacementError {
    ReplacementError {
        code: Some(code),
        message: message.into(),
        details: None,
    }
}

fn plain(message: impl Into<String>) -> ReplacementError {
    ReplacementError::new(message)
}

/// `${value}` in a template literal.
fn template(value: &JsValue) -> Result<String, ReplacementError> {
    crate::js_value::to_string_units_checked(value)
        .map(|units| String::from_utf16_lossy(&units))
        .map_err(|error| plain(error.message()))
}

fn sequence(entry: &JsValue) -> f64 {
    field(entry, "sequence").as_number().unwrap_or(f64::NAN)
}

/// `sanitizeSessionIdForPath(input.sessionKey)`: the first property read on
/// the session key, so its TypeErrors are the method's.
fn session_key(input: &JsValue) -> Result<JsString, ReplacementError> {
    match field(input, "sessionKey") {
        JsValue::String(key) => Ok(key.clone()),
        JsValue::Undefined => Err(plain(
            "Cannot read properties of undefined (reading 'replace')",
        )),
        JsValue::Null => Err(plain("Cannot read properties of null (reading 'replace')")),
        _ => Err(plain("sessionId.replace is not a function")),
    }
}

struct ForkPoint<'a> {
    target: &'a JsValue,
    accepted: &'a JsValue,
    preserve_target: bool,
}

fn find_fork_point<'a>(
    entries: &'a [JsValue],
    from_entry_id: &JsValue,
    preserve_accepted_input: bool,
) -> Result<ForkPoint<'a>, ReplacementError> {
    let Some(target) = entries
        .iter()
        .find(|entry| strict_equals(field(entry, "entryId"), from_entry_id))
    else {
        return Err(fork_error(
            "fork_entry_not_found",
            format!("Transcript entry not found: {}", template(from_entry_id)?),
        ));
    };
    if is(field(target, "type"), "accepted_input") && !preserve_accepted_input {
        return Ok(ForkPoint {
            target,
            accepted: target,
            preserve_target: false,
        });
    }
    let Some(accepted) = entries.iter().find(|entry| {
        is(field(entry, "type"), "accepted_input")
            && strict_equals(field(entry, "turnId"), field(target, "turnId"))
    }) else {
        return Err(fork_error(
            "fork_turn_not_found",
            format!(
                "No accepted_input found for turn {}",
                template(field(target, "turnId"))?
            ),
        ));
    };
    Ok(ForkPoint {
        target,
        accepted,
        preserve_target: true,
    })
}

/// `===` for the JSON values a transcript holds.
fn strict_equals(a: &JsValue, b: &JsValue) -> bool {
    match (a, b) {
        (JsValue::String(a), JsValue::String(b)) => a == b,
        (JsValue::Number(a), JsValue::Number(b)) => a == b,
        (JsValue::Bool(a), JsValue::Bool(b)) => a == b,
        (JsValue::Null, JsValue::Null) | (JsValue::Undefined, JsValue::Undefined) => true,
        _ => false,
    }
}

fn validate_resume_drops_turn(
    entries: &[JsValue],
    point: &ForkPoint<'_>,
    resume_drops_turn: &JsValue,
) -> Result<(), ReplacementError> {
    if resume_drops_turn.is_undefined() {
        return Ok(());
    }
    const PREFIX: &str = "Resume rejected by --resume-drops-turn: ";
    let dropped = entries
        .iter()
        .find(|entry| strict_equals(field(entry, "entryId"), resume_drops_turn));
    let Some(dropped) = dropped.filter(|entry| is(field(entry, "type"), "accepted_input")) else {
        return Err(fork_error(
            "resume_drops_turn_invalid",
            format!("{PREFIX}the supplied UUID is not an accepted-input entry."),
        ));
    };
    let target_sequence = sequence(point.target);
    if sequence(dropped) <= target_sequence {
        return Err(fork_error(
            "resume_drops_turn_invalid",
            format!("{PREFIX}the dropped turn must occur after the resume point."),
        ));
    }
    let discarded: Vec<_> = entries
        .iter()
        .filter(|entry| sequence(entry) > target_sequence)
        .collect();
    if discarded.is_empty()
        || discarded
            .iter()
            .any(|entry| !strict_equals(field(entry, "turnId"), field(dropped, "turnId")))
    {
        return Err(fork_error(
            "resume_drops_turn_mismatch",
            format!(
                "{PREFIX}entries after the resume point are not attributable solely to the requested turn."
            ),
        ));
    }
    Ok(())
}

fn array(value: &JsValue) -> &[JsValue] {
    match value {
        JsValue::Array(items) => items,
        _ => &[],
    }
}

fn accepted_messages(entry: &JsValue) -> &[JsValue] {
    array(field(entry, "messages"))
}

fn extract_accepted_input_text(entry: &JsValue) -> String {
    let mut chunks = Vec::new();
    for message in accepted_messages(entry) {
        for block in array(field(message, "content")) {
            if is(field(block, "type"), "text")
                && let JsValue::String(text) = field(block, "text")
            {
                let text = String::from_utf16_lossy(text.units());
                let trimmed = crate::metadata::js_trim(&text);
                if !trimmed.is_empty() {
                    chunks.push(trimmed.to_owned());
                }
            }
        }
    }
    crate::metadata::js_trim(&chunks.join("\n\n")).to_owned()
}

fn has_unsupported_prefill_content(entry: &JsValue) -> bool {
    accepted_messages(entry).iter().any(|message| {
        array(field(message, "content"))
            .iter()
            .any(|block| !is(field(block, "type"), "text"))
    })
}

fn last_session_metadata(entries: &[JsValue]) -> Option<&JsValue> {
    entries
        .iter()
        .rev()
        .find(|entry| is(field(entry, "type"), "session_metadata"))
        .map(|entry| field(entry, "metadata"))
}

fn count_carried_messages(entries: &[&JsValue]) -> usize {
    entries
        .iter()
        .map(|entry| match field(entry, "type") {
            JsValue::String(kind) if kind.equals_str("accepted_input") => {
                accepted_messages(entry).len()
            }
            JsValue::String(kind)
                if kind.equals_str("assistant_message")
                    || kind.equals_str("tool_result_message")
                    || kind.equals_str("durable_message") =>
            {
                1
            }
            _ => 0,
        })
        .sum()
}

fn should_preserve(entry: &JsValue, point: &ForkPoint<'_>) -> bool {
    if !point.preserve_target {
        return sequence(entry) < sequence(point.accepted);
    }
    if sequence(entry) <= sequence(point.target) {
        return true;
    }
    strict_equals(field(entry, "turnId"), field(point.target, "turnId"))
        && (is(field(entry, "type"), "turn_result") || is(field(entry, "type"), "file_artifacts"))
}

/// `buildForkTitle`: whitespace runs collapse to one space, at most 48 UTF-16
/// units of it are kept (end-trimmed) before an ellipsis.
fn build_fork_title(prefill: &str, carried: usize, inherited: Option<&JsString>) -> JsString {
    let mut normalized = String::new();
    let mut in_space = false;
    for c in prefill.chars() {
        if crate::metadata::js_space(c) {
            if !in_space {
                normalized.push(' ');
            }
            in_space = true;
        } else {
            normalized.push(c);
            in_space = false;
        }
    }
    let normalized = crate::metadata::js_trim(&normalized);
    if !normalized.is_empty() {
        let units: Vec<u16> = normalized.encode_utf16().collect();
        let snippet = if units.len() > 48 {
            let head = String::from_utf16_lossy(&units[..48]);
            format!("{}…", crate::metadata::js_trim_end(&head))
        } else {
            normalized.to_owned()
        };
        return format!("⑂ {snippet}").into();
    }
    if let Some(inherited) = inherited {
        let mut units: Vec<u16> = "⑂ ".encode_utf16().collect();
        units.extend_from_slice(inherited.units());
        return JsString::from_utf16(units);
    }
    if carried > 0 {
        "⑂ Forked session".into()
    } else {
        "⑂ New branch".into()
    }
}

fn with(object: &JsValue, key: &str, value: JsValue) -> JsValue {
    let mut copy = object.as_object().cloned().unwrap_or_default();
    copy.insert(key.into(), value);
    JsValue::Object(copy)
}

/// `{...message, metadata: {...message.metadata, forkCarryover: {...}}}`.
fn mark_message(message: &JsValue, source_session: &JsValue, source_turn: &JsValue) -> JsValue {
    let mut metadata = match field(message, "metadata") {
        JsValue::Object(object) => object.clone(),
        _ => JsObject::new(),
    };
    let mut carryover = JsObject::new();
    carryover.insert("sourceSessionId".into(), source_session.clone());
    carryover.insert("sourceTurnId".into(), source_turn.clone());
    metadata.insert("forkCarryover".into(), JsValue::Object(carryover));
    with(message, "metadata", JsValue::Object(metadata))
}

fn compact_boundary(entry: &JsValue) -> Option<&JsValue> {
    let boundary = field(entry, "boundary");
    (is(field(entry, "type"), "control_boundary")
        && is(field(boundary, "kind"), "compact")
        && is(field(boundary, "subtype"), "compact_boundary"))
    .then_some(boundary)
}

/// `markTranscriptEntryAsForkCarryover` after the session id has been retargeted.
fn mark_entry(entry: &JsValue, source_session: &JsValue) -> Result<JsValue, ReplacementError> {
    let turn = field(entry, "turnId");
    let kind = field(entry, "type");
    if is(kind, "accepted_input") {
        let messages = accepted_messages(entry)
            .iter()
            .map(|message| mark_message(message, source_session, turn))
            .collect();
        return Ok(with(entry, "messages", JsValue::Array(messages)));
    }
    if is(kind, "assistant_message")
        || is(kind, "tool_result_message")
        || is(kind, "durable_message")
    {
        return Ok(with(
            entry,
            "message",
            mark_message(field(entry, "message"), source_session, turn),
        ));
    }
    if let Some(boundary) = compact_boundary(entry)
        && let Some(JsValue::Array(messages)) = crate::storage::compact_snapshot_messages(boundary)
            .map_err(|error| plain(error.to_string()))?
    {
        let mut snapshot = JsObject::new();
        snapshot.insert("version".into(), JsValue::Number(1.0));
        snapshot.insert(
            "messages".into(),
            JsValue::Array(
                messages
                    .iter()
                    .map(|message| mark_message(message, source_session, turn))
                    .collect(),
            ),
        );
        return Ok(with(
            entry,
            "boundary",
            with(boundary, "snapshot", JsValue::Object(snapshot)),
        ));
    }
    Ok(entry.clone())
}

fn retarget_entry(entry: &JsValue, session: &JsString) -> Result<JsValue, ReplacementError> {
    let retargeted = with(entry, "sessionId", JsValue::String(session.clone()));
    let kind = field(entry, "type");
    if is(kind, "accepted_input")
        || is(kind, "assistant_message")
        || is(kind, "tool_result_message")
        || is(kind, "durable_message")
        || compact_boundary(entry).is_some()
    {
        return mark_entry(&retargeted, field(entry, "sessionId"));
    }
    Ok(retargeted)
}

// ── node fork transaction (NodeProjectSessionForkPort.ts) ──

/// `path.resolve(p)` for an absolute or cwd-relative path, lexically.
fn resolve_path(path: &str) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let resolved = crate::metadata::resolve_units(&cwd.to_string_lossy(), &JsString::from(path));
    PathBuf::from(String::from_utf16_lossy(resolved.units()))
}

/// `retargetAuxiliaryPath`: a path inside the source session dir moves to the
/// same place under the target session dir; anything else is kept verbatim.
fn retarget_aux_path(path: &JsString, source_dir: &Path, target_dir: &Path) -> JsValue {
    let absolute = resolve_path(&String::from_utf16_lossy(path.units()));
    match absolute.strip_prefix(source_dir) {
        Ok(relative) if !relative.as_os_str().is_empty() => JsValue::String(
            target_dir
                .join(relative)
                .to_string_lossy()
                .into_owned()
                .into(),
        ),
        _ => JsValue::String(path.clone()),
    }
}

fn retarget_block(block: &JsValue, source_dir: &Path, target_dir: &Path) -> JsValue {
    let kind = field(block, "type");
    if is(kind, "tool_result_reference") || is(kind, "media_reference") {
        let path = match field(block, "path") {
            JsValue::String(path) => retarget_aux_path(path, source_dir, target_dir),
            other => other.clone(),
        };
        return with(block, "path", path);
    }
    block.clone()
}

fn retarget_content(message: &JsValue, source_dir: &Path, target_dir: &Path) -> JsValue {
    let content = array(field(message, "content"))
        .iter()
        .map(|block| retarget_block(block, source_dir, target_dir))
        .collect();
    with(message, "content", JsValue::Array(content))
}

fn retarget_aux_paths(
    entry: &JsValue,
    source_dir: &Path,
    target_dir: &Path,
) -> Result<JsValue, ReplacementError> {
    let kind = field(entry, "type");
    if is(kind, "accepted_input") {
        let messages = accepted_messages(entry)
            .iter()
            .map(|message| retarget_content(message, source_dir, target_dir))
            .collect();
        return Ok(with(entry, "messages", JsValue::Array(messages)));
    }
    if is(kind, "assistant_message")
        || is(kind, "tool_result_message")
        || is(kind, "durable_message")
    {
        return Ok(with(
            entry,
            "message",
            retarget_content(field(entry, "message"), source_dir, target_dir),
        ));
    }
    if let Some(boundary) = compact_boundary(entry)
        && let Some(JsValue::Array(messages)) = crate::storage::compact_snapshot_messages(boundary)
            .map_err(|error| plain(error.to_string()))?
    {
        let mut snapshot = JsObject::new();
        snapshot.insert("version".into(), JsValue::Number(1.0));
        snapshot.insert(
            "messages".into(),
            JsValue::Array(
                messages
                    .iter()
                    .map(|message| retarget_content(message, source_dir, target_dir))
                    .collect(),
            ),
        );
        return Ok(with(
            entry,
            "boundary",
            with(boundary, "snapshot", JsValue::Object(snapshot)),
        ));
    }
    Ok(entry.clone())
}

fn retarget_relative_session_path(
    path: &JsString,
    source_safe: &str,
    target_safe: &str,
) -> JsValue {
    let text = String::from_utf16_lossy(path.units());
    let mut parts = text.split(['/', '\\']);
    if parts.next() != Some(source_safe) {
        return JsValue::String(path.clone());
    }
    let rest: Vec<&str> = parts.collect();
    let mut joined = target_safe.to_owned();
    for part in rest {
        joined.push('/');
        joined.push_str(part);
    }
    JsValue::String(joined.into())
}

fn node_retarget(
    entry: &JsValue,
    source_safe: &str,
    target_safe: &str,
    source_dir: &Path,
    target_dir: &Path,
) -> Result<JsValue, ReplacementError> {
    let retargeted = retarget_aux_paths(entry, source_dir, target_dir)?;
    if !is(field(&retargeted, "type"), "subagent_started") {
        return Ok(retargeted);
    }
    let path = match field(&retargeted, "transcriptRelativePath") {
        JsValue::String(path) => retarget_relative_session_path(path, source_safe, target_safe),
        other => other.clone(),
    };
    Ok(with(&retargeted, "transcriptRelativePath", path))
}

// The external adapter keeps each sidechain under the owning session key.
fn legacy_retarget(
    entry: &JsValue,
    source: &crate::storage::SessionStorage,
    target: &crate::storage::SessionStorage,
) -> Result<JsValue, ReplacementError> {
    fn map_message(
        message: &JsValue,
        source: &crate::storage::SessionStorage,
        target: &crate::storage::SessionStorage,
    ) -> JsValue {
        let blocks = array(field(message, "content"))
            .iter()
            .map(|block| {
                if !is(field(block, "type"), "tool_result_reference")
                    && !is(field(block, "type"), "media_reference")
                {
                    return block.clone();
                }
                let JsValue::String(path) = field(block, "path") else {
                    return block.clone();
                };
                let absolute = resolve_path(&String::from_utf16_lossy(path.units()));
                let source_dir = source.subagents_dir.parent().unwrap_or(&source.chat_dir);
                let target_dir = target.subagents_dir.parent().unwrap_or(&target.chat_dir);
                for (from, to) in [
                    (
                        source.tool_results_dir.as_path(),
                        target.tool_results_dir.as_path(),
                    ),
                    (source_dir, target_dir),
                ] {
                    if let Ok(relative) = absolute.strip_prefix(from) {
                        if !relative.as_os_str().is_empty()
                            && !relative.to_string_lossy().starts_with("..")
                        {
                            return with(
                                block,
                                "path",
                                JsValue::String(
                                    to.join(relative).to_string_lossy().into_owned().into(),
                                ),
                            );
                        }
                    }
                }
                block.clone()
            })
            .collect();
        with(message, "content", JsValue::Array(blocks))
    }
    let kind = field(entry, "type");
    let adjusted = if is(kind, "accepted_input") {
        with(
            entry,
            "messages",
            JsValue::Array(
                accepted_messages(entry)
                    .iter()
                    .map(|message| map_message(message, source, target))
                    .collect(),
            ),
        )
    } else if is(kind, "assistant_message")
        || is(kind, "tool_result_message")
        || is(kind, "durable_message")
    {
        with(
            entry,
            "message",
            map_message(field(entry, "message"), source, target),
        )
    } else if let Some(boundary) = compact_boundary(entry) {
        if let Some(JsValue::Array(messages)) = crate::storage::compact_snapshot_messages(boundary)
            .map_err(|error| plain(error.to_string()))?
        {
            let mut snapshot = JsObject::new();
            snapshot.insert("version".into(), JsValue::Number(1.0));
            snapshot.insert(
                "messages".into(),
                JsValue::Array(
                    messages
                        .iter()
                        .map(|message| map_message(message, source, target))
                        .collect(),
                ),
            );
            with(
                entry,
                "boundary",
                with(boundary, "snapshot", JsValue::Object(snapshot)),
            )
        } else {
            entry.clone()
        }
    } else {
        entry.clone()
    };
    if !is(field(&adjusted, "type"), "subagent_started") {
        return Ok(adjusted);
    }
    let source_id = match field(entry, "sessionId") {
        JsValue::String(id) => crate::storage::sanitize_session_id(id),
        _ => String::new(),
    };
    let filename = target
        .transcript_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let target_id = filename.strip_suffix(".jsonl").unwrap_or(&filename);
    let target_id = crate::storage::sanitize_session_id(&JsString::from(target_id));
    let path = match field(&adjusted, "transcriptRelativePath") {
        JsValue::String(path) => retarget_relative_session_path(path, &source_id, &target_id),
        other => other.clone(),
    };
    Ok(with(&adjusted, "transcriptRelativePath", path))
}

fn sidechain_path(parent: &Path, entry: &JsValue) -> PathBuf {
    let path = match field(entry, "transcriptRelativePath") {
        JsValue::String(path) => String::from_utf16_lossy(path.units()),
        _ => String::new(),
    };
    resolve_path(
        &parent
            .parent()
            .unwrap_or(Path::new("/"))
            .join(path)
            .to_string_lossy(),
    )
}

fn copy_external_sidechains(
    source: &crate::storage::SessionStorage,
    target: &crate::storage::SessionStorage,
    entries: &[JsValue],
) -> Result<(), ReplacementError> {
    fn visit(
        source: &crate::storage::SessionStorage,
        target: &crate::storage::SessionStorage,
        source_parent: &Path,
        target_parent: &Path,
        entries: &[JsValue],
        seen: &mut std::collections::HashSet<PathBuf>,
    ) -> Result<(), ReplacementError> {
        for entry in entries {
            if !is(field(entry, "type"), "subagent_started") {
                continue;
            }
            let from = sidechain_path(source_parent, entry);
            if !seen.insert(from.clone()) {
                continue;
            }
            let transformed = legacy_retarget(entry, source, target)?;
            let to = sidechain_path(target_parent, &transformed);
            let read = source
                .read_transcript_at_path(&from, None)
                .map_err(|e| plain(e.to_string()))?;
            let errors: Vec<_> = read
                .diagnostics
                .iter()
                .filter(|d| d.severity == "error")
                .map(|d| d.message.as_str())
                .collect();
            if !errors.is_empty() {
                return Err(plain(format!(
                    "Cannot copy fork sidechain transcript {}: {}",
                    from.display(),
                    errors.join("; ")
                )));
            }
            let transformed_entries = read
                .entries
                .iter()
                .map(|entry| legacy_retarget(entry, source, target))
                .collect::<Result<Vec<_>, _>>()?;
            target.store().replace(&target.key_for_path(&to), &transformed_entries)
                .ok_or_else(|| plain("The configured external transcript store does not support forking sessions."))?
                .map_err(|e| plain(e.to_string()))?;
            visit(source, target, &from, &to, &read.entries, seen)?;
        }
        Ok(())
    }
    let mut seen = std::collections::HashSet::new();
    visit(
        source,
        target,
        &source.transcript_path,
        &target.transcript_path,
        entries,
        &mut seen,
    )?;
    Ok(())
}

fn copy_external_blobs(
    source: &crate::storage::SessionStorage,
    target: &crate::storage::SessionStorage,
    primary: &[JsValue],
) -> Result<(), ReplacementError> {
    if let Some(target_store) = target.file_history_backup_storage() {
        let source_store = source.file_history_backup_storage().ok_or_else(|| plain("The configured source transcript storage cannot read file-history backup payloads for fork creation."))?;
        let mut names = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for entry in primary {
            if !is(field(entry, "type"), "file_snapshot_recorded") {
                continue;
            }
            if let JsValue::Object(backups) = field(entry, "trackedFileBackups") {
                for (_, backup) in backups.own_entries() {
                    if let JsValue::String(name) = field(backup, "backupFileName")
                        && !name.is_empty()
                    {
                        let name = String::from_utf16_lossy(name.units());
                        if seen.insert(name.clone()) {
                            names.push(name);
                        }
                    }
                }
            }
        }
        for name in names {
            if let Some(bytes) = source_store.read(&name).map_err(|e| plain(e.to_string()))? {
                target_store
                    .write(&name, &bytes)
                    .map_err(|e| plain(e.to_string()))?;
            }
        }
    }
    if let (Some(source_store), Some(target_store)) = (
        source.tool_result_artifact_storage(),
        target.tool_result_artifact_storage(),
    ) {
        fn collect(
            source: &crate::storage::SessionStorage,
            parent: &Path,
            entries: &[JsValue],
            visited: &mut std::collections::HashSet<PathBuf>,
            names: &mut Vec<String>,
            seen: &mut std::collections::HashSet<String>,
        ) -> Result<(), ReplacementError> {
            for entry in entries {
                let messages: Vec<&JsValue> = if is(field(entry, "type"), "accepted_input") {
                    accepted_messages(entry).iter().collect()
                } else if is(field(entry, "type"), "assistant_message")
                    || is(field(entry, "type"), "tool_result_message")
                    || is(field(entry, "type"), "durable_message")
                {
                    vec![field(entry, "message")]
                } else {
                    vec![]
                };
                for message in messages {
                    for block in array(field(message, "content")) {
                        if !is(field(block, "type"), "tool_result_reference")
                            && !is(field(block, "type"), "media_reference")
                        {
                            continue;
                        }
                        if let JsValue::String(path) = field(block, "path") {
                            let absolute = resolve_path(&String::from_utf16_lossy(path.units()));
                            if let Ok(relative) = absolute.strip_prefix(&source.tool_results_dir) {
                                if relative.components().count() == 1 {
                                    if let Some(name) =
                                        relative.file_name().and_then(|name| name.to_str())
                                    {
                                        if !name.starts_with("..") && seen.insert(name.to_owned()) {
                                            names.push(name.to_owned());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if !is(field(entry, "type"), "subagent_started") {
                    continue;
                }
                let path = sidechain_path(parent, entry);
                if !visited.insert(path.clone()) {
                    continue;
                }
                let read = source
                    .read_transcript_at_path(&path, None)
                    .map_err(|e| plain(e.to_string()))?;
                let errors: Vec<_> = read
                    .diagnostics
                    .iter()
                    .filter(|d| d.severity == "error")
                    .map(|d| d.message.as_str())
                    .collect();
                if !errors.is_empty() {
                    return Err(plain(format!(
                        "Cannot inspect fork sidechain transcript {}: {}",
                        path.display(),
                        errors.join("; ")
                    )));
                }
                collect(source, &path, &read.entries, visited, names, seen)?;
            }
            Ok(())
        }
        let mut names = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut visited = std::collections::HashSet::new();
        collect(
            source,
            &source.transcript_path,
            primary,
            &mut visited,
            &mut names,
            &mut seen,
        )?;
        for name in names {
            if let Some(bytes) = source_store.read(&name).map_err(|e| plain(e.to_string()))? {
                target_store
                    .write(&name, &bytes)
                    .map_err(|e| plain(e.to_string()))?;
            }
        }
    }
    Ok(())
}

fn io_error(error: std::io::Error) -> ReplacementError {
    plain(error.to_string())
}

fn exists(path: &Path) -> Result<bool, ReplacementError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error(error)),
    }
}

/// `fs.cp(source, target, { recursive: true, force: true })`.
fn copy_tree(source: &Path, target: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(source)?;
    if metadata.is_dir() {
        std::fs::create_dir_all(target)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &target.join(entry.file_name()))?;
        }
        Ok(())
    } else if metadata.file_type().is_symlink() {
        let link = std::fs::read_link(source)?;
        let _ = std::fs::remove_file(target);
        std::os::unix::fs::symlink(link, target)
    } else {
        crate::node_io::copy_file(source, target).map(|_| ())
    }
}

fn rewrite_subagent_transcripts(
    dir: &Path,
    source_dir: &Path,
    target_dir: &Path,
) -> Result<(), ReplacementError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error(error)),
    };
    for entry in entries {
        let entry = entry.map_err(io_error)?;
        let path = entry.path();
        let kind = entry.file_type().map_err(io_error)?;
        if kind.is_dir() {
            rewrite_subagent_transcripts(&path, source_dir, target_dir)?;
            continue;
        }
        if !kind.is_file() || !entry.file_name().to_string_lossy().ends_with(".jsonl") {
            continue;
        }
        let content = std::fs::read_to_string(&path).map_err(io_error)?;
        let mut lines = Vec::new();
        for raw in content.split('\n') {
            let line = raw.strip_suffix('\r').unwrap_or(raw);
            if crate::metadata::js_trim(line).is_empty() {
                lines.push(line.to_owned());
                continue;
            }
            match pilotdeck_protocol::parse_json(line) {
                Ok(parsed) => {
                    let retargeted = retarget_aux_paths(&parsed, source_dir, target_dir)?;
                    lines.push(stringify(&retargeted)?);
                }
                Err(_) => lines.push(line.to_owned()),
            }
        }
        std::fs::write(&path, lines.join("\n")).map_err(io_error)?;
    }
    Ok(())
}

fn stringify(value: &JsValue) -> Result<String, ReplacementError> {
    pilotdeck_protocol::stringify_json(value)
        .map_err(|error| plain(error.to_string()))?
        .ok_or_else(|| plain("transcript entry cannot be serialized"))
}

fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

fn node_fork(
    chat_dir: &Path,
    source: &JsString,
    target: &JsString,
    entries: &[JsValue],
) -> Result<(), ReplacementError> {
    let source_safe = crate::storage::sanitize_session_id(source);
    let target_safe = crate::storage::sanitize_session_id(target);
    let source_dir = resolve_path(&chat_dir.join(&source_safe).to_string_lossy());
    let target_dir = resolve_path(&chat_dir.join(&target_safe).to_string_lossy());
    let transcript = chat_dir.join(format!("{target_safe}.jsonl"));
    let temporary = chat_dir.join(format!(".{target_safe}.{}.fork.tmp", uuid::Uuid::new_v4()));
    if exists(&transcript)? || exists(&target_dir)? {
        return Err(plain(format!(
            "Fork target session already exists: {}",
            String::from_utf16_lossy(target.units())
        )));
    }
    let mut created = false;
    let result = (|| -> Result<(), ReplacementError> {
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(chat_dir)
                .map_err(io_error)?;
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&target_dir)
                .map_err(io_error)?;
        }
        created = true;
        for sub in ["tool-results", "file-history", "subagents"] {
            let from = source_dir.join(sub);
            if !exists(&from)? {
                continue;
            }
            let to = target_dir.join(sub);
            copy_tree(&from, &to).map_err(io_error)?;
            if sub == "subagents" {
                rewrite_subagent_transcripts(&to, &source_dir, &target_dir)?;
            }
        }
        let mut body = String::new();
        for entry in entries {
            let retargeted =
                node_retarget(entry, &source_safe, &target_safe, &source_dir, &target_dir)?;
            body.push_str(&stringify(&retargeted)?);
            body.push('\n');
        }
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .map_err(io_error)?;
            file.write_all(body.as_bytes()).map_err(io_error)?;
        }
        std::fs::rename(&temporary, &transcript).map_err(io_error)?;
        set_mode(chat_dir, 0o700).map_err(io_error)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
        if created {
            let _ = std::fs::remove_dir_all(&target_dir);
        }
    }
    result
}

fn now_iso() -> JsValue {
    JsValue::String(
        pilotdeck_transport::system_clock::format_epoch_iso(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64,
        )
        .into(),
    )
}

/// The replacement coordinator's storage ports: `forkSession`,
/// `replaceLastTurn` and `finalizeLastTurnReplacement` of
/// `GatewaySessionHistoryBundle` (createLocalGateway.ts:890-918, 1485-1486).

pub(super) fn storage_with_adapter(
    options: &Options,
    adapter: Option<std::sync::Arc<crate::storage::AsyncTranscriptAdapter>>,
) -> crate::runtime::replacement::ReplacementStorage {
    use crate::runtime::replacement::StorageOperation;
    fn blocking(
        options: &Options,
        run: impl Fn(&Options, &JsValue) -> Result<JsValue, ReplacementError>
        + Send
        + Sync
        + Clone
        + 'static,
    ) -> StorageOperation {
        let options = options.clone();
        std::sync::Arc::new(move |input| {
            let (options, run) = (options.clone(), run.clone());
            Box::pin(async move {
                tokio::task::spawn_blocking(move || run(&options, &input))
                    .await
                    .map_err(|error| plain(error.to_string()))?
            })
        })
    }
    // `replacementTransactionOwner = { instanceId: randomUUID(), pid }`
    // (createLocalGateway.ts:655), one per gateway.
    let mut owner = JsObject::new();
    owner.insert(
        "instanceId".into(),
        JsValue::String(uuid::Uuid::new_v4().to_string().into()),
    );
    owner.insert("pid".into(), JsValue::Number(f64::from(std::process::id())));
    let owner = JsValue::Object(owner);
    crate::runtime::replacement::ReplacementStorage {
        fork_session: Some(blocking(options, {
            let adapter = adapter.clone();
            move |options, input| fork_session_with_adapter(options, adapter.as_deref(), input)
        })),
        replace_last_turn: Some(blocking(options, {
            let adapter = adapter.clone();
            move |options, input| {
                super::session_replace::replace_last_turn_with_adapter(
                    options,
                    adapter.as_deref(),
                    &owner,
                    input,
                )
            }
        })),
        finalize_last_turn_replacement: Some(blocking(options, move |options, input| {
            super::session_replace::finalize_with_adapter(options, adapter.as_deref(), input)
        })),
    }
}

/// `GatewaySessionHistoryBundle.forkSession` → `forkWebSession`.

pub(super) fn selected_storage(
    options: &Options,
    adapter: &crate::storage::AsyncTranscriptAdapter,
    input: &JsValue,
    session: &JsString,
) -> Result<crate::storage::SessionStorage, ReplacementError> {
    let key = field(input, "projectKey")
        .as_string()
        .filter(|key| !key.is_empty());
    let root = options.project_root_for(key);
    adapter
        .validate_project_layout(&crate::storage::ProjectKey {
            project_root: root.clone(),
            pilot_home: options.home.clone(),
        })
        .map_err(|e| plain(e.to_string()))?;
    adapter
        .create_session_storage(crate::storage::SessionStorageInput {
            project_root: root,
            pilot_home: options.home.clone(),
            session_id: session.clone(),
            now: None,
        })
        .map_err(|e| plain(e.to_string()))
}

fn fork_session_with_adapter(
    options: &Options,
    adapter: Option<&crate::storage::AsyncTranscriptAdapter>,
    input: &JsValue,
) -> Result<JsValue, ReplacementError> {
    // `projectKey ? projectKey : fallbackProjectRoot`.
    let project_key = match field(input, "projectKey") {
        JsValue::String(key) if !key.is_empty() => Some(key.clone()),
        _ => None,
    };
    let project_root = options.project_root_for(project_key.as_ref());
    let source = session_key(input)?;
    let selected = adapter
        .map(|adapter| selected_storage(options, adapter, input, &source))
        .transpose()?;
    let chat_dir = selected
        .as_ref()
        .map(|storage| storage.chat_dir.clone())
        .unwrap_or_else(|| {
            crate::session_chat_dir(
                &options.home,
                &project_root,
                &project_root.to_string_lossy(),
            )
        });
    let source_path = selected
        .as_ref()
        .map(|storage| storage.transcript_path.clone())
        .unwrap_or_else(|| {
            chat_dir.join(format!(
                "{}.jsonl",
                crate::storage::sanitize_session_id(&source)
            ))
        });
    if let Some(result) = selected
        .as_ref()
        .and_then(|storage| storage.recover_transcript_replacements())
    {
        result.map_err(|e| plain(e.to_string()))?;
    }
    let read = match &selected {
        Some(storage) => storage
            .read_transcript(None)
            .map_err(|e| plain(e.to_string()))?,
        None => crate::storage::read_transcript(&source_path).map_err(|e| plain(e.to_string()))?,
    };
    let entries = read.entries;
    if entries.is_empty() {
        return Err(fork_error(
            "fork_empty_transcript",
            "Cannot fork an empty session transcript.",
        ));
    }
    let point = find_fork_point(
        &entries,
        field(input, "fromEntryId"),
        matches!(field(input, "resumeAt"), JsValue::Bool(true)),
    )?;
    validate_resume_drops_turn(&entries, &point, field(input, "resumeDropsTurn"))?;
    if !point.preserve_target && has_unsupported_prefill_content(point.accepted) {
        return Err(fork_error(
            "fork_unsupported_content",
            "Forking messages with attachments or non-text input is not supported yet.",
        ));
    }
    let accepted_metadata = field(point.accepted, "metadata");
    let fork_mode = is(field(accepted_metadata, "permissionMode"), "plan");
    let run_mode = match field(accepted_metadata, "runMode") {
        JsValue::String(mode)
            if mode.equals_str("agent") || mode.equals_str("plan") || mode.equals_str("ask") =>
        {
            Some(mode.clone())
        }
        _ => None,
    };
    let preserved_source: Vec<&JsValue> = entries
        .iter()
        .filter(|entry| should_preserve(entry, &point))
        .collect();
    let fork_input_text = extract_accepted_input_text(point.accepted);
    let prefill = if point.preserve_target {
        String::new()
    } else {
        fork_input_text.clone()
    };
    let carried = count_carried_messages(&preserved_source);
    let new_session: JsString = format!("web:s_{}", uuid::Uuid::new_v4()).into();
    let preserved = preserved_source
        .iter()
        .map(|entry| retarget_entry(entry, &new_session))
        .collect::<Result<Vec<_>, _>>()?;
    let last_entry_id = match preserved.last().map(|entry| field(entry, "entryId")) {
        None | Some(JsValue::Undefined) | Some(JsValue::Null) => JsValue::Null,
        Some(id) => id.clone(),
    };
    let max_sequence = preserved.iter().fold(0.0_f64, |max, entry| {
        pilotdeck_protocol::js_math::max(max, sequence(entry))
    });
    let parent = last_session_metadata(&entries);
    let inherited = parent.and_then(|metadata| {
        [field(metadata, "title"), field(metadata, "aiTitle")]
            .into_iter()
            .find_map(|value| match value {
                JsValue::String(text) if !text.is_empty() => Some(text.clone()),
                _ => None,
            })
    });
    let title_source = if fork_input_text.is_empty() {
        &prefill
    } else {
        &fork_input_text
    };
    let title = build_fork_title(title_source, carried, inherited.as_ref());
    let first_prompt = if !fork_input_text.is_empty() {
        JsValue::String(fork_input_text.clone().into())
    } else if !prefill.is_empty() {
        JsValue::String(prefill.clone().into())
    } else {
        JsValue::Undefined
    };
    let mut metadata = JsObject::new();
    metadata.insert("parentSessionId".into(), JsValue::String(source.clone()));
    metadata.insert(
        "forkedFromTurnId".into(),
        field(point.accepted, "turnId").clone(),
    );
    metadata.insert("title".into(), JsValue::String(title));
    metadata.insert("firstPrompt".into(), first_prompt);
    let mut metadata_entry = JsObject::new();
    metadata_entry.insert("type".into(), "session_metadata".into());
    metadata_entry.insert("sessionId".into(), JsValue::String(new_session.clone()));
    metadata_entry.insert(
        "turnId".into(),
        JsValue::String(format!("fork-{}", uuid::Uuid::new_v4()).into()),
    );
    metadata_entry.insert("sequence".into(), JsValue::Number(max_sequence + 1.0));
    metadata_entry.insert("createdAt".into(), now_iso());
    metadata_entry.insert(
        "entryId".into(),
        JsValue::String(uuid::Uuid::new_v4().to_string().into()),
    );
    metadata_entry.insert("parentEntryId".into(), last_entry_id);
    metadata.insert("updatedAt".into(), now_iso());
    metadata_entry.insert("metadata".into(), JsValue::Object(metadata));
    let mut written = preserved;
    written.push(JsValue::Object(metadata_entry));
    if let Some(adapter) = adapter {
        let target = selected_storage(options, adapter, input, &new_session)?;
        let source_storage = selected.as_ref().expect("selected adapter storage");
        let source_entries: Vec<JsValue> = preserved_source
            .iter()
            .map(|entry| (*entry).clone())
            .collect();
        let mut rewritten = source_entries
            .iter()
            .map(|entry| {
                retarget_entry(
                    &legacy_retarget(entry, source_storage, &target)?,
                    &new_session,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        rewritten.push(written.last().expect("fork metadata entry").clone());
        if target.store().supports_replace() {
            copy_external_sidechains(source_storage, &target, &source_entries)?;
        }
        copy_external_blobs(source_storage, &target, &source_entries)?;
        target
            .replace_transcript(&rewritten)
            .ok_or_else(|| {
                fork_error(
                    "fork_unsupported_storage",
                    "The configured session storage does not support atomic fork creation.",
                )
            })?
            .map_err(|e| plain(e.to_string()))?;
    } else {
        node_fork(&chat_dir, &source, &new_session, &written)?;
    }
    let mut result = JsObject::new();
    result.insert("newSessionKey".into(), JsValue::String(new_session));
    result.insert("prefillText".into(), JsValue::String(prefill.into()));
    result.insert(
        "carriedMessageCount".into(),
        JsValue::Number(carried as f64),
    );
    if let Some(mode) = run_mode {
        result.insert("runMode".into(), JsValue::String(mode));
    }
    if fork_mode {
        result.insert("mode".into(), "plan".into());
    }
    Ok(JsValue::Object(result))
}
