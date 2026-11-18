//! Install refusal policy for the distribution tools (t021): the shim must
//! never overwrite an existing `pilotdeck` binary (executable file or
//! symlink) and never modifies PATH itself.
//!
//! The check is deliberately on *existence*, not on executability: whatever
//! occupies the target path is something the user put there, and the
//! distribution must ask, not replace.

use std::fmt;
use std::path::{Path, PathBuf};

/// What currently occupies the target path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Existing {
    RegularFile,
    Symlink,
    Directory,
    Other,
}

/// The target already holds something; refusing to overwrite it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallRefused {
    pub target: PathBuf,
    pub existing: Existing,
}

impl InstallRefused {
    /// Error code for programmatic consumers.
    pub fn code(&self) -> &'static str {
        "install_target_occupied"
    }
}

impl fmt::Display for InstallRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "refusing to install over {} {} (error: {}); remove or move it first, the shim never overwrites an existing pilotdeck",
            match self.existing {
                Existing::RegularFile => "existing file",
                Existing::Symlink => "existing symlink",
                Existing::Directory => "existing directory",
                Existing::Other => "existing entry",
            },
            self.target.display(),
            self.code()
        )
    }
}

impl std::error::Error for InstallRefused {}

/// Checks a distribution target path (e.g. `<prefix>/bin/pilotdeck`).
/// `Ok(())` means the path is free to create; any existing entry (file,
/// symlink — including a dangling one —, directory, anything) is refused.
/// The function never creates, moves, or removes anything, and it never
/// touches PATH.
pub fn check_install_target(bin_path: &Path) -> Result<(), InstallRefused> {
    // `symlink_metadata` does not follow links, so a dangling symlink is
    // still classified as a symlink rather than "not found".
    match std::fs::symlink_metadata(bin_path) {
        Err(_) => Ok(()), // nothing there: free to install
        Ok(meta) => {
            let existing = if meta.file_type().is_symlink() {
                Existing::Symlink
            } else if meta.is_dir() {
                Existing::Directory
            } else if meta.is_file() {
                Existing::RegularFile
            } else {
                Existing::Other
            };
            Err(InstallRefused {
                target: bin_path.to_path_buf(),
                existing,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn empty_target_is_installable() {
        let dir = TempDir::new("install-empty");
        let target = dir.path().join("bin").join("pilotdeck");
        assert_eq!(check_install_target(&target), Ok(()));
    }

    #[test]
    fn existing_regular_file_is_refused() {
        let dir = TempDir::new("install-file");
        let target = dir.path().join("pilotdeck");
        std::fs::write(&target, b"legacy binary\n").unwrap();
        match check_install_target(&target) {
            Err(refused) => {
                assert_eq!(refused.existing, Existing::RegularFile);
                assert_eq!(refused.target, target);
                assert_eq!(refused.code(), "install_target_occupied");
                assert!(refused.to_string().contains("never overwrites"));
            }
            Ok(()) => panic!("must refuse"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn existing_symlink_is_refused_including_dangling() {
        let dir = TempDir::new("install-symlink");
        let real = dir.path().join("real-pilotdeck");
        std::fs::write(&real, b"binary\n").unwrap();
        let link = dir.path().join("pilotdeck");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(
            check_install_target(&link).unwrap_err().existing,
            Existing::Symlink
        );

        let dangling = dir.path().join("pilotdeck-dangling");
        std::os::unix::fs::symlink(dir.path().join("nowhere"), &dangling).unwrap();
        assert_eq!(
            check_install_target(&dangling).unwrap_err().existing,
            Existing::Symlink
        );
    }

    #[test]
    fn existing_directory_is_refused() {
        let dir = TempDir::new("install-dir");
        let target = dir.path().join("pilotdeck");
        std::fs::create_dir_all(&target).unwrap();
        assert_eq!(
            check_install_target(&target).unwrap_err().existing,
            Existing::Directory
        );
    }

    #[test]
    fn sibling_files_do_not_block_the_target() {
        let dir = TempDir::new("install-sibling");
        std::fs::write(dir.path().join("other"), b"x").unwrap();
        assert_eq!(check_install_target(&dir.path().join("pilotdeck")), Ok(()));
    }
}
