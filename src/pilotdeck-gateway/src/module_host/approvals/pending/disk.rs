//! Persistent adapter around the same pending-state reducer used by HTTP and
//! WebSocket callers. Every operation reloads under a filesystem lock; a
//! receipt is acknowledged only after its atomic replacement is durable.
use super::*;
use base64::Engine as _;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Clone)]
pub(crate) struct DiskSopStore {
    profiles: ProfileResolver,
}
impl DiskSopStore {
    pub(crate) fn new(profiles: ProfileResolver) -> Self {
        Self { profiles }
    }
    /// Runtime transitions use the same directory lock and migration as
    /// approvals. The closure returns a replacement only for actual writes.
    pub(crate) async fn transact<T: Send + 'static>(
        &self,
        input: StoreStatusInput,
        operation: impl FnOnce(
            Option<JsValue>,
        ) -> Result<
            (Option<JsValue>, T),
            crate::module_host::staffdeck::sop::SopError,
        > + Send
        + 'static,
    ) -> Result<T, crate::module_host::staffdeck::sop::SopError> {
        use crate::module_host::staffdeck::sop::SopError;
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let root = this
                .root(input.project_key.as_ref())
                .map_err(SopError::from)?;
            fs::create_dir_all(&root).map_err(|_| SopError::from(storage_failure()))?;
            this.locked(
                input.project_key.as_ref(),
                &input.session_key,
                |store, raw, path| {
                    let root = (store.profiles)(None).ok_or_else(storage_failure)?;
                    let states = store.states.lock().map_err(|_| storage_failure())?;
                    let migrated = states.get(&(root, input.session_key.clone())).map(|state| {
                        let mut value = crate::metadata::object([
                            ("schemaVersion", JsValue::Number(3.0)),
                            ("sessionId", JsValue::String(state.session_id.clone())),
                            ("bundle", state.bundle.clone()),
                            ("state", state.state.clone()),
                            ("revision", JsValue::Number(state.revision)),
                            (
                                "resumeRequests",
                                JsValue::Object(state.resume_requests.clone()),
                            ),
                            (
                                "updatedAt",
                                raw.as_ref()
                                    .and_then(|v| v.get("updatedAt"))
                                    .filter(|v| v.as_string().is_some())
                                    .cloned()
                                    .unwrap_or("1970-01-01T00:00:00.000Z".into()),
                            ),
                        ]);
                        if let JsValue::Object(object) = &mut value {
                            if let Some(wait) = &state.wait {
                                object.insert("wait".into(), wait.clone());
                            }
                            if let Some(delivery) = raw
                                .as_ref()
                                .and_then(|v| v.get("replyDelivery"))
                                .filter(|v| valid_reply_delivery(v))
                            {
                                object.insert("replyDelivery".into(), delivery.clone());
                            }
                        }
                        value
                    });
                    drop(states);
                    let result = operation(migrated).and_then(|(replacement, result)| {
                        if let Some(replacement) = replacement {
                            persist(path, &replacement).map_err(SopError::from)?;
                        }
                        Ok(result)
                    });
                    Ok(result)
                },
            )
            .map_err(SopError::from)?
        })
        .await
        .map_err(|_| SopError::from(storage_failure()))?
    }
    fn root(&self, project: Option<&JsValue>) -> Result<PathBuf, ApprovalFailure> {
        (self.profiles)(project)
            .map(|root| PathBuf::from(root).join("sessions"))
            .ok_or_else(|| state_error("SOP_MODULE_DISABLED"))
    }
    fn locked<T>(
        &self,
        project: Option<&JsValue>,
        session: &JsString,
        operation: impl FnOnce(&PendingSopStore, Option<JsValue>, &Path) -> Result<T, ApprovalFailure>,
    ) -> Result<T, ApprovalFailure> {
        let root = self.root(project)?;
        let path = state_path(&root, session);
        // Lock the directory inode, which is stable across atomic state-file
        // renames. Linux and macOS support flock on this read-only descriptor.
        // Missing roots have no state and need no mutation merely to read.
        let directory = match File::open(&root) {
            Ok(file) => Some(file),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err(storage_failure()),
        };
        if let Some(directory) = &directory {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match directory.try_lock() {
                    Ok(()) => break,
                    Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Err(_) => return Err(storage_failure()),
                }
            }
        }
        let raw = read_state(&path)?;
        let identity = root.to_string_lossy().into_owned();
        let store = PendingSopStore::new(Arc::new(move |_| Some(identity.clone())));
        if let Some(raw) = &raw {
            store.seed(None, session, raw)?;
        }
        operation(&store, raw, &path)
        // File drop releases the directory lock, including on all failures.
    }
    pub(crate) async fn authority(
        &self,
        input: &StoreStatusInput,
        revision: f64,
    ) -> Result<JsValue, ApprovalFailure> {
        let this = self.clone();
        let input = input.clone();
        tokio::task::spawn_blocking(move || {
            this.locked(
                input.project_key.as_ref(),
                &input.session_key,
                |store, _, _| {
                    let root = (store.profiles)(None).ok_or_else(storage_failure)?;
                    let states = store.states.lock().map_err(|_| storage_failure())?;
                    let state = states
                        .get(&(root, input.session_key.clone()))
                        .ok_or_else(|| state_error("SOP_AUTHORITY_STATE_MISSING"))?;
                    if state.revision != revision {
                        return Err(state_error("SOP_REVISION_CONFLICT"));
                    }
                    if state.wait.is_some()
                        || state.state.get("status") != Some(&JsValue::from("active"))
                    {
                        return Err(state_error("SOP_AUTHORITY_STATE_INACTIVE"));
                    }
                    let id = state
                        .state
                        .get("active_skill_id")
                        .filter(|v| !matches!(v, JsValue::Null | JsValue::Undefined))
                        .or_else(|| state.state.get("selected_skill_id"));
                    let matches = match state.bundle.get("sops") {
                        Some(JsValue::Array(rows)) => rows
                            .iter()
                            .filter(|sop| {
                                sop.get("skill_id")
                                    .filter(|v| !matches!(v, JsValue::Null | JsValue::Undefined))
                                    .or_else(|| sop.get("id"))
                                    == id
                            })
                            .collect::<Vec<_>>(),
                        _ => Vec::new(),
                    };
                    let pin_invalid = || state_error("SOP_AUTHORITY_PIN_INVALID");
                    if matches.len() != 1 {
                        return Err(pin_invalid());
                    }
                    let sop = matches[0];
                    let version = sop
                        .get("version")
                        .filter(|v| v.as_string().is_some())
                        .ok_or_else(pin_invalid)?;
                    let content = sop
                        .get("content")
                        .filter(|v| v.as_object().is_some())
                        .ok_or_else(pin_invalid)?;
                    let node = state
                        .state
                        .get("active_step_id")
                        .filter(|v| v.as_string().is_some())
                        .ok_or_else(pin_invalid)?;
                    Ok(crate::metadata::object([
                        ("sopId", id.cloned().unwrap_or(JsValue::Undefined)),
                        ("sopVersion", version.clone()),
                        ("nodeId", node.clone()),
                        ("content", content.clone()),
                    ]))
                },
            )
        })
        .await
        .map_err(|_| storage_failure())?
    }
}
impl ApprovalPendingStore for DiskSopStore {
    fn status<'a>(
        &'a self,
        input: &'a StoreStatusInput,
    ) -> Pin<Box<dyn Future<Output = Result<Option<JsValue>, ApprovalFailure>> + Send + 'a>> {
        let this = self.clone();
        let input = input.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                this.locked(
                    input.project_key.as_ref(),
                    &input.session_key,
                    |store, _, _| {
                        store.status_locked(&StoreStatusInput {
                            project_key: None,
                            ..input.clone()
                        })
                    },
                )
            })
            .await
            .map_err(|_| storage_failure())?
        })
    }
    fn resume<'a>(
        &'a self,
        input: &'a StoreResumeInput,
    ) -> Pin<Box<dyn Future<Output = Result<JsValue, ApprovalFailure>> + Send + 'a>> {
        let this = self.clone();
        let input = input.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                this.locked(
                    input.project_key.as_ref(),
                    &input.session_key,
                    |store, raw, path| {
                        let result = store.resume_locked(&StoreResumeInput {
                            project_key: None,
                            ..input.clone()
                        })?;
                        if result.get("duplicate") == Some(&JsValue::Bool(true)) {
                            return Ok(result);
                        }
                        let root = (store.profiles)(None).ok_or_else(storage_failure)?;
                        let states = store.states.lock().map_err(|_| storage_failure())?;
                        let state = states
                            .get(&(root, input.session_key.clone()))
                            .ok_or_else(storage_failure)?;
                        let mut next = crate::metadata::object([
                            ("schemaVersion", JsValue::Number(3.0)),
                            ("sessionId", JsValue::String(state.session_id.clone())),
                            ("bundle", state.bundle.clone()),
                            ("state", state.state.clone()),
                            ("revision", JsValue::Number(state.revision)),
                        ]);
                        let JsValue::Object(object) = &mut next else {
                            return Err(storage_failure());
                        };
                        if let Some(wait) = &state.wait {
                            object.insert("wait".into(), wait.clone());
                        }
                        if let Some(delivery) = raw
                            .as_ref()
                            .and_then(|v| v.get("replyDelivery"))
                            .filter(|value| valid_reply_delivery(value))
                        {
                            object.insert("replyDelivery".into(), delivery.clone());
                        }
                        object.insert(
                            "resumeRequests".into(),
                            JsValue::Object(state.resume_requests.clone()),
                        );
                        object.insert(
                            "updatedAt".into(),
                            crate::runtime::context::prompt::iso_timestamp_now()
                                .as_str()
                                .into(),
                        );
                        persist(path, &next)?;
                        Ok(result)
                    },
                )
            })
            .await
            .map_err(|_| storage_failure())?
        })
    }
}
fn valid_reply_delivery(value: &JsValue) -> bool {
    let string =
        |object: &JsValue, key: &str| object.get(key).is_some_and(|v| v.as_string().is_some());
    let Some(result) = value.get("result").filter(|v| v.as_object().is_some()) else {
        return false;
    };
    value.as_object().is_some()
        && string(value, "turnId")
        && string(value, "committedAt")
        && value
            .get("phase")
            .and_then(JsValue::as_string)
            .is_some_and(|v| v.equals_str("pending") || v.equals_str("durable"))
        && string(result, "status")
        && string(result, "replyFragment")
        && result
            .get("slotUpdates")
            .is_some_and(|v| v.as_object().is_some())
        && matches!(result.get("events"), Some(JsValue::Array(_)))
        && value
            .get("durableAt")
            .is_none_or(|v| v.is_undefined() || v.as_string().is_some())
}
fn state_path(root: &Path, session: &JsString) -> PathBuf {
    let name = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(String::from_utf16_lossy(session.units()).as_bytes());
    root.join(format!("{name}.json"))
}
fn storage_failure() -> ApprovalFailure {
    ApprovalFailure {
        status: None,
        code: None,
    }
}
fn read_state(path: &Path) -> Result<Option<JsValue>, ApprovalFailure> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(storage_failure()),
    };
    let mut bytes = Vec::new();
    (&mut file)
        .take(50 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| storage_failure())?;
    if bytes.len() > 50 * 1024 * 1024 {
        return Err(storage_failure());
    }
    pilotdeck_protocol::parse_json(&String::from_utf8_lossy(&bytes))
        .map(Some)
        .map_err(|_| storage_failure())
}
fn persist(path: &Path, value: &JsValue) -> Result<(), ApprovalFailure> {
    let temporary = path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).map_err(|_| storage_failure())?;
        let json = pilotdeck_protocol::stringify_json(value)
            .map_err(|_| storage_failure())?
            .ok_or_else(storage_failure)?;
        file.write_all(json.as_bytes())
            .and_then(|_| file.write_all(b"\n"))
            .and_then(|_| file.sync_all())
            .map_err(|_| storage_failure())?;
        fs::rename(&temporary, path).map_err(|_| storage_failure())?;
        if let Some(parent) = path.parent() {
            File::open(parent)
                .and_then(|file| file.sync_all())
                .map_err(|_| storage_failure())?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests;
