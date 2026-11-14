//! Extension directory watching, ported from the baseline's
//! `src/cli/ExtensionWatchManager.ts`.
//!
//! * One global scope — `<pilotHome>/mcp.json`, `<pilotHome>/plugins`,
//!   `<pilotHome>/skills` and the bundled skills root when there is one — and
//!   one scope per activated project — `<root>/.pilotdeck/mcp.json`,
//!   `<root>/.pilotdeck/plugins`, `<root>/.pilotdeck/skills`
//!   (ExtensionWatchManager.ts:96-107). A project scope is added the first
//!   time the registry resolves that project (`onProjectActivated`,
//!   ProjectRuntimeRegistry.ts:1541-1544).
//! * A change anywhere under a watched path schedules that path for its
//!   scope; 250 ms after the scope's last change the scope reports the
//!   sorted set of watched paths that changed (:148-163).
//! * The baseline uses recursive `fs.watch` on the path, or on its nearest
//!   existing ancestor filtered back to the path (:109-131, :170-188). Like
//!   the config store's watcher (`config/store.rs`), this crate has no native
//!   watcher dependency, so each watched path's tree is polled: a fingerprint
//!   of every entry under it (type, length, modification time; a watched
//!   path that is itself a link is followed). What lies behind a symbolic
//!   link inside the tree depends on the platform, as the baseline's
//!   recursive `fs.watch` does: on macOS (FSEvents) a change behind it is
//!   not reported; on Linux Node 22 has no native recursive watch and
//!   watches every entry it finds with its own inotify watch
//!   (`internal/fs/recursive_watch.js`, `#watchFolder` / `#watchFile`),
//!   which for a link follows it and covers the target and its direct
//!   children, but does not walk further below it. `link_target_sample`
//!   gives the Linux side that one level.
//!   A change is therefore seen up to one poll interval late, and a change
//!   that leaves every fingerprinted field as it was is not seen. Every
//!   sample takes every entry's metadata, but a directory that has not
//!   changed since it was listed (and not within the last couple of
//!   seconds) is not listed again: its previous names are walked instead
//!   (`DirListings`), which is most of what an idle poll used to cost.
//! * The polling runs on its own thread, never on an async worker, and no
//!   lock is held while it touches the file system: registering a scope
//!   (`watch_project`, called on every project resolve under the registry's
//!   lock) only records the paths and wakes the thread, which samples a new
//!   scope's starting point before anything else. A pass over the trees is
//!   followed by a pause of at least `IDLE_DUTY - 1` times its own length:
//!   a huge tree (a `node_modules` under `plugins/`) is polled less often
//!   instead of keeping a core busy, at the price of its changes being seen
//!   later.

use std::collections::{BTreeSet, HashMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

/// `debounceMs ?? 250`.
pub const DEBOUNCE: Duration = Duration::from_millis(250);
/// How often every watched tree is sampled while sampling is cheap.
pub const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// The walk takes at most this fraction (1 / n) of one core.
const IDLE_DUTY: u32 = 20;

/// `ExtensionWatchScope`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtensionWatchScope {
    Global,
    Project(PathBuf),
}

/// `ExtensionWatchEvent`: the scope and its changed watched paths, sorted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionWatchEvent {
    pub scope: ExtensionWatchScope,
    pub changed_paths: Vec<String>,
}

pub type OnExtensionChange = Arc<dyn Fn(ExtensionWatchEvent) + Send + Sync>;

pub struct ExtensionWatchOptions {
    pub pilot_home: PathBuf,
    pub builtin_skills_root: Option<PathBuf>,
    pub on_change: OnExtensionChange,
}

struct WatchedPath {
    path: PathBuf,
    /// `None` until the watch thread has sampled the path once.
    fingerprint: Option<u64>,
    /// Whether the path itself is followed when it is a symbolic link:
    /// decided at the first sample (see `watch_thread`).
    follow_root: bool,
}

struct ScopeRecord {
    scope: ExtensionWatchScope,
    paths: Vec<WatchedPath>,
    pending: BTreeSet<Vec<u16>>,
    deadline: Option<Instant>,
}

#[derive(Default)]
struct State {
    /// Only ever appended to, so a `(scope, path)` index pair taken under
    /// the lock stays valid after it is released.
    scopes: Vec<ScopeRecord>,
    /// A scope was added since the thread last looked.
    added: bool,
}

