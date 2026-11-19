//! Shim-owned persistent state under `$AGENTCPM_PD_HOME/compat/`.
//!
//! Everything the shim persists lives here (never in `~/.pilotdeck`):
//!
//! - `compat/config.json` — explicit shim configuration ([`crate::config`]);
//! - `compat/sessions.json` — the legacy→new session mapping table used by
//!   `resume` and every entry that receives a legacy session id
//!   (`sdk-resume` case: the old id must resolve through this table, unknown
//!   ids fail explicitly instead of being guessed);
//! - `compat/runs/<requestKey>.json` — one write-ahead record per submitted
//!   run: the request key is persisted BEFORE `run.submit` so a lost
//!   response can be retried with the same key, and the SIGINT guardian can
//!   recover the run id and submit `run.cancel` after the shim dies.
//!
//! All writes are atomic (temp file + rename) so a crash never leaves a
//! half-written JSON document behind. Atomicity alone does not make the
//! sessions table safe against two shims sharing one
//! `AGENTCPM_PD_HOME`: `sessions.json` is a cross-process read-modify-write,
//! so every [`SessionMap::record`] also holds `compat/sessions.lock`
//! (see [`SessionLock`]) and re-loads the table inside the lock.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

/// Directory holding the run records.
pub const RUNS_DIR: &str = "runs";
/// Session mapping table file name.
pub const SESSIONS_FILE: &str = "sessions.json";
/// Cross-process write lock for [`SESSIONS_FILE`] (see [`SessionLock`]).
pub const SESSIONS_LOCK_FILE: &str = "sessions.lock";

/// Current state schema version.
pub const STATE_SCHEMA_VERSION: u32 = 1;

/// Run record lifecycle states.
pub const RUN_SUBMITTING: &str = "submitting";
pub const RUN_RUNNING: &str = "running";
pub const RUN_SETTLED: &str = "settled";
pub const RUN_CANCELLED: &str = "cancelled";

/// One legacy→new session mapping row.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMapping {
    /// Session id as seen by the user through the shim (what `run --json`
    /// printed and what `resume` accepts).
    pub legacy_session_id: String,
    /// The new agentcpm-pd session id. For shim-created sessions this is the
    /// id the shim itself generated for `session.create` (an identity row);
    /// the t020 migration can add rows mapping true legacy ids onto new
    /// sessions.
    pub session_id: String,
    /// Project the session was created under.
    pub project_id: String,
    /// Unix seconds when the row was recorded.
    pub created_at: String,
}

/// The legacy→new session mapping table document.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMap {
    pub schema_version: u32,
    pub sessions: Vec<SessionMapping>,
}

impl Default for SessionMap {
    fn default() -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            sessions: Vec::new(),
        }
    }
}

impl SessionMap {
    /// Loads the table; a missing file is an empty table (fresh install).
    /// A present-but-invalid document is an error, never a silent default.
    pub fn load(home: &Path) -> Result<Self, String> {
        let path = sessions_path(home);
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let map: SessionMap =
            serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
        if map.schema_version != STATE_SCHEMA_VERSION {
            return Err(format!(
                "{}: schema version {} is not supported (expected {STATE_SCHEMA_VERSION})",
                path.display(),
                map.schema_version
            ));
        }
        Ok(map)
    }

    /// Adds a row and saves atomically (appends; duplicate legacy ids are
    /// updated in place). The table is shared by every shim process under
    /// one home, so the write is a cross-process read-modify-write: the
    /// `compat/sessions.lock` is held across a fresh [`SessionMap::load`],
    /// the merge and the save — rows a concurrent shim recorded after this
    /// copy was loaded survive instead of being overwritten. The
    /// in-memory copy is refreshed to the merged table.
    pub fn record(&mut self, home: &Path, mapping: SessionMapping) -> Result<(), String> {
        let _lock = SessionLock::acquire(home)?;
        let mut fresh = Self::load(home)?;
        match fresh
            .sessions
            .iter_mut()
            .find(|m| m.legacy_session_id == mapping.legacy_session_id)
        {
            Some(existing) => *existing = mapping.clone(),
            None => fresh.sessions.push(mapping),
        }
        fresh.save(home)?;
        self.sessions = fresh.sessions;
        Ok(())
    }

