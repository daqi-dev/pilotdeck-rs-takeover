//! The raw provider-call audit (`storage/invocationStorage.ts`): with
//! `PILOTDECK_STORAGE_ROOT` set, every model request is staged under
//! `<root>/workspaces/<workspace>/sessions/<session>/llm/.pending/<requestLogId>.json`
//! before it is sent, and its terminal record is appended to
//! `llm/invocations.jsonl` once the attempt settles, which removes the
//! staged copy.
use std::{
    collections::HashSet,
    fs, io,
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
};

use pilotdeck_protocol::{JsObject, JsValue};

/// `InvocationStorageConfig` (invocationStorage.ts:18-42), the fields the
/// JSONL sink reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvocationStorageConfig {
    pub root: PathBuf,
    pub storage_config_version: String,
}

/// `resolveInvocationStorageConfig(env)`: a blank or missing
/// `PILOTDECK_STORAGE_ROOT` disables the audit; the version falls back to
/// `"env"`.
pub fn resolve_invocation_storage_config(
    env: impl Fn(&str) -> Option<String>,
) -> Option<InvocationStorageConfig> {
    let root = env("PILOTDECK_STORAGE_ROOT")?;
    let root = crate::metadata::js_trim(&root);
    if root.is_empty() {
        return None;
    }
    let root = crate::metadata::resolve_process_path(Path::new(""), Path::new(root)).ok()?;
    let version = env("PILOTDECK_STORAGE_CONFIG_VERSION")
        .map(|value| crate::metadata::js_trim(&value).to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "env".to_owned());
    Some(InvocationStorageConfig {
        root,
        storage_config_version: version,
    })
}

/// `JsonlInvocationLogSink`.
#[derive(Debug)]
pub struct JsonlInvocationLogSink {
    config: InvocationStorageConfig,
    /// The append queue plus the `appended` set: one terminal per
    /// `requestLogId`.
    appended: Mutex<HashSet<String>>,
}

fn text(record: &JsObject, key: &str) -> String {
    record
        .get(key)
        .and_then(JsValue::as_string)
        .map(|value| String::from_utf16_lossy(value.units()))
        .unwrap_or_default()
}

impl JsonlInvocationLogSink {
    pub fn new(config: InvocationStorageConfig) -> Self {
        Self {
            config,
            appended: Mutex::new(HashSet::new()),
        }
    }

    pub fn storage_config_version(&self) -> &str {
        &self.config.storage_config_version
    }

    fn path_for(&self, record: &JsObject) -> io::Result<PathBuf> {
        Ok(self
            .config
            .root
            .join("workspaces")
            .join(safe_part(&text(record, "workspaceId"))?)
            .join("sessions")
            .join(safe_part(&text(record, "sessionId"))?)
            .join("llm")
            .join("invocations.jsonl"))
    }

    /// `stage(record)`: synchronous and durable — a failure here keeps the
    /// provider request from being sent.
    pub fn stage(&self, record: &JsObject) -> io::Result<()> {
        let path = self.path_for(record)?;
        let pending = path.with_file_name(".pending");
        fs::create_dir_all(&pending)?;
        let staged = pending.join(format!(
            "{}.json",
            safe_part(&text(record, "requestLogId"))?
        ));
        let temporary = PathBuf::from(format!(
            "{}.tmp-{}",
            staged.to_string_lossy(),
            uuid::Uuid::new_v4()
        ));
        let body = stringify(record)?;
        let written = (|| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(body.as_bytes())?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, &staged)?;
            fs::File::open(&pending)?.sync_all()
        })();
        if written.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        written
    }

    /// `append(record)`: the terminal line, then the staged copy is removed.
    pub fn append(&self, record: &JsObject) -> io::Result<()> {
        let id = text(record, "requestLogId");
        let mut appended = self.appended.lock().unwrap_or_else(|e| e.into_inner());
        if appended.contains(&id) {
            return Ok(());
        }
        let path = self.path_for(record)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        file.write_all(format!("{}\n", stringify(record)?).as_bytes())?;
        if let Ok(staged) = safe_part(&id) {
            let _ = fs::remove_file(
                path.with_file_name(".pending")
                    .join(format!("{staged}.json")),
            );
        }
        appended.insert(id);
        Ok(())
    }
}

fn stringify(record: &JsObject) -> io::Result<String> {
    pilotdeck_protocol::stringify_json(&JsValue::Object(record.clone()))
        .map_err(|error| io::Error::other(error.to_string()))?
        .ok_or_else(|| io::Error::other("record is not JSON"))
}

/// `safePart` (invocationStorage.ts:429-437).
fn safe_part(value: &str) -> io::Result<String> {
    let invalid = || io::Error::other("Invalid storage identity");
    let raw = crate::metadata::js_trim(value);
    if raw.is_empty()
        || raw.contains('\0')
        || raw
            .split(['\\', '/'])
            .any(|segment| segment == "." || segment == "..")
    {
        return Err(invalid());
    }
    let mut replaced = String::with_capacity(raw.len());
    let mut dashes = false;
    for character in raw.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-') {
            replaced.push(character);
            dashes = false;
        } else if !dashes {
            replaced.push('-');
            dashes = true;
        }
    }
    let part = replaced.trim_matches('-');
    if part.is_empty() || part == "." || part == ".." {
        return Err(invalid());
    }
    Ok(part.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str) -> JsObject {
        let mut record = JsObject::new();
        record.insert("workspaceId".into(), "ws one".into());
        record.insert("sessionId".into(), "s/../x".into());
        record.insert("requestLogId".into(), JsValue::from(id));
        record
    }

    #[test]
    fn identities_are_sanitized_like_safe_part() {
        assert_eq!(safe_part(" a b  c ").unwrap(), "a-b-c");
        assert_eq!(safe_part("--x--").unwrap(), "x");
        assert!(safe_part("a/../b").is_err());
        assert!(safe_part("  ").is_err());
        assert!(safe_part("///").is_err());
    }

    #[test]
    fn stage_then_append_leaves_one_line_and_an_empty_pending_dir() {
        let root = std::env::temp_dir().join(format!("pdrs-invocation-{}", uuid::Uuid::new_v4()));
        let sink = JsonlInvocationLogSink::new(InvocationStorageConfig {
            root: root.clone(),
            storage_config_version: "env".into(),
        });
        let mut ok = record("r1");
        ok.insert("sessionId".into(), "s1".into());
        sink.stage(&ok).unwrap();
        let dir = root.join("workspaces/ws-one/sessions/s1/llm");
        assert!(dir.join(".pending/r1.json").exists());
        sink.append(&ok).unwrap();
        sink.append(&ok).unwrap();
        assert!(!dir.join(".pending/r1.json").exists());
        let lines = fs::read_to_string(dir.join("invocations.jsonl")).unwrap();
        assert_eq!(lines.lines().count(), 1, "one terminal per requestLogId");
        // A traversal identity is refused before anything is written.
        assert!(sink.stage(&record("r2")).is_err());
        let _ = fs::remove_dir_all(root);
    }
}
