//! Strict canonical workspace boundary enforcement (`WorkspaceBoundary`).
//!
//! Guards all workspace file and search operations against:
//! - non-absolute or uncanonicalizable workspace roots,
//! - NUL bytes and empty paths,
//! - `..` parent traversal segments,
//! - symlinks whose canonical target escapes the workspace root (for both
//!   existing files and non-existent write targets via nearest-existing-ancestor
//!   canonicalization and dangling-symlink inspection).

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

const MAX_SYMLINK_HOPS: usize = 40;

/// Machine-readable category for [`BoundaryError`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BoundaryErrorKind {
    InvalidRoot,
    InvalidPath,
    OutsideWorkspace,
    SymlinkEscape,
    NotFound,
    NotADirectory,
    Io,
}

/// Fail-closed workspace containment or path resolution error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundaryError {
    pub kind: BoundaryErrorKind,
    pub message: String,
}

impl BoundaryError {
    pub fn new(kind: BoundaryErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn from_io(err: std::io::Error, path_hint: &str) -> Self {
        let kind = match err.kind() {
            ErrorKind::NotFound => BoundaryErrorKind::NotFound,
            ErrorKind::NotADirectory => BoundaryErrorKind::NotADirectory,
            _ => BoundaryErrorKind::Io,
        };
        Self::new(kind, format!("filesystem error for '{path_hint}': {err}"))
    }
}

impl fmt::Display for BoundaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for BoundaryError {}

/// Canonicalized target guaranteed to stay inside a [`WorkspaceBoundary`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedWorkspacePath {
    pub canonical_path: PathBuf,
    /// Workspace-relative POSIX path (`/` separated); `"."` for the root itself.
    pub relative_path: String,
}

/// Canonical workspace root containment policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceBoundary {
    canonical_root: PathBuf,
}

