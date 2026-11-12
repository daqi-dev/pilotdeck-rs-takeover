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

use std::collections::{BTreeSet, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::time::{Instant, MissedTickBehavior};

/// `debounceMs ?? 250`.
pub const DEBOUNCE: Duration = Duration::from_millis(250);
/// How often every watched tree is sampled.
pub const POLL_INTERVAL: Duration = Duration::from_millis(50);

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
    fingerprint: u64,
}

struct ScopeRecord {
    scope: ExtensionWatchScope,
    paths: Vec<WatchedPath>,
    pending: BTreeSet<Vec<u16>>,
    deadline: Option<Instant>,
}

struct Shared {
    options: ExtensionWatchOptions,
    scopes: Mutex<Vec<ScopeRecord>>,
}

/// `ExtensionWatchManager`, started. Dropping every handle stops the polling
/// task at its next tick.
#[derive(Clone)]
pub struct ExtensionWatchManager {
    shared: Arc<Shared>,
}

impl ExtensionWatchManager {
    /// `new ExtensionWatchManager(options).start()`: the global scope is
    /// watched from here on. Must be called inside a Tokio runtime.
    pub fn start(options: ExtensionWatchOptions) -> Self {
        let manager = Self {
            shared: Arc::new(Shared {
                options,
                scopes: Mutex::new(Vec::new()),
            }),
        };
        manager.ensure_scope(ExtensionWatchScope::Global);
        tokio::spawn(poll_loop(Arc::downgrade(&manager.shared)));
        manager
    }

    /// `watchProject(projectRoot)`; the root is already resolved.
    pub fn watch_project(&self, project_root: &Path) {
        self.ensure_scope(ExtensionWatchScope::Project(project_root.to_path_buf()));
    }

    fn ensure_scope(&self, scope: ExtensionWatchScope) {
        let mut scopes = self.shared.scopes.lock().unwrap_or_else(|e| e.into_inner());
        if scopes.iter().any(|record| record.scope == scope) {
            return;
        }
        let paths = watched_paths(&self.shared.options, &scope)
            .into_iter()
            .map(|path| WatchedPath {
                fingerprint: fingerprint(&path),
                path,
            })
            .collect();
        scopes.push(ScopeRecord {
            scope,
            paths,
            pending: BTreeSet::new(),
            deadline: None,
        });
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

async fn poll_loop(shared: Weak<Shared>) {
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let Some(shared) = shared.upgrade() else {
            return;
        };
        let now = Instant::now();
        let mut due = Vec::new();
        {
            let mut scopes = shared.scopes.lock().unwrap_or_else(|e| e.into_inner());
            for record in scopes.iter_mut() {
                for watched in &mut record.paths {
                    let current = fingerprint(&watched.path);
                    if current != watched.fingerprint {
                        watched.fingerprint = current;
                        record
                            .pending
                            .insert(watched.path.to_string_lossy().encode_utf16().collect());
                        record.deadline = Some(now + DEBOUNCE);
                    }
                }
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
        }
        for event in due {
            (shared.options.on_change)(event);
        }
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
