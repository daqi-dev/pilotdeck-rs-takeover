//! Port of baseline `src/adapters/channel/protocol/ChannelRuntimeStatus.ts`:
//! the durable per-channel runtime-status sink under
//! `<pilotHome>/channels/runtime-status.json`. Both the lifecycle core (its
//! four states) and the weixin adapter (its login states) report through this
//! one writer; `updatedAt` is the report wall clock and is masked in goldens.
use pilotdeck_protocol::{JsObject, JsString, JsValue, parse_json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::weixin::stringify_pretty;

/// TS `ChannelRuntimeStatusUpdate` minus the channel key it is reported for.
pub struct ChannelRuntimeStatusUpdate {
    pub state: String,
    pub message: Option<String>,
    pub account_id: Option<JsString>,
    pub error: Option<String>,
    pub qr_url: Option<JsString>,
}

pub type ChannelRuntimeStatusReporter =
    Arc<dyn Fn(&JsString, &ChannelRuntimeStatusUpdate) + Send + Sync>;

/// `new Date(0).toISOString()` — the empty snapshot's `updatedAt`.
const EPOCH_ISO: &str = "1970-01-01T00:00:00.000Z";

pub fn channel_runtime_status_path(pilot_home: &Path) -> PathBuf {
    pilot_home.join("channels").join("runtime-status.json")
}

/// `readChannelRuntimeStatusSnapshot`: an absent or unparsable file yields
/// the epoch empty snapshot instead of failing the report.
pub(crate) fn read_snapshot(pilot_home: &Path) -> JsObject {
    let empty = || {
        let mut snapshot = JsObject::new();
        snapshot.insert("updatedAt".into(), EPOCH_ISO.into());
        snapshot.insert("channels".into(), JsValue::Object(JsObject::new()));
        snapshot
    };
    let Ok(raw) = std::fs::read_to_string(channel_runtime_status_path(pilot_home)) else {
        return empty();
    };
    let Ok(parsed) = parse_json(&raw) else {
        return empty();
    };
    let mut snapshot = empty();
    if let Some(updated_at) = parsed.get("updatedAt").and_then(JsValue::as_string) {
        snapshot.insert("updatedAt".into(), JsValue::String(updated_at.clone()));
    }
    if let Some(channels) = parsed.get("channels").and_then(JsValue::as_object) {
        snapshot.insert("channels".into(), JsValue::Object(channels.clone()));
    }
    snapshot
}

/// `createChannelRuntimeStatusReporter` — synchronous fs, like the TS module.
/// A write failure surfaces as the caller's warn (the TS safe reporter's
/// catch), not a panic.
pub fn create_channel_runtime_status_reporter(pilot_home: PathBuf) -> ChannelRuntimeStatusReporter {
    // Clones of this reporter share one transaction lock. Independent
    // reporters and processes do not share it.
    let lock = Mutex::new(());
    Arc::new(move |channel_key, update| {
        let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = now_iso();
        let mut channels = match read_snapshot(&pilot_home)
            .get("channels")
            .and_then(JsValue::as_object)
        {
            Some(channels) => channels.clone(),
            None => JsObject::new(),
        };
        // Entry order: channelKey, state, updatedAt, then the optional
        // fields — each only when its value is truthy (a non-empty string).
        let mut entry = JsObject::new();
        entry.insert("channelKey".into(), JsValue::String(channel_key.clone()));
        entry.insert(
            "state".into(),
            JsValue::String(update.state.as_str().into()),
        );
        entry.insert("updatedAt".into(), JsValue::String(now.clone()));
        if let Some(message) = update.message.clone().filter(|text| !text.is_empty()) {
            entry.insert("message".into(), JsValue::String(JsString::from(message)));
        }
        if let Some(account_id) = &update.account_id
            && !account_id.is_empty()
        {
            entry.insert("accountId".into(), JsValue::String(account_id.clone()));
        }
        if let Some(error) = update.error.clone().filter(|text| !text.is_empty()) {
            entry.insert("error".into(), JsValue::String(JsString::from(error)));
        }
        if let Some(qr_url) = &update.qr_url
            && !qr_url.is_empty()
        {
            entry.insert("qrUrl".into(), JsValue::String(qr_url.clone()));
        }
        channels.insert(channel_key.clone(), JsValue::Object(entry));
        let mut next = JsObject::new();
        next.insert("updatedAt".into(), JsValue::String(now));
        next.insert("channels".into(), JsValue::Object(channels));
        let status_path = channel_runtime_status_path(&pilot_home);
        let written = (|| -> std::io::Result<()> {
            if let Some(parent) = status_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Resolve terminal symlinks without replacing them, including a
            // dangling final target that the original writer could create.
            let path = resolve_status_target(&status_path)?;
            // Preserve the old writer's permission check and existing mode.
            // Opening without truncate leaves the old bytes intact on failure.
            let existing_permissions = match std::fs::metadata(&path) {
                Ok(metadata) => {
                    std::fs::OpenOptions::new().write(true).open(&path)?;
                    Some(metadata.permissions())
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
            let temporary =
                path.with_file_name(format!(".runtime-status.{}.tmp", uuid::Uuid::new_v4()));
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            let result = (|| -> std::io::Result<()> {
                use std::io::Write;
                if let Some(permissions) = existing_permissions {
                    file.set_permissions(permissions)?;
                }
                file.write_all(
                    format!("{}\n", stringify_pretty(&JsValue::Object(next))).as_bytes(),
                )?;
                drop(file);
                std::fs::rename(&temporary, &path)
            })();
            if result.is_err() {
                let _ = std::fs::remove_file(&temporary);
            }
            result
        })();
        if let Err(error) = written {
            crate::native_process::channels::console_log_line(
                &mut std::io::stderr(),
                &format!(
                    "[adapters] failed to write runtime status for {}: {error}",
                    String::from_utf16_lossy(channel_key.units())
                ),
            );
        }
    })
}

fn resolve_status_target(path: &Path) -> std::io::Result<PathBuf> {
    let mut target = path.to_path_buf();
    for _ in 0..40 {
        match std::fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let link = std::fs::read_link(&target)?;
                target = if link.is_absolute() {
                    link
                } else {
                    target.parent().unwrap_or_else(|| Path::new(".")).join(link)
                };
            }
            Ok(_) => return Ok(target),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(target),
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::other(
        "too many symbolic links resolving runtime status",
    ))
}

fn now_iso() -> JsString {
    use pilotdeck_transport::{server::ExecutionClock, system_clock::SystemExecutionClock};
    SystemExecutionClock.now_iso()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Barrier,
        atomic::{AtomicBool, Ordering},
    };

    #[test]
    fn cloned_reporters_preserve_updates_and_publish_complete_json() {
        let home = std::env::temp_dir().join(format!("pd-status-test-{}", uuid::Uuid::new_v4()));
        let reporter = create_channel_runtime_status_reporter(home.clone());
        let done = Arc::new(AtomicBool::new(false));
        let reader_home = home.clone();
        let reader_done = done.clone();
        let reader = std::thread::spawn(move || {
            while !reader_done.load(Ordering::Relaxed) {
                if let Ok(bytes) =
                    std::fs::read_to_string(channel_runtime_status_path(&reader_home))
                {
                    assert!(parse_json(&bytes).is_ok(), "partial status: {bytes}");
                }
            }
        });
        let barrier = Arc::new(Barrier::new(32));
        let threads: Vec<_> = (0..32)
            .map(|i| {
                let report = reporter.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..5 {
                        report(
                            &format!("channel-{i}").as_str().into(),
                            &ChannelRuntimeStatusUpdate {
                                state: "ready".into(),
                                message: None,
                                account_id: None,
                                error: None,
                                qr_url: None,
                            },
                        );
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        done.store(true, Ordering::Relaxed);
        reader.join().unwrap();
        let snapshot = read_snapshot(&home);
        let channels = snapshot.get("channels").unwrap().as_object().unwrap();
        for i in 0..32 {
            assert!(
                channels.get(format!("channel-{i}").as_str()).is_some(),
                "missing channel {i}"
            );
        }
        std::fs::remove_dir_all(home).unwrap();
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;
    #[test]
    fn directory_target_preflight_failure_preserves_target() {
        let home = std::env::temp_dir().join(format!("pd-status-failure-{}", uuid::Uuid::new_v4()));
        let target = channel_runtime_status_path(&home);
        std::fs::create_dir_all(&target).unwrap();
        let report = create_channel_runtime_status_reporter(home.clone());
        report(
            &"channel".into(),
            &ChannelRuntimeStatusUpdate {
                state: "ready".into(),
                message: None,
                account_id: None,
                error: None,
                qr_url: None,
            },
        );
        assert!(target.is_dir());
        assert_eq!(
            std::fs::read_dir(target.parent().unwrap()).unwrap().count(),
            1
        );
        std::fs::remove_dir_all(home).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn existing_mode_and_read_only_failure_are_preserved() {
        use std::os::unix::fs::PermissionsExt;
        let home = std::env::temp_dir().join(format!("pd-status-mode-{}", uuid::Uuid::new_v4()));
        let path = channel_runtime_status_path(&home);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{}\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let report = create_channel_runtime_status_reporter(home.clone());
        let update = ChannelRuntimeStatusUpdate {
            state: "ready".into(),
            message: None,
            account_id: None,
            error: None,
            qr_url: None,
        };
        report(&"channel".into(), &update);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
        let before = std::fs::read(&path).unwrap();
        let can_write = std::fs::OpenOptions::new().write(true).open(&path).is_ok();
        report(&"other".into(), &update);
        if can_write {
            let snapshot = read_snapshot(&home);
            assert!(
                snapshot
                    .get("channels")
                    .unwrap()
                    .as_object()
                    .unwrap()
                    .get("other")
                    .is_some()
            );
        } else {
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o400
        );
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
        std::fs::remove_dir_all(home).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn existing_symlink_is_preserved_and_target_updated() {
        use std::os::unix::fs::symlink;
        let home = std::env::temp_dir().join(format!("pd-status-link-{}", uuid::Uuid::new_v4()));
        let path = channel_runtime_status_path(&home);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let target = path.parent().unwrap().join("target.json");
        std::fs::write(&target, "{}\n").unwrap();
        symlink("target.json", &path).unwrap();
        let report = create_channel_runtime_status_reporter(home.clone());
        report(
            &"channel".into(),
            &ChannelRuntimeStatusUpdate {
                state: "ready".into(),
                message: None,
                account_id: None,
                error: None,
                qr_url: None,
            },
        );
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let parsed =
            pilotdeck_protocol::parse_json(&std::fs::read_to_string(target).unwrap()).unwrap();
        assert!(
            parsed
                .get("channels")
                .unwrap()
                .as_object()
                .unwrap()
                .get("channel")
                .is_some()
        );
        std::fs::remove_dir_all(home).unwrap();
    }
}

#[cfg(all(test, unix))]
mod symlink_tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn dangling_relative_link_creates_target_and_self_loop_fails() {
        let home = std::env::temp_dir().join(format!("pd-status-links-{}", uuid::Uuid::new_v4()));
        let path = channel_runtime_status_path(&home);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        symlink("target.json", &path).unwrap();
        let report = create_channel_runtime_status_reporter(home.clone());
        let update = ChannelRuntimeStatusUpdate {
            state: "ready".into(),
            message: None,
            account_id: None,
            error: None,
            qr_url: None,
        };
        report(&"channel".into(), &update);
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let target = path.parent().unwrap().join("target.json");
        let contents = std::fs::read_to_string(&target).unwrap();
        assert!(
            parse_json(&contents)
                .unwrap()
                .get("channels")
                .unwrap()
                .as_object()
                .unwrap()
                .get("channel")
                .is_some()
        );
        std::fs::remove_file(&path).unwrap();
        symlink("runtime-status.json", &path).unwrap();
        assert!(resolve_status_target(&path).is_err());
        report(&"other".into(), &update);
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), contents);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            2
        );
        std::fs::remove_dir_all(home).unwrap();
    }
}