    fn save(&self, home: &Path) -> Result<(), String> {
        let dir = home.join(crate::config::COMPAT_DIR);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        atomic_write_json(
            &sessions_path(home),
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )
    }

    /// Resolves a legacy session id to its mapping row.
    pub fn find(&self, legacy_session_id: &str) -> Option<&SessionMapping> {
        self.sessions
            .iter()
            .find(|m| m.legacy_session_id == legacy_session_id)
    }

    /// Reverse lookup: the user-facing legacy id for a new session id.
    pub fn legacy_for(&self, session_id: &str) -> Option<&str> {
        self.sessions
            .iter()
            .find(|m| m.session_id == session_id)
            .map(|m| m.legacy_session_id.as_str())
    }
}

/// Path of the session mapping table.
pub fn sessions_path(home: &Path) -> PathBuf {
    home.join(crate::config::COMPAT_DIR).join(SESSIONS_FILE)
}

// ---------------------------------------------------------------------------
// Cross-process sessions lock
// ---------------------------------------------------------------------------

/// How long acquiring [`SessionLock`] waits before the bounded stale-lock
/// recovery (or the honest error) kicks in.
const LOCK_MAX_WAIT: Duration = Duration::from_secs(5);
/// A lock file older than this belongs to a crashed holder (the locked
/// section is a load-merge-save of a small JSON document — milliseconds)
/// and is stolen.
const LOCK_STALE_AFTER: Duration = Duration::from_secs(10);
/// Poll interval while waiting for the lock.
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Bounded number of stale-lock steals per acquisition.
const MAX_LOCK_STEALS: u32 = 3;

/// Cross-process write lock for `compat/sessions.json`, taken as
/// `compat/sessions.lock` with `O_CREAT|O_EXCL` (`create_new` — the only
/// mutual-exclusion primitive reachable without `unsafe`, so no flock).
///
/// Two shims sharing one `AGENTCPM_PD_HOME` (two terminals running
/// `pilotdeck run`, a run and a `sessions list` write, the t020 migration)
/// both read-modify-write the same table; `atomic_write_json` only keeps
/// each file untorn, it does not merge concurrent updates — without the
/// lock one process's rows are silently lost. The lock is held only for
/// the load-merge-save section; a holder that died mid-section leaves the
/// file behind, so a lock older than the staleness bound is removed
/// (bounded number of times) and every remaining failure is an honest
/// error string for the caller to report — never a silent overwrite.
struct SessionLock {
    path: PathBuf,
}

impl SessionLock {
    /// Acquires the lock with the production wait/staleness bounds.
    fn acquire(home: &Path) -> Result<Self, String> {
        Self::acquire_with(home, LOCK_MAX_WAIT, LOCK_STALE_AFTER)
    }

