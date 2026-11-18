//! Minimal test utilities: an isolated temporary directory helper built on
//! `std` only (the crate pins its dependency set to serde/serde_json).
//! Tests must never touch `~/.pilotdeck` or `~/.agentcpm-pd`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// An isolated temporary directory removed on drop.
pub(crate) struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "agentcpm-pd-compat-{tag}-{}-{unique}",
            std::process::id()
        ));
        // Retry a few times in the (unlikely) case of a name collision.
        for attempt in 0..8 {
            let candidate = if attempt == 0 {
                path.clone()
            } else {
                std::env::temp_dir().join(format!(
                    "agentcpm-pd-compat-{tag}-{}-{unique}-{attempt}",
                    std::process::id()
                ))
            };
            match std::fs::create_dir(&candidate) {
                Ok(()) => return Self { path: candidate },
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("tempdir create failed: {e}"),
            }
        }
        panic!("tempdir create: name collision after retries")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Best-effort cleanup; leaked temp dirs are only ever test scratch.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_cleans_up_unique_directories() {
        let first = TempDir::new("util");
        assert!(first.path().is_dir());
        let second = TempDir::new("util");
        assert_ne!(first.path(), second.path());
        let path = first.path().to_path_buf();
        drop(first);
        assert!(!path.exists(), "TempDir must remove itself on drop");
    }
}
