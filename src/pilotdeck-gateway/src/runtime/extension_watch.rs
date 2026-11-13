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
//!   of every entry under it (type, length, modification time; symbolic links
//!   are not followed, as `fs.watch` does not report changes behind them).
//!   A change is therefore seen up to one poll interval late, and a change
//!   that leaves every fingerprinted field as it was is not seen.
//! * The polling runs on its own thread, never on an async worker, and no
//!   lock is held while it touches the file system: registering a scope
//!   (`watch_project`, called on every project resolve under the registry's
//!   lock) only records the paths and wakes the thread, which samples a new
//!   scope's starting point before anything else. A pass over the trees is
//!   followed by a pause of at least `IDLE_DUTY - 1` times its own length:
//!   a huge tree (a `node_modules` under `plugins/`) is polled less often
//!   instead of keeping a core busy, at the price of its changes being seen
//!   later.

use std::collections::{BTreeSet, hash_map::DefaultHasher};
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

/// The paths to sample, as `(scope index, path index, path)`: the ones never
/// sampled yet (`initial`) or all the others.
fn sample_list(state: &State, initial: bool) -> Vec<(usize, usize, PathBuf)> {
    let mut list = Vec::new();
    for (scope, record) in state.scopes.iter().enumerate() {
        for (index, watched) in record.paths.iter().enumerate() {
            if watched.fingerprint.is_none() == initial {
                list.push((scope, index, watched.path.clone()));
            }
        }
    }
    list
}

fn watch_thread(shared: Weak<Shared>) {
    let mut next_poll = Instant::now();
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
            let samples: Vec<u64> = initial
                .iter()
                .map(|(_, _, path)| fingerprint(path))
                .collect();
            let mut state = shared.lock();
            for ((scope, index, _), sample) in initial.iter().zip(samples) {
                state.scopes[*scope].paths[*index].fingerprint = Some(sample);
            }
        }
        let started = Instant::now();
        if started >= next_poll {
            let list = sample_list(&shared.lock(), false);
            let samples: Vec<u64> = list.iter().map(|(_, _, path)| fingerprint(path)).collect();
            let finished = Instant::now();
            let mut state = shared.lock();
            for ((scope, index, path), sample) in list.into_iter().zip(samples) {
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

/// Everything under `path` that a recursive watch would report a change
/// of: each entry's relative name, type, length and modification time.
fn fingerprint(path: &Path) -> u64 {
    let mut hasher = DefaultHasher::new();
    let mut stack = vec![(path.to_path_buf(), PathBuf::new())];
    while let Some((absolute, relative)) = stack.pop() {
        relative.hash(&mut hasher);
        let Ok(metadata) = std::fs::symlink_metadata(&absolute) else {
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
        if file_type.is_dir()
            && let Ok(entries) = std::fs::read_dir(&absolute)
        {
            let mut names: Vec<_> = entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name())
                .collect();
            names.sort();
            // Pushed in reverse so they are visited in name order.
            for name in names.into_iter().rev() {
                stack.push((absolute.join(&name), relative.join(&name)));
            }
        }
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let missing = fingerprint(&skills);
        std::fs::create_dir_all(skills.join("a")).unwrap();
        let created = fingerprint(&skills);
        assert_ne!(missing, created);
        std::fs::write(root.join("unrelated.txt"), "x").unwrap();
        assert_eq!(created, fingerprint(&skills));
        std::fs::write(skills.join("a/SKILL.md"), "x").unwrap();
        assert_ne!(created, fingerprint(&skills));
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