    /// [`SessionLock::acquire`] with injectable bounds (tests use fast
    /// values). Waits at most `max_wait` for another holder; past the
    /// wait bound a lock older than `stale_after` is stolen, at most
    /// [`MAX_LOCK_STEALS`] times, and anything else is an error.
    fn acquire_with(
        home: &Path,
        max_wait: Duration,
        stale_after: Duration,
    ) -> Result<Self, String> {
        let dir = home.join(crate::config::COMPAT_DIR);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = dir.join(SESSIONS_LOCK_FILE);
        let deadline = std::time::Instant::now() + max_wait;
        let mut steals = 0u32;
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    // Holder pid, for humans debugging a wedged lock.
                    use std::io::Write as _;
                    let _ = write!(file, "{}", std::process::id());
                    return Ok(Self { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if std::time::Instant::now() < deadline {
                        std::thread::sleep(LOCK_POLL_INTERVAL);
                        continue;
                    }
                    if steals >= MAX_LOCK_STEALS {
                        return Err(format!(
                            "sessions lock held by another process: {}",
                            path.display()
                        ));
                    }
                    match lock_age(&path) {
                        Ok(age) if age < stale_after => {
                            return Err(format!(
                                "sessions lock held by another process: {}",
                                path.display()
                            ));
                        }
                        // A live holder's lock is too young to steal; an
                        // older one is a crashed holder. A vanished lock
                        // (the holder dropped it between our failed
                        // create and the stat) is simply retried.
                        Ok(_) | Err(_) => {}
                    }
                    // Crashed or vanished holder: remove (if still
                    // present) and retry, bounded by the steal counter
                    // (another process may win the race; that is a normal
                    // wait, not an error).
                    let _ = std::fs::remove_file(&path);
                    steals += 1;
                }
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
        }
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Age of a lock file (`ZERO` when the clock disagrees with the metadata).
fn lock_age(path: &Path) -> Result<Duration, String> {
    let modified = std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(modified.elapsed().unwrap_or(Duration::ZERO))
}

/// Write-ahead record for one submitted run (see module docs).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRecord {
    pub schema_version: u32,
    pub request_key: String,
    pub session_id: String,
    pub project_id: String,
    pub prompt: String,
    /// `submitting` | `running` | `settled` | `cancelled`.
    pub state: String,
    pub run_id: Option<String>,
    /// Final run state observed (`succeeded`, `failed`, ...), when known.
    pub terminal_state: Option<String>,
    pub updated_at: String,
}

impl RunRecord {
    /// Creates a fresh pre-submit record.
    pub fn new(
        request_key: String,
        session_id: String,
        project_id: String,
        prompt: String,
    ) -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            request_key,
            session_id,
            project_id,
            prompt,
            state: RUN_SUBMITTING.to_owned(),
            run_id: None,
            terminal_state: None,
            updated_at: now_string(),
        }
    }

    /// Path of the record file under the shim home.
    pub fn path(&self, home: &Path) -> PathBuf {
        run_record_path(home, &self.request_key)
    }

    /// Persists the record atomically.
    pub fn save(&self, home: &Path) -> Result<(), String> {
        let dir = home.join(crate::config::COMPAT_DIR).join(RUNS_DIR);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        atomic_write_json(
            &self.path(home),
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )
    }

    /// Loads the record for a request key (`None` when absent).
    pub fn load(home: &Path, request_key: &str) -> Result<Option<Self>, String> {
        Self::load_by_path(&run_record_path(home, request_key))
    }

    /// Loads the record from its file path (`None` when absent); used by
    /// the SIGINT guardian, which receives the path directly.
    pub fn load_by_path(path: &Path) -> Result<Option<Self>, String> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        serde_json::from_str(&raw)
            .map(Some)
            .map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// Path of one run record file.
pub fn run_record_path(home: &Path, request_key: &str) -> PathBuf {
    home.join(crate::config::COMPAT_DIR)
        .join(RUNS_DIR)
        .join(format!("{request_key}.json"))
}

/// Generates a shim-unique id (`<prefix>-<32 hex chars>`). Randomness comes
/// from the OS entropy source when available; the fallback mixes time, pid
/// and a process-wide counter so ids stay unique within and across runs.
pub fn generate_id(prefix: &str) -> String {
    let mut entropy = [0u8; 16];
    let filled = read_os_entropy(&mut entropy);
    if !filled {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTimeNanos::now().0 as u64;
        let pid = std::process::id() as u64;
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut mix = nanos ^ (pid << 32) ^ count.rotate_left(17);
        for slot in entropy.iter_mut() {
            // xorshift64*
            mix ^= mix >> 12;
            mix ^= mix << 25;
            mix ^= mix >> 27;
            *slot = (mix.wrapping_mul(0x2545F4914F6CDD1D) & 0xff) as u8;
        }
    }
    let hex: String = entropy.iter().map(|b| format!("{b:02x}")).collect();
    format!("{prefix}-{hex}")
}

/// Reads exactly `out.len()` bytes from the OS entropy source; `false` when
/// unavailable (non-Unix or closed device). `File::read_exact` is used
/// because the device never signals EOF.
fn read_os_entropy(out: &mut [u8]) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open("/dev/urandom") else {
        return false;
    };
    file.read_exact(out).is_ok()
}

/// Generates a request key valid for the operation contract id pattern
/// (`^[A-Za-z0-9][A-Za-z0-9._~-]*$`).
pub fn generate_request_key() -> String {
    generate_id("req")
}

struct SystemTimeNanos(u128);

impl SystemTimeNanos {
    fn now() -> Self {
        Self(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        )
    }
}