struct Shared {
    options: ExtensionWatchOptions,
    state: Mutex<State>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// `ExtensionWatchManager`, started. Dropping every handle stops the watch
/// thread at its next wake-up.
#[derive(Clone)]
pub struct ExtensionWatchManager {
    shared: Arc<Shared>,
}

impl ExtensionWatchManager {
    /// `new ExtensionWatchManager(options).start()`: the global scope is
    /// watched from here on.
    pub fn start(options: ExtensionWatchOptions) -> Self {
        let manager = Self {
            shared: Arc::new(Shared {
                options,
                state: Mutex::new(State::default()),
                wake: Condvar::new(),
            }),
        };
        manager.ensure_scope(ExtensionWatchScope::Global);
        let shared = Arc::downgrade(&manager.shared);
        std::thread::Builder::new()
            .name("extension-watch".into())
            .spawn(move || watch_thread(shared))
            .expect("spawn the extension watch thread");
        manager
    }

    /// `watchProject(projectRoot)`; the root is already resolved. No file
    /// system access here: the watch thread takes the first sample.
    pub fn watch_project(&self, project_root: &Path) {
        self.ensure_scope(ExtensionWatchScope::Project(project_root.to_path_buf()));
    }

    /// Test-only: wait until the thread has taken every registered path's
    /// starting point, which `watch_project` itself no longer waits for.
    #[cfg(test)]
    async fn sampled(&self) {
        while self
            .shared
            .lock()
            .scopes
            .iter()
            .any(|record| record.paths.iter().any(|path| path.fingerprint.is_none()))
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn ensure_scope(&self, scope: ExtensionWatchScope) {
        let mut state = self.shared.lock();
        if state.scopes.iter().any(|record| record.scope == scope) {
            return;
        }
        let paths = watched_paths(&self.shared.options, &scope)
            .into_iter()
            .map(|path| WatchedPath {
                path,
                fingerprint: None,
                follow_root: false,
            })
            .collect();
        state.scopes.push(ScopeRecord {
            scope,
            paths,
            pending: BTreeSet::new(),
            deadline: None,
        });
        state.added = true;
        drop(state);
        self.shared.wake.notify_one();
    }
}

/// `getWatchedPaths(scope)` (ExtensionWatchManager.ts:96-107).
fn watched_paths(options: &ExtensionWatchOptions, scope: &ExtensionWatchScope) -> Vec<PathBuf> {
    match scope {
        ExtensionWatchScope::Global => {
            let mut paths = vec![
                options.pilot_home.join("mcp.json"),
                options.pilot_home.join("plugins"),
                options.pilot_home.join("skills"),
            ];
            if let Some(root) = &options.builtin_skills_root {
                paths.push(root.clone());
            }
            paths
        }
        ExtensionWatchScope::Project(root) => {
            let dir = root.join(".pilotdeck");
            vec![
                dir.join("mcp.json"),
                dir.join("plugins"),
                dir.join("skills"),
            ]
        }
    }
}

/// The paths to sample, as `(scope index, path index, path, follow_root)`:
/// the ones never sampled yet (`initial`) or all the others.
fn sample_list(state: &State, initial: bool) -> Vec<(usize, usize, PathBuf, bool)> {
    let mut list = Vec::new();
    for (scope, record) in state.scopes.iter().enumerate() {
        for (index, watched) in record.paths.iter().enumerate() {
            if watched.fingerprint.is_none() == initial {
                list.push((scope, index, watched.path.clone(), watched.follow_root));
            }
        }
    }
    list
}

fn watch_thread(shared: Weak<Shared>) {
    let mut next_poll = Instant::now();
    // Each watched path's directory listings from its last sample, keyed by
    // `(scope index, path index)`; only this thread touches them.
    let mut listings: HashMap<(usize, usize), DirListings> = HashMap::new();
    loop {
        let Some(shared) = shared.upgrade() else {
            return;
        };
        // A new scope's starting point is taken first and on its own, so a
        // change right after `watch_project` is not folded into it while
        // the other trees are walked.
        let initial = {
            let mut state = shared.lock();
            state.added = false;
            sample_list(&state, true)
        };
        if !initial.is_empty() {
            // `resolveExistingWatchTarget` (ExtensionWatchManager.ts:170-180)
            // runs once, when the watcher is created: a path that exists then
            // (`existsSync` follows links) is watched itself, through a link
            // too; one that does not is watched from an ancestor, which never
            // looks behind a link created there later.
            let samples: Vec<(u64, bool)> = initial
                .iter()
                .map(|(scope, index, path, _)| {
                    let follow = path.exists();
                    let cache = listings.entry((*scope, *index)).or_default();
                    (fingerprint_with(path, follow, cache), follow)
                })
                .collect();
            let mut state = shared.lock();
            for ((scope, index, _, _), (sample, follow)) in initial.iter().zip(samples) {
                let watched = &mut state.scopes[*scope].paths[*index];
                watched.fingerprint = Some(sample);
                watched.follow_root = follow;
            }
        }
        let started = Instant::now();
        if started >= next_poll {
            let list = sample_list(&shared.lock(), false);
            let samples: Vec<u64> = list
                .iter()
                .map(|(scope, index, path, follow)| {
                    let cache = listings.entry((*scope, *index)).or_default();
                    fingerprint_with(path, *follow, cache)
                })
                .collect();
            let finished = Instant::now();
            let mut state = shared.lock();
            for ((scope, index, path, _), sample) in list.into_iter().zip(samples) {
                let record = &mut state.scopes[scope];
                let watched = &mut record.paths[index];
                if watched.fingerprint != Some(sample) {
                    watched.fingerprint = Some(sample);
                    record
                        .pending
                        .insert(path.to_string_lossy().encode_utf16().collect());
                    record.deadline = Some(finished + DEBOUNCE);
                }
            }
            next_poll = finished + POLL_INTERVAL.max((finished - started) * (IDLE_DUTY - 1));
        }
        let now = Instant::now();
        let mut due = Vec::new();
        let mut state = shared.lock();
        for record in state.scopes.iter_mut() {
            if record.deadline.is_some_and(|deadline| deadline <= now) {
                record.deadline = None;
                // `[...pendingPaths].sort()`: UTF-16 code unit order.
                let changed_paths = std::mem::take(&mut record.pending)
                    .into_iter()
                    .map(|units| String::from_utf16_lossy(&units))
                    .collect();
                due.push(ExtensionWatchEvent {
                    scope: record.scope.clone(),
                    changed_paths,
                });
            }
        }
        if !due.is_empty() {
            drop(state);
            for event in due {
                (shared.options.on_change)(event);
            }
            continue;
        }
        let wake_at = state
            .scopes
            .iter()
            .filter_map(|record| record.deadline)
            .fold(next_poll, Instant::min);
        let wait = wake_at.saturating_duration_since(Instant::now());
        drop(
            shared
                .wake
                .wait_timeout_while(state, wait, |state| !state.added)
                .unwrap_or_else(|e| e.into_inner()),
        );
    }
}

#[cfg(test)]
fn fingerprint(path: &Path, follow_root: bool) -> u64 {
    fingerprint_with(path, follow_root, &mut DirListings::default())
}

/// How long after a directory last changed its listing is read again on
/// every sample instead of being reused (see [`DirListings`]). Well above
/// the timestamp granularity of the file systems this runs on (Linux keeps
/// file times at a coarse, jiffy-sized clock; APFS and ext4 store
/// nanoseconds).
const LISTING_SETTLE: Duration = Duration::from_secs(2);

/// The directory listings one watched path's walk read last time, so the
/// next walk need not list a directory that has not changed since. The
/// walk still takes every entry's metadata on each sample — a change to a
/// file is seen by the file's own fields, as before — and only a
/// directory's *names* are reused, and only when its identity, size,
/// modification and status-change times are all as they were when it was
/// listed: adding, removing or renaming an entry updates both times of the
/// directory it is in, and resetting the modification time (`utimes`)
/// still moves the status-change time. A listing read within
/// [`LISTING_SETTLE`] of the directory's last change is not trusted (a
/// change landing in the same timestamp tick as the read would leave the
/// times unchanged — git's "racily clean" index entries); it is read again
/// until it is older than that.
#[derive(Default)]
struct DirListings(HashMap<PathBuf, DirListing>);

struct DirListing {
    stamp: DirStamp,
    names: Vec<std::ffi::OsString>,
}

#[derive(PartialEq, Eq)]
struct DirStamp {
    dev: u64,
    ino: u64,
    len: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl DirStamp {
    /// The stamp of a directory, and whether a listing read at `read_at`
    /// may be reused while the stamp stays the same. `None` where the
    /// platform gives no status-change time: such a directory is listed on
    /// every sample.
    #[cfg(unix)]
    fn of(metadata: &std::fs::Metadata, read_at: std::time::SystemTime) -> Option<(Self, bool)> {
        use std::os::unix::fs::MetadataExt;
        let stamp = Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            len: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        };
        let settled = read_at
            .checked_sub(LISTING_SETTLE)
            .and_then(|limit| limit.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|limit| {
                let limit = (limit.as_secs() as i64, i64::from(limit.subsec_nanos()));
                stamp.modified < limit && stamp.changed < limit
            });
        Some((stamp, settled))
    }
    #[cfg(not(unix))]
    fn of(_: &std::fs::Metadata, _: std::time::SystemTime) -> Option<(Self, bool)> {
        None
    }
}

/// Everything under `path` that a recursive watch would report a change
/// of: each entry's relative name, type, length and modification time.
/// With `follow_root` the watched path itself is followed when it is a
/// symbolic link — the baseline's `fs.watch` on an existing linked path
/// watches the link's target (measured on macOS: a write behind a linked
/// `skills` / `plugins` / project `.pilotdeck/skills` is reported) — while
/// links inside the tree never are (a write behind an inner linked directory
/// is not reported on macOS; on Linux one level behind it is, see
/// `link_target_sample`). `listings` carries the directory listings from
/// the previous sample of the same path in and this sample's out (see
/// [`DirListings`]); the result is the same as a walk that lists every
/// directory.
fn fingerprint_with(path: &Path, follow_root: bool, listings: &mut DirListings) -> u64 {
    let mut previous = std::mem::take(&mut listings.0);
    let mut hasher = DefaultHasher::new();
    let mut stack = vec![(path.to_path_buf(), PathBuf::new())];
    while let Some((absolute, relative)) = stack.pop() {
        let root = follow_root && relative.as_os_str().is_empty();
        relative.hash(&mut hasher);
        let metadata = if root {
            std::fs::metadata(&absolute)
        } else {
            std::fs::symlink_metadata(&absolute)
        };
        let Ok(metadata) = metadata else {
            0u8.hash(&mut hasher);
            continue;
        };
        let file_type = metadata.file_type();
        (file_type.is_dir(), file_type.is_symlink()).hash(&mut hasher);
        metadata.len().hash(&mut hasher);
        metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|time| time.as_nanos())
            .hash(&mut hasher);
        #[cfg(target_os = "linux")]
        if file_type.is_symlink() {
            link_target_sample(&absolute, &mut hasher);
        }
        if !file_type.is_dir() {
            continue;
        }
        let read_at = std::time::SystemTime::now();
        let stamp = DirStamp::of(&metadata, read_at);
        let reused = match (previous.remove(&absolute), &stamp) {
            (Some(listing), Some((stamp, true))) if listing.stamp == *stamp => Some(listing.names),
            _ => None,
        };
        let names = match reused {
            Some(names) => names,
            None => {
                #[cfg(test)]
                tests::LISTED.with(|listed| listed.set(listed.get() + 1));
                let Ok(entries) = std::fs::read_dir(&absolute) else {
                    continue;
                };
                let mut names: Vec<_> = entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.file_name())
                    .collect();
                names.sort();
                names
            }
        };
        // Pushed in reverse so they are visited in name order.
        for name in names.iter().rev() {
            stack.push((absolute.join(name), relative.join(name)));
        }
        if let Some((stamp, _)) = stamp {
            listings.0.insert(absolute, DirListing { stamp, names });
        }
    }
    hasher.finish()
}