impl WorkspaceBoundary {
    /// Construct a boundary from an existing, absolute, canonicalizable directory.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, BoundaryError> {
        let root_path = root.as_ref();
        if root_path.as_os_str().is_empty() {
            return Err(BoundaryError::new(
                BoundaryErrorKind::InvalidRoot,
                "workspace root cannot be empty",
            ));
        }
        if root_path.as_os_str().as_encoded_bytes().contains(&0) {
            return Err(BoundaryError::new(
                BoundaryErrorKind::InvalidRoot,
                "workspace root cannot contain NUL bytes",
            ));
        }
        if !root_path.is_absolute() {
            return Err(BoundaryError::new(
                BoundaryErrorKind::InvalidRoot,
                format!(
                    "workspace root must be absolute: {}",
                    root_path.to_string_lossy()
                ),
            ));
        }
        let canonical_root = fs::canonicalize(root_path).map_err(|err| {
            BoundaryError::new(
                BoundaryErrorKind::InvalidRoot,
                format!(
                    "workspace root cannot be canonicalized ({}): {err}",
                    root_path.to_string_lossy()
                ),
            )
        })?;
        let meta = fs::metadata(&canonical_root).map_err(|err| {
            BoundaryError::new(
                BoundaryErrorKind::InvalidRoot,
                format!(
                    "workspace root metadata unavailable ({}): {err}",
                    canonical_root.to_string_lossy()
                ),
            )
        })?;
        if !meta.is_dir() {
            return Err(BoundaryError::new(
                BoundaryErrorKind::InvalidRoot,
                format!(
                    "workspace root must be a directory: {}",
                    canonical_root.to_string_lossy()
                ),
            ));
        }
        Ok(Self { canonical_root })
    }

    /// Canonical absolute workspace root path.
    pub fn root(&self) -> &Path {
        &self.canonical_root
    }

    /// Returns true iff `canonical` lies strictly within (or equals) [`Self::root`].
    pub fn contains_canonical(&self, canonical: &Path) -> bool {
        canonical.starts_with(&self.canonical_root)
    }

    /// Convert a path under [`Self::root`] to a normalized POSIX relative path.
    pub fn relative_posix(&self, contained: &Path) -> Result<String, BoundaryError> {
        let stripped = contained.strip_prefix(&self.canonical_root).map_err(|_| {
            BoundaryError::new(
                BoundaryErrorKind::OutsideWorkspace,
                format!(
                    "path '{}' is outside workspace root",
                    contained.to_string_lossy()
                ),
            )
        })?;
        let mut parts = Vec::new();
        for comp in stripped.components() {
            match comp {
                Component::CurDir => {}
                Component::Normal(seg) => {
                    parts.push(seg.to_string_lossy().into_owned());
                }
                _ => {
                    return Err(BoundaryError::new(
                        BoundaryErrorKind::InvalidPath,
                        format!(
                            "unexpected component in workspace path '{}'",
                            contained.to_string_lossy()
                        ),
                    ));
                }
            }
        }
        if parts.is_empty() {
            Ok(".".to_owned())
        } else {
            Ok(parts.join("/"))
        }
    }

    /// Resolve an existing file or directory inside the workspace boundary.
    pub fn resolve_existing(&self, raw: &str) -> Result<ResolvedWorkspacePath, BoundaryError> {
        validate_raw_path(raw)?;
        let candidate = self.candidate_path(raw);

        match fs::canonicalize(&candidate) {
            Ok(real) => {
                if !self.contains_canonical(&real) {
                    return Err(escape_error(raw, &candidate, &self.canonical_root));
                }
                let relative_path = self.relative_posix(&real)?;
                Ok(ResolvedWorkspacePath {
                    canonical_path: real,
                    relative_path,
                })
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {
                self.check_missing_containment(raw, &candidate)?;
                Err(BoundaryError::new(
                    BoundaryErrorKind::NotFound,
                    format!("path '{raw}' does not exist"),
                ))
            }
            Err(err) => Err(BoundaryError::from_io(err, raw)),
        }
    }

    /// Resolve a target path for creating or overwriting a file inside the workspace.
    ///
    /// If the target (or parent directories) do not exist yet, the nearest existing
    /// ancestor is canonicalized and verified to stay within [`Self::root`].
    /// Escaping symlinks (including dangling symlinks pointing outside the
    /// workspace) are rejected.
    pub fn resolve_write_target(&self, raw: &str) -> Result<ResolvedWorkspacePath, BoundaryError> {
        validate_raw_path(raw)?;
        let candidate = self.candidate_path(raw);
        self.resolve_write_candidate(raw, &candidate, 0)
    }

    /// Resolve a search directory root (`None` defaults to workspace root `"."`).
    pub fn resolve_search_root(
        &self,
        raw: Option<&str>,
    ) -> Result<ResolvedWorkspacePath, BoundaryError> {
        match raw {
            None => Ok(ResolvedWorkspacePath {
                canonical_path: self.canonical_root.clone(),
                relative_path: ".".to_owned(),
            }),
            Some(path) => self.resolve_existing(path),
        }
    }

    /// Inspect a directory entry encountered during bounded tree traversal.
    ///
    /// Returns `Ok(None)` for symlinks that are broken or resolve outside the
    /// workspace boundary so directory scans never escape or leak external files.
    pub fn inspect_walk_entry(
        &self,
        entry_path: &Path,
    ) -> Result<Option<(ResolvedWorkspacePath, bool)>, BoundaryError> {
        let meta = match fs::symlink_metadata(entry_path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(BoundaryError::from_io(err, &entry_path.to_string_lossy())),
        };
        let is_symlink = meta.file_type().is_symlink();
        let real = match fs::canonicalize(entry_path) {
            Ok(real) => real,
            Err(err) if is_symlink && err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(BoundaryError::from_io(err, &entry_path.to_string_lossy())),
        };
        if !self.contains_canonical(&real) {
            return Ok(None);
        }
        let target_meta = if is_symlink {
            match fs::metadata(&real) {
                Ok(m) => m,
                Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
                Err(err) => return Err(BoundaryError::from_io(err, &real.to_string_lossy())),
            }
        } else {
            meta
        };
        let relative_path = self.relative_posix(&real)?;
        Ok(Some((
            ResolvedWorkspacePath {
                canonical_path: real,
                relative_path,
            },
            target_meta.is_dir(),
        )))
    }

    fn candidate_path(&self, raw: &str) -> PathBuf {
        let path = Path::new(raw);
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.canonical_root.join(path)
        };
        normalize_curdir(&joined)
    }

    fn check_missing_containment(&self, raw: &str, candidate: &Path) -> Result<(), BoundaryError> {
        let (ancestor, tail) = nearest_existing_ancestor(candidate, raw)?;
        let meta = fs::symlink_metadata(&ancestor)
            .map_err(|err| BoundaryError::from_io(err, &ancestor.to_string_lossy()))?;
        if meta.file_type().is_symlink() {
            match fs::canonicalize(&ancestor) {
                Ok(real) => {
                    if !self.contains_canonical(&real) {
                        return Err(escape_error(raw, &ancestor, &self.canonical_root));
                    }
                }
                Err(err) if err.kind() == ErrorKind::NotFound => {
                    let link_target = follow_dangling_link_target(&ancestor)?;
                    self.verify_dangling_target_contained(raw, &link_target, 1)?;
                    if !tail.is_empty() {
                        return Err(BoundaryError::new(
                            BoundaryErrorKind::NotFound,
                            format!("parent directory in '{raw}' is a broken symlink"),
                        ));
                    }
                }
                Err(err) => return Err(BoundaryError::from_io(err, raw)),
            }
        } else {
            let real = fs::canonicalize(&ancestor)
                .map_err(|err| BoundaryError::from_io(err, &ancestor.to_string_lossy()))?;
            if !self.contains_canonical(&real) {
                return Err(escape_error(raw, &ancestor, &self.canonical_root));
            }
        }
        Ok(())
    }

    fn resolve_write_candidate(
        &self,
        raw: &str,
        candidate: &Path,
        depth: usize,
    ) -> Result<ResolvedWorkspacePath, BoundaryError> {
        if depth > MAX_SYMLINK_HOPS {
            return Err(BoundaryError::new(
                BoundaryErrorKind::SymlinkEscape,
                format!("too many symlink hops resolving '{raw}'"),
            ));
        }
        let (ancestor, missing_tail) = nearest_existing_ancestor(candidate, raw)?;
        let meta = fs::symlink_metadata(&ancestor)
            .map_err(|err| BoundaryError::from_io(err, &ancestor.to_string_lossy()))?;

        if meta.file_type().is_symlink() {
            match fs::canonicalize(&ancestor) {
                Ok(real_ancestor) => {
                    if !self.contains_canonical(&real_ancestor) {
                        return Err(escape_error(raw, &ancestor, &self.canonical_root));
                    }
                    let mut target_meta_is_dir = fs::metadata(&real_ancestor)
                        .map_err(|err| BoundaryError::from_io(err, raw))?
                        .is_dir();
                    if !missing_tail.is_empty() && !target_meta_is_dir {
                        return Err(BoundaryError::new(
                            BoundaryErrorKind::NotADirectory,
                            format!("parent path for '{raw}' is not a directory"),
                        ));
                    }
                    target_meta_is_dir = true;
                    let _ = target_meta_is_dir;
                    let canonical_path = append_tail(real_ancestor, &missing_tail);
                    if !self.contains_canonical(&canonical_path) {
                        return Err(escape_error(raw, &canonical_path, &self.canonical_root));
                    }
                    let relative_path = self.relative_posix(&canonical_path)?;
                    Ok(ResolvedWorkspacePath {
                        canonical_path,
                        relative_path,
                    })
                }
                Err(err) if err.kind() == ErrorKind::NotFound => {
                    let link_target = follow_dangling_link_target(&ancestor)?;
                    self.verify_dangling_target_contained(raw, &link_target, depth + 1)?;
                    if !missing_tail.is_empty() {
                        return Err(BoundaryError::new(
                            BoundaryErrorKind::NotADirectory,
                            format!("parent directory in '{raw}' is a broken symlink"),
                        ));
                    }
                    self.resolve_write_candidate(raw, &link_target, depth + 1)
                }
                Err(err) => Err(BoundaryError::from_io(err, raw)),
            }
        } else {
            let real_ancestor = fs::canonicalize(&ancestor)
                .map_err(|err| BoundaryError::from_io(err, &ancestor.to_string_lossy()))?;
            if !self.contains_canonical(&real_ancestor) {
                return Err(escape_error(raw, &ancestor, &self.canonical_root));
            }
            if !missing_tail.is_empty() && !meta.is_dir() {
                return Err(BoundaryError::new(
                    BoundaryErrorKind::NotADirectory,
                    format!("parent path for '{raw}' is not a directory"),
                ));
            }
            let canonical_path = append_tail(real_ancestor, &missing_tail);
            if !self.contains_canonical(&canonical_path) {
                return Err(escape_error(raw, &canonical_path, &self.canonical_root));
            }
            let relative_path = self.relative_posix(&canonical_path)?;
            Ok(ResolvedWorkspacePath {
                canonical_path,
                relative_path,
            })
        }
    }

    fn verify_dangling_target_contained(
        &self,
        raw: &str,
        target: &Path,
        depth: usize,
    ) -> Result<(), BoundaryError> {
        if depth > MAX_SYMLINK_HOPS {
            return Err(BoundaryError::new(
                BoundaryErrorKind::SymlinkEscape,
                format!("too many symlink hops resolving '{raw}'"),
            ));
        }
        let (ancestor, _) = nearest_existing_ancestor(target, raw)?;
        let meta = fs::symlink_metadata(&ancestor)
            .map_err(|err| BoundaryError::from_io(err, &ancestor.to_string_lossy()))?;
        if meta.file_type().is_symlink() {
            match fs::canonicalize(&ancestor) {
                Ok(real) => {
                    if !self.contains_canonical(&real) {
                        return Err(BoundaryError::new(
                            BoundaryErrorKind::SymlinkEscape,
                            format!("symlink target for '{raw}' escapes workspace boundary"),
                        ));
                    }
                }
                Err(err) if err.kind() == ErrorKind::NotFound => {
                    let next = follow_dangling_link_target(&ancestor)?;
                    self.verify_dangling_target_contained(raw, &next, depth + 1)?;
                }
                Err(err) => return Err(BoundaryError::from_io(err, raw)),
            }
        } else {
            let real = fs::canonicalize(&ancestor)
                .map_err(|err| BoundaryError::from_io(err, &ancestor.to_string_lossy()))?;
            if !self.contains_canonical(&real) {
                return Err(BoundaryError::new(
                    BoundaryErrorKind::SymlinkEscape,
                    format!("symlink target for '{raw}' escapes workspace boundary"),
                ));
            }
        }
        Ok(())
    }
}