/// Current time as a unix-seconds string (no clock crate in the dependency
/// set; monotonicity within the shim is not required).
fn now_string() -> String {
    (SystemTimeNanos::now().0 / 1_000_000_000).to_string()
}

/// Writes `value` to `path` atomically: serialize, write to a sibling temp
/// file, `rename` over the target. A crash mid-write leaves the old document
/// (or nothing) — never a torn file.
pub fn atomic_write_json(path: &Path, value: &Value) -> Result<(), String> {
    let raw = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("state"),
        std::process::id()
    ));
    std::fs::write(&tmp, raw).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use serde_json::json;

    #[test]
    fn generated_ids_are_prefixed_and_unique() {
        let a = generate_id("shim-s");
        let b = generate_id("shim-s");
        assert!(a.starts_with("shim-s-"), "{a}");
        assert_ne!(a, b);
        // Contract id pattern: alnum start, then [A-Za-z0-9._~-].
        let body = &a[7..];
        assert!(!body.is_empty());
        assert!(body.chars().all(|c| c.is_ascii_hexdigit()));
        let key = generate_request_key();
        assert!(key.starts_with("req-"), "{key}");
    }

    #[test]
    fn session_map_round_trips_and_updates() {
        let home = TempDir::new("state-sessions");
        let mut map = SessionMap::load(home.path()).unwrap();
        assert_eq!(map, SessionMap::default());

        let row = SessionMapping {
            legacy_session_id: "shim-s-abc".to_owned(),
            session_id: "shim-s-abc".to_owned(),
            project_id: "shim-p-1".to_owned(),
            created_at: "123".to_owned(),
        };
        map.record(home.path(), row.clone()).unwrap();
        let reloaded = SessionMap::load(home.path()).unwrap();
        assert_eq!(reloaded.find("shim-s-abc"), Some(&row));
        assert_eq!(reloaded.legacy_for("shim-s-abc"), Some("shim-s-abc"));
        assert!(reloaded.find("nope").is_none());

        // Updating an existing legacy id replaces the row.
        let mut updated = row.clone();
        updated.session_id = "new-session".to_owned();
        map.record(home.path(), updated.clone()).unwrap();
        let reloaded = SessionMap::load(home.path()).unwrap();
        assert_eq!(reloaded.sessions.len(), 1);
        assert_eq!(
            reloaded.find("shim-s-abc").unwrap().session_id,
            "new-session"
        );
        assert_eq!(reloaded.legacy_for("new-session"), Some("shim-s-abc"));
    }

    fn row(legacy: &str, session: &str) -> SessionMapping {
        SessionMapping {
            legacy_session_id: legacy.to_owned(),
            session_id: session.to_owned(),
            project_id: "shim-p-1".to_owned(),
            created_at: "1".to_owned(),
        }
    }

    #[test]
    fn concurrent_records_keep_every_row() {
        // Two shims under one home recording simultaneously: without the
        // cross-process lock one side's rows would be overwritten by the
        // other's stale in-memory table (lost update).
        let home = TempDir::new("state-sessions-concurrent");
        let home_ref = &home;
        let all_ok = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|t| {
                    scope.spawn(move || {
                        (0..10).all(|i| {
                            let mut map = SessionMap::load(home_ref.path()).unwrap();
                            map.record(
                                home_ref.path(),
                                row(&format!("legacy-{t}-{i}"), &format!("new-{t}-{i}")),
                            )
                            .is_ok()
                        })
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(all_ok.iter().all(|&ok| ok));
        let map = SessionMap::load(home.path()).unwrap();
        let mut ids: Vec<&str> = map
            .sessions
            .iter()
            .map(|m| m.legacy_session_id.as_str())
            .collect();
        ids.sort();
        assert_eq!(ids.len(), 40, "no row may be lost: {ids:?}");
        assert_eq!(ids[0], "legacy-0-0");
        assert_eq!(ids[39], "legacy-3-9");
    }

    #[test]
    fn record_merges_rows_written_after_this_copy_was_loaded() {
        // The exact interleaving from the review: A loads the (empty)
        // table; B records its row; A records its row. A's stale copy
        // must not clobber B's row.
        let home = TempDir::new("state-sessions-merge");
        let mut stale = SessionMap::load(home.path()).unwrap();
        let mut other = SessionMap::default();
        other.record(home.path(), row("legacy-b", "new-b")).unwrap();
        assert!(stale.find("legacy-b").is_none(), "sanity: stale copy");

        stale.record(home.path(), row("legacy-a", "new-a")).unwrap();
        let merged = SessionMap::load(home.path()).unwrap();
        assert_eq!(merged.sessions.len(), 2, "both rows must survive");
        assert!(merged.find("legacy-a").is_some());
        assert!(merged.find("legacy-b").is_some());
        // The writer's in-memory copy follows the merged table.
        assert_eq!(
            stale.find("legacy-b").map(|m| m.session_id.as_str()),
            Some("new-b")
        );
    }

    #[test]
    fn lock_busy_errors_and_crashed_lock_is_recovered() {
        let home = TempDir::new("state-sessions-lock");
        let dir = home.path().join(crate::config::COMPAT_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join(SESSIONS_LOCK_FILE);
        std::fs::write(&lock, "12345").unwrap();

        // A fresh lock (a live holder): bounded wait, then an honest
        // error — never a silent overwrite.
        assert!(
            SessionLock::acquire_with(
                home.path(),
                Duration::from_millis(60),
                Duration::from_secs(10)
            )
            .is_err()
        );

        // A lock older than the staleness bound (a crashed holder) is
        // stolen; `stale_after` of zero makes any lock stale.
        let guard =
            SessionLock::acquire_with(home.path(), Duration::from_millis(60), Duration::ZERO)
                .expect("crashed lock is recovered");
        assert!(lock.exists(), "held lock file is present");
        drop(guard);
        assert!(!lock.exists(), "guard removes the lock on drop");

        // The public record path works again afterwards.
        let mut map = SessionMap::default();
        map.record(home.path(), row("legacy-x", "new-x")).unwrap();
        assert_eq!(
            SessionMap::load(home.path()).unwrap().find("legacy-x"),
            Some(&row("legacy-x", "new-x"))
        );
    }

    #[test]
    fn session_map_rejects_invalid_documents_loudly() {
        let home = TempDir::new("state-sessions-bad");
        std::fs::create_dir_all(home.path().join(crate::config::COMPAT_DIR)).unwrap();
        std::fs::write(sessions_path(home.path()), "not json").unwrap();
        assert!(SessionMap::load(home.path()).is_err());

        let future = serde_json::to_string(&SessionMap {
            schema_version: 99,
            sessions: vec![],
        })
        .unwrap();
        std::fs::write(sessions_path(home.path()), future).unwrap();
        assert!(SessionMap::load(home.path()).is_err());
    }

    #[test]
    fn run_record_round_trips_under_runs_dir() {
        let home = TempDir::new("state-runs");
        let mut rec = RunRecord::new(
            "req-1".to_owned(),
            "shim-s-1".to_owned(),
            "shim-p-1".to_owned(),
            "hello".to_owned(),
        );
        assert_eq!(rec.state, RUN_SUBMITTING);
        assert_eq!(rec.run_id, None);
        rec.save(home.path()).unwrap();

        let path = run_record_path(home.path(), "req-1");
        assert!(path.ends_with("compat/runs/req-1.json"), "{path:?}");
        let loaded = RunRecord::load(home.path(), "req-1").unwrap().unwrap();
        assert_eq!(loaded, rec);
        assert!(
            RunRecord::load(home.path(), "req-missing")
                .unwrap()
                .is_none()
        );

        rec.state = RUN_RUNNING.to_owned();
        rec.run_id = Some("run-9".to_owned());
        rec.save(home.path()).unwrap();
        let loaded = RunRecord::load(home.path(), "req-1").unwrap().unwrap();
        assert_eq!(loaded.run_id.as_deref(), Some("run-9"));
    }

    #[test]
    fn atomic_write_replaces_content_and_leaves_no_temp() {
        let home = TempDir::new("state-atomic");
        let dir = home.path().join("compat");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("doc.json");
        atomic_write_json(&path, &json!({"a": 1})).unwrap();
        atomic_write_json(&path, &json!({"a": 2})).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.trim_end(), "{\n  \"a\": 2\n}");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        assert_eq!(leftovers.len(), 1, "temp files must be renamed away");
    }
}