/// Linux only: what the baseline's per-entry inotify watch on a symbolic
/// link inside a watched tree reports a change of. `fs.watch(link)` follows
/// the link; for a directory it reports entries appearing, disappearing or
/// being renamed in it and writes to the files directly in it, but nothing
/// further down (a file written in a subdirectory of the target is not
/// seen, though the subdirectory being created is); for a file, writes to
/// it. The link's own entries are not walked on (`#watchFolder` skips
/// links), so links found here are not followed again.
#[cfg(target_os = "linux")]
fn link_target_sample(link: &Path, hasher: &mut DefaultHasher) {
    fn file_sample(metadata: &std::fs::Metadata, hasher: &mut DefaultHasher) {
        metadata.len().hash(hasher);
        metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|time| time.as_nanos())
            .hash(hasher);
    }
    let Ok(target) = std::fs::metadata(link) else {
        0u8.hash(hasher);
        return;
    };
    1u8.hash(hasher);
    if !target.is_dir() {
        file_sample(&target, hasher);
        return;
    }
    let Ok(entries) = std::fs::read_dir(link) else {
        return;
    };
    let mut children: Vec<_> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| (entry.file_name(), entry.path()))
        .collect();
    children.sort();
    for (name, path) in children {
        name.hash(hasher);
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            0u8.hash(hasher);
            continue;
        };
        let file_type = metadata.file_type();
        (file_type.is_dir(), file_type.is_symlink()).hash(hasher);
        // A directory's own modification time moves with its contents,
        // which this watch does not see.
        if !file_type.is_dir() {
            file_sample(&metadata, hasher);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// Directories this thread's walks listed (rather than reused).
        pub(super) static LISTED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn listed() -> usize {
        LISTED.with(std::cell::Cell::get)
    }

    /// A reused listing gives the same fingerprint as a full walk, through
    /// every kind of change, and a settled directory is not listed again.
    #[cfg(unix)]
    #[test]
    fn reused_listings_match_a_full_walk() {
        let root = temp_dir("listings");
        let skills = root.join("skills");
        std::fs::create_dir_all(skills.join("a/deep")).unwrap();
        std::fs::write(skills.join("a/SKILL.md"), "x").unwrap();
        std::fs::create_dir_all(skills.join("b")).unwrap();
        let mut cache = DirListings::default();
        let fresh = fingerprint_with(&skills, true, &mut cache);
        assert_eq!(fresh, fingerprint(&skills, true));
        // Just changed: listed again on every sample until settled.
        let before = listed();
        assert_eq!(fresh, fingerprint_with(&skills, true, &mut cache));
        assert_eq!(listed() - before, 4, "unsettled directories are listed");
        std::thread::sleep(LISTING_SETTLE + Duration::from_millis(200));
        fingerprint_with(&skills, true, &mut cache);
        let before = listed();
        let settled = fingerprint_with(&skills, true, &mut cache);
        assert_eq!(listed() - before, 0, "settled directories are reused");
        assert_eq!(settled, fingerprint(&skills, true));
        // An entry added deep down, a file written, an entry removed.
        std::fs::write(skills.join("a/deep/NEW.md"), "x").unwrap();
        let added = fingerprint_with(&skills, true, &mut cache);
        assert_ne!(added, settled);
        assert_eq!(added, fingerprint(&skills, true));
        std::fs::write(skills.join("a/SKILL.md"), "longer").unwrap();
        let written = fingerprint_with(&skills, true, &mut cache);
        assert_ne!(written, added);
        assert_eq!(written, fingerprint(&skills, true));
        std::fs::remove_dir(skills.join("b")).unwrap();
        let removed = fingerprint_with(&skills, true, &mut cache);
        assert_ne!(removed, written);
        assert_eq!(removed, fingerprint(&skills, true));
        let _ = std::fs::remove_dir_all(&root);
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pdrs-extension-watch-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn fingerprint_sees_changes_under_the_path_only() {
        let root = temp_dir("fingerprint");
        let skills = root.join("skills");
        let missing = fingerprint(&skills, true);
        std::fs::create_dir_all(skills.join("a")).unwrap();
        let created = fingerprint(&skills, true);
        assert_ne!(missing, created);
        std::fs::write(root.join("unrelated.txt"), "x").unwrap();
        assert_eq!(created, fingerprint(&skills, true));
        std::fs::write(skills.join("a/SKILL.md"), "x").unwrap();
        assert_ne!(created, fingerprint(&skills, true));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_watched_path_is_followed_inner_links_per_platform() {
        let root = temp_dir("links");
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(elsewhere.join("skills")).unwrap();
        std::fs::create_dir_all(elsewhere.join("inner")).unwrap();
        let skills = root.join("skills");
        std::os::unix::fs::symlink(elsewhere.join("skills"), &skills).unwrap();
        let before = fingerprint(&skills, true);
        std::fs::create_dir_all(elsewhere.join("skills/new")).unwrap();
        std::fs::write(elsewhere.join("skills/new/SKILL.md"), "x").unwrap();
        let after = fingerprint(&skills, true);
        assert_ne!(before, after, "a write behind a linked root is seen");
        let unfollowed = fingerprint(&skills, false);
        std::fs::write(elsewhere.join("skills/new/OTHER.md"), "x").unwrap();
        assert_eq!(
            unfollowed,
            fingerprint(&skills, false),
            "a write two levels behind a link watched from an ancestor is not seen"
        );
        std::os::unix::fs::symlink(elsewhere.join("inner"), elsewhere.join("skills/linked"))
            .unwrap();
        let linked = fingerprint(&skills, true);
        std::fs::write(elsewhere.join("inner/SKILL.md"), "x").unwrap();
        if cfg!(target_os = "linux") {
            assert_ne!(
                linked,
                fingerprint(&skills, true),
                "Linux: a write directly behind an inner link is seen"
            );
        } else {
            assert_eq!(
                linked,
                fingerprint(&skills, true),
                "an inner link is not followed"
            );
        }
        std::fs::create_dir_all(elsewhere.join("inner/deep")).unwrap();
        let deep = fingerprint(&skills, true);
        std::fs::write(elsewhere.join("inner/deep/SKILL.md"), "x").unwrap();
        assert_eq!(
            deep,
            fingerprint(&skills, true),
            "a write two levels behind an inner link is not seen"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Linux: a link that appears under a path watched from its ancestor is
    /// followed one level, as Node's per-entry inotify watch does.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_follows_a_link_found_under_an_ancestor_watch_one_level() {
        let root = temp_dir("ancestor-link");
        let target = root.join("target");
        std::fs::create_dir_all(&target).unwrap();
        let skills = root.join("skills");
        std::os::unix::fs::symlink(&target, &skills).unwrap();
        let before = fingerprint(&skills, false);
        std::fs::create_dir_all(target.join("early")).unwrap();
        let created = fingerprint(&skills, false);
        assert_ne!(before, created, "a new entry behind the link is seen");
        std::fs::write(target.join("early/SKILL.md"), "x").unwrap();
        assert_eq!(
            created,
            fingerprint(&skills, false),
            "a write in a subdirectory behind the link is not seen"
        );
        std::fs::write(target.join("NOTES.txt"), "x").unwrap();
        assert_ne!(created, fingerprint(&skills, false));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn changes_are_debounced_per_scope_and_sorted() {
        let root = temp_dir("debounce");
        let home = root.join("home");
        let project = root.join("project");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let manager = ExtensionWatchManager::start(ExtensionWatchOptions {
            pilot_home: home.clone(),
            builtin_skills_root: None,
            on_change: Arc::new(move |event| sink.lock().unwrap().push(event)),
        });
        manager.watch_project(&project);
        manager.sampled().await;
        std::fs::create_dir_all(home.join("skills/x")).unwrap();
        std::fs::write(home.join("mcp.json"), "{}").unwrap();
        std::fs::create_dir_all(project.join(".pilotdeck/plugins/p")).unwrap();
        tokio::time::sleep(Duration::from_millis(800)).await;
        let events = events.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![
                ExtensionWatchEvent {
                    scope: ExtensionWatchScope::Global,
                    changed_paths: vec![
                        home.join("mcp.json").to_string_lossy().into_owned(),
                        home.join("skills").to_string_lossy().into_owned(),
                    ],
                },
                ExtensionWatchEvent {
                    scope: ExtensionWatchScope::Project(project.clone()),
                    changed_paths: vec![
                        project
                            .join(".pilotdeck/plugins")
                            .to_string_lossy()
                            .into_owned()
                    ],
                },
            ]
        );
        drop(manager);
        let _ = std::fs::remove_dir_all(&root);
    }
}