fn validate_raw_path(raw: &str) -> Result<(), BoundaryError> {
    if raw.is_empty() {
        return Err(BoundaryError::new(
            BoundaryErrorKind::InvalidPath,
            "path must not be empty",
        ));
    }
    if raw.contains('\0') {
        return Err(BoundaryError::new(
            BoundaryErrorKind::InvalidPath,
            "path must not contain NUL bytes",
        ));
    }
    if raw.split(['/', '\\']).any(|segment| segment == "..")
        || Path::new(raw)
            .components()
            .any(|comp| matches!(comp, Component::ParentDir))
    {
        return Err(BoundaryError::new(
            BoundaryErrorKind::OutsideWorkspace,
            format!("path '{raw}' contains '..' traversal which is not allowed"),
        ));
    }
    Ok(())
}

fn normalize_curdir(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

fn nearest_existing_ancestor(
    candidate: &Path,
    raw: &str,
) -> Result<(PathBuf, Vec<OsString>), BoundaryError> {
    let mut probe = candidate.to_path_buf();
    let mut tail = Vec::new();
    loop {
        match fs::symlink_metadata(&probe) {
            Ok(_) => return Ok((probe, tail)),
            Err(err) if err.kind() == ErrorKind::NotFound => {
                let Some(name) = probe.file_name().map(OsString::from) else {
                    return Err(BoundaryError::new(
                        BoundaryErrorKind::OutsideWorkspace,
                        format!("path '{raw}' has no existing ancestor inside workspace"),
                    ));
                };
                tail.push(name);
                if !probe.pop() {
                    return Err(BoundaryError::new(
                        BoundaryErrorKind::OutsideWorkspace,
                        format!("path '{raw}' has no existing ancestor inside workspace"),
                    ));
                }
            }
            Err(err) => return Err(BoundaryError::from_io(err, raw)),
        }
    }
}

fn append_tail(mut base: PathBuf, reversed_tail: &[OsString]) -> PathBuf {
    for seg in reversed_tail.iter().rev() {
        base.push(seg);
    }
    base
}

fn follow_dangling_link_target(link_path: &Path) -> Result<PathBuf, BoundaryError> {
    let target = fs::read_link(link_path)
        .map_err(|err| BoundaryError::from_io(err, &link_path.to_string_lossy()))?;
    let joined = if target.is_absolute() {
        target
    } else {
        let parent = link_path.parent().unwrap_or_else(|| Path::new("/"));
        parent.join(target)
    };
    Ok(normalize_lexical_for_symlink(&joined))
}

fn normalize_lexical_for_symlink(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => out.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(seg) => out.push(seg),
        }
    }
    out
}

