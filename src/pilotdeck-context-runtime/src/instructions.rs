//! PILOTDECK.md instruction discovery (TS `context/instructions/InstructionDiscovery.ts`).
//!
//! Discovery is intentionally storage-agnostic at the boundary: missing files,
//! directories and permission failures are all "absent layer" as in Node, while
//! file contents are decoded with replacement like `readFile(path, "utf8")`.

use std::{
    collections::HashSet,
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Managed,
    User,
    Project,
    ProjectRules,
    Local,
}
impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::User => "user",
            Self::Project => "project",
            Self::ProjectRules => "project-rules",
            Self::Local => "local",
        }
    }
    pub fn description(self) -> &'static str {
        match self {
            Self::Managed => " (managed instructions, set by administrator)",
            Self::User => " (user's global instructions for all projects)",
            Self::Project => " (project instructions, checked into the codebase)",
            Self::ProjectRules => " (project rule, checked into the codebase)",
            Self::Local => " (user's private project instructions, not checked in)",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layer {
    pub scope: Scope,
    pub path: String,
    pub content: String,
}

#[derive(Clone, Debug)]
pub struct InstructionDiscovery {
    project_root: PathBuf,
    cwd: PathBuf,
    pilot_home: PathBuf,
    managed_dir: Option<PathBuf>,
}
/// Host-bound, scoped instruction access. Entries report the same lstat-style
/// regular-file predicate used by Node Dirent.isFile (symlinks are false).
pub trait InstructionAccess: Send + Sync {
    fn read(&self, path: &Path) -> Result<Vec<u8>, String>;
    fn list(&self, path: &Path) -> Result<Vec<InstructionEntry>, String>;
}
pub struct InstructionEntry {
    pub name: String,
    pub is_file: bool,
}

impl InstructionDiscovery {
    /// All roots must be absolute and supplied by the trusted host.
    pub fn new(
        project_root: PathBuf,
        cwd: PathBuf,
        pilot_home: PathBuf,
        managed_dir: Option<PathBuf>,
    ) -> Result<Self, String> {
        for path in [&project_root, &cwd, &pilot_home]
            .into_iter()
            .chain(managed_dir.iter())
        {
            if !path.is_absolute() {
                return Err("instruction root must be absolute".into());
            }
        }
        Ok(Self {
            project_root,
            cwd,
            pilot_home,
            managed_dir,
        })
    }

    pub fn discover(&self, access: &dyn InstructionAccess) -> Result<Vec<Layer>, String> {
        self.discover_with_access(access)
    }

    fn discover_with_access(&self, access: &dyn InstructionAccess) -> Result<Vec<Layer>, String> {
        let mut layers = Vec::new();
        let mut seen = HashSet::new();
        let managed_dir = self.managed_dir.clone();
        if let Some(dir) = &managed_dir {
            self.try_add(
                access,
                &mut layers,
                &mut seen,
                Scope::Managed,
                absolute(dir)?.join("PILOTDECK.md"),
            )?;
        }
        let home = absolute(&self.pilot_home)?;
        self.try_add(
            access,
            &mut layers,
            &mut seen,
            Scope::User,
            home.join("PILOTDECK.md"),
        )?;
        self.try_add_rules_dir(
            access,
            &mut layers,
            &mut seen,
            Scope::User,
            home.join("rules"),
        )?;
        for dir in self.collect_directory_chain()? {
            self.try_add(
                access,
                &mut layers,
                &mut seen,
                Scope::Project,
                dir.join("PILOTDECK.md"),
            )?;
            self.try_add(
                access,
                &mut layers,
                &mut seen,
                Scope::Project,
                dir.join(".pilotdeck").join("PILOTDECK.md"),
            )?;
            self.try_add_rules_dir(
                access,
                &mut layers,
                &mut seen,
                Scope::ProjectRules,
                dir.join(".pilotdeck").join("rules"),
            )?;
            self.try_add(
                access,
                &mut layers,
                &mut seen,
                Scope::Local,
                dir.join("PILOTDECK.local.md"),
            )?;
        }
        Ok(layers)
    }

    /// Root-first chain from `project_root` down to `cwd`, matching TS even when
    /// cwd is outside the project (the loop then runs up to `/`).
    fn collect_directory_chain(&self) -> Result<Vec<PathBuf>, String> {
        let root = absolute(&self.project_root)?;
        let current = absolute(&self.cwd)?;
        if root == current {
            return Ok(vec![root]);
        }
        let mut dirs = Vec::new();
        let mut dir = current;
        loop {
            let parent = dir.parent().map_or_else(|| dir.clone(), Path::to_path_buf);
            if dir == root || dir == parent {
                break;
            }
            dirs.push(dir);
            dir = parent;
        }
        dirs.push(root);
        dirs.reverse();
        Ok(dirs)
    }

    fn try_add(
        &self,
        access: &dyn InstructionAccess,
        layers: &mut Vec<Layer>,
        seen: &mut HashSet<PathBuf>,
        scope: Scope,
        file_path: PathBuf,
    ) -> Result<(), String> {
        let resolved = absolute(&file_path)?;
        if !seen.insert(resolved.clone()) {
            return Ok(());
        }
        let Ok(bytes) = access.read(&resolved) else {
            return Ok(());
        };
        // Node's UTF-8 decoder replaces ill-formed sequences and never throws.
        let content = String::from_utf8_lossy(&bytes);
        let trimmed = js_trim(&content);
        if !trimmed.is_empty() {
            layers.push(Layer {
                scope,
                path: resolved.to_string_lossy().into_owned(),
                content: trimmed.to_owned(),
            });
        }
        Ok(())
    }

    fn try_add_rules_dir(
        &self,
        access: &dyn InstructionAccess,
        layers: &mut Vec<Layer>,
        seen: &mut HashSet<PathBuf>,
        scope: Scope,
        dir_path: PathBuf,
    ) -> Result<(), String> {
        let dir_path = absolute(&dir_path)?;
        let Ok(entries) = access.list(&dir_path) else {
            return Ok(());
        };
        let mut names = Vec::new();
        for entry in entries {
            // Dirent.isFile() is false for symlinks: file_type() is lstat.
            if !entry.is_file {
                continue;
            }
            let name = entry.name;
            if name.ends_with(".md") && name != "SKILL.md" {
                names.push(name);
            }
        }
        // Array.prototype.sort default: UTF-16 code-unit order, not bytes.
        names.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        for name in names {
            self.try_add(access, layers, seen, scope, dir_path.join(name))?;
        }
        Ok(())
    }
}

// Gateway metadata::resolve_path lexical normalization; no process cwd fallback.
fn absolute(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("instruction path must be absolute".into());
    }
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                output.pop();
            }
            Component::CurDir => {}
            component => output.push(component.as_os_str()),
        }
    }
    Ok(output)
}
// Gateway metadata::js_trim, ECMAScript whitespace and line terminators.
pub(crate) fn js_trim(value: &str) -> &str {
    value.trim_matches(|c| matches!(c, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'))
}