fn escape_error(raw: &str, candidate: &Path, canonical_root: &Path) -> BoundaryError {
    if !Path::new(raw).is_absolute() || candidate.starts_with(canonical_root) {
        BoundaryError::new(
            BoundaryErrorKind::SymlinkEscape,
            format!("path '{raw}' resolves outside workspace boundary via symlink"),
        )
    } else {
        BoundaryError::new(
            BoundaryErrorKind::OutsideWorkspace,
            format!("path '{raw}' is outside workspace boundary"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempWorkspace {
        dir: PathBuf,
    }

    impl TempWorkspace {
        fn new(tag: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let base = std::env::temp_dir().join(format!(
                "pilotdeck-tool-core-guard-{}-{}-{}",
                tag,
                std::process::id(),
                n
            ));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            Self { dir: base }
        }

        fn path(&self) -> &Path {
            &self.dir
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn rejects_relative_nonexistent_and_file_workspace_roots() {
        assert_eq!(
            WorkspaceBoundary::new("relative/dir").unwrap_err().kind,
            BoundaryErrorKind::InvalidRoot
        );
        let ws = TempWorkspace::new("bad-root");
        let missing = ws.path().join("does-not-exist");
        assert_eq!(
            WorkspaceBoundary::new(&missing).unwrap_err().kind,
            BoundaryErrorKind::InvalidRoot
        );
        let file = ws.path().join("file.txt");
        fs::write(&file, b"hi").unwrap();
        assert_eq!(
            WorkspaceBoundary::new(&file).unwrap_err().kind,
            BoundaryErrorKind::InvalidRoot
        );
        assert_eq!(
            WorkspaceBoundary::new("/tmp\0bad").unwrap_err().kind,
            BoundaryErrorKind::InvalidRoot
        );
    }

    #[test]
    fn rejects_nul_bytes_parent_traversal_and_prefix_siblings() {
        let root_ws = TempWorkspace::new("ws");
        let sibling_ws = TempWorkspace::new("ws_secret");
        let sibling_file = sibling_ws.path().join("secret.txt");
        fs::write(&sibling_file, b"secret").unwrap();

        let boundary = WorkspaceBoundary::new(root_ws.path()).unwrap();
        assert_eq!(
            boundary.resolve_existing("a\0b.txt").unwrap_err().kind,
            BoundaryErrorKind::InvalidPath
        );
        assert_eq!(
            boundary.resolve_existing("").unwrap_err().kind,
            BoundaryErrorKind::InvalidPath
        );
        assert_eq!(
            boundary.resolve_existing("../escape.txt").unwrap_err().kind,
            BoundaryErrorKind::OutsideWorkspace
        );
        assert_eq!(
            boundary
                .resolve_write_target("sub/../../escape.txt")
                .unwrap_err()
                .kind,
            BoundaryErrorKind::OutsideWorkspace
        );
        assert_eq!(
            boundary
                .resolve_existing(&sibling_file.to_string_lossy())
                .unwrap_err()
                .kind,
            BoundaryErrorKind::OutsideWorkspace
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escapes_for_existing_and_nonexistent_write_targets() {
        use std::os::unix::fs::symlink;

        let root_ws = TempWorkspace::new("sym-ws");
        let outside_ws = TempWorkspace::new("sym-outside");
        fs::write(outside_ws.path().join("secret.txt"), b"top-secret").unwrap();
        fs::create_dir_all(root_ws.path().join("inside_dir")).unwrap();

        symlink(
            outside_ws.path().join("secret.txt"),
            root_ws.path().join("file_link.txt"),
        )
        .unwrap();
        symlink(outside_ws.path(), root_ws.path().join("dir_link")).unwrap();
        symlink(
            outside_ws.path().join("missing.txt"),
            root_ws.path().join("dangling_outside.txt"),
        )
        .unwrap();
        symlink(
            root_ws.path().join("inside_dir"),
            root_ws.path().join("good_link"),
        )
        .unwrap();

        let boundary = WorkspaceBoundary::new(root_ws.path()).unwrap();

        assert_eq!(
            boundary.resolve_existing("file_link.txt").unwrap_err().kind,
            BoundaryErrorKind::SymlinkEscape
        );
        assert_eq!(
            boundary
                .resolve_existing("dir_link/nonexistent.txt")
                .unwrap_err()
                .kind,
            BoundaryErrorKind::SymlinkEscape
        );
        assert_eq!(
            boundary
                .resolve_write_target("dir_link/nested/new_file.txt")
                .unwrap_err()
                .kind,
            BoundaryErrorKind::SymlinkEscape
        );
        assert_eq!(
            boundary
                .resolve_write_target("dangling_outside.txt")
                .unwrap_err()
                .kind,
            BoundaryErrorKind::SymlinkEscape
        );

        let inside = boundary
            .resolve_write_target("good_link/sub/created.txt")
            .unwrap();
        assert_eq!(inside.relative_path, "inside_dir/sub/created.txt");
    }

    #[test]
    fn rejects_backslash_traversal_and_non_directory_parents() {
        let ws = TempWorkspace::new("parents");
        fs::write(ws.path().join("regular.txt"), b"data").unwrap();
        let boundary = WorkspaceBoundary::new(ws.path()).unwrap();

        assert_eq!(
            boundary
                .resolve_write_target(r"sub\..\escape.txt")
                .unwrap_err()
                .kind,
            BoundaryErrorKind::OutsideWorkspace
        );
        assert_eq!(
            boundary
                .resolve_write_target("regular.txt/child/new.txt")
                .unwrap_err()
                .kind,
            BoundaryErrorKind::NotADirectory
        );
        let deep = boundary
            .resolve_write_target("a/b/c/d/new_file.txt")
            .unwrap();
        assert_eq!(deep.relative_path, "a/b/c/d/new_file.txt");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_loops_and_relative_dangling_symlink_escapes() {
        use std::os::unix::fs::symlink;

        let ws = TempWorkspace::new("loops");
        fs::create_dir_all(ws.path().join("sub")).unwrap();
        symlink(ws.path().join("b"), ws.path().join("a")).unwrap();
        symlink(ws.path().join("a"), ws.path().join("b")).unwrap();
        symlink("../outside_missing.txt", ws.path().join("rel_escape.txt")).unwrap();
        symlink("inside_missing.txt", ws.path().join("rel_inside.txt")).unwrap();

        let boundary = WorkspaceBoundary::new(ws.path()).unwrap();

        assert!(boundary.resolve_existing("a").is_err());
        assert_eq!(
            boundary
                .resolve_write_target("rel_escape.txt")
                .unwrap_err()
                .kind,
            BoundaryErrorKind::SymlinkEscape
        );
        let inside_dangling = boundary.resolve_write_target("rel_inside.txt").unwrap();
        assert_eq!(inside_dangling.relative_path, "inside_missing.txt");
    }

    #[test]
    fn resolves_un_canonicalized_absolute_alias_paths_inside_workspace() {
        let ws = TempWorkspace::new("alias-abs");
        fs::write(ws.path().join("file.txt"), b"ok").unwrap();
        let boundary = WorkspaceBoundary::new(ws.path()).unwrap();

        let existing = boundary
            .resolve_existing(&ws.path().join("file.txt").to_string_lossy())
            .unwrap();
        assert_eq!(existing.relative_path, "file.txt");

        let future = boundary
            .resolve_write_target(&ws.path().join("new_sub/next.txt").to_string_lossy())
            .unwrap();
        assert_eq!(future.relative_path, "new_sub/next.txt");
    }
}
