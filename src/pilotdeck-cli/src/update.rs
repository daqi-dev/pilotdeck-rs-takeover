//! Optional Git Web updater entry point.
//!
//! The fixed baseline's `src/runtime/updateCommand.ts:6-25` discovers the
//! updater shipped by an existing Git Web installation and starts that
//! installation's management script. The Web management component remains
//! outside the native backend scope: this module neither imports it into the
//! Rust runtime nor claims to update a standalone Rust distribution. A native
//! installation without the script fails before looking for Node.
use crate::{Failure, stderr_line};
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{SyncSender, sync_channel};

const MISSING_UPDATER: &str = "This installation does not include the Git Web updater. Use application settings or update manually.";

/// `handleUpdateCommand` (`src/cli/pilotdeck.ts:351-359`). Arguments belong
/// to the installed updater, including validation of unknown options. Exit 2
/// means that the release is already installed and is successful at the CLI.
pub(crate) fn run(argv: &[String]) -> Result<u8, Failure> {
    let executable = std::env::current_exe()
        .ok()
        .map(|path| std::fs::canonicalize(&path).unwrap_or(path));
    let root = find_update_root(executable.as_deref().and_then(Path::parent));
    let Some(root) = root else {
        stderr_line(&format!("Update failed: {MISSING_UPDATER}"));
        return Ok(1);
    };

    match run_process(root, argv) {
        Ok(status) => Ok(u8::from(!matches!(status.code(), Some(0 | 2)))),
        Err(error) => {
            stderr_line(&format!("Update failed: {}", process_error(&error)));
            Ok(1)
        }
    }
}

fn find_update_root(start: Option<&Path>) -> Option<&Path> {
    start?
        .ancestors()
        .find(|directory| directory.join("scripts/update-web.mjs").exists())
}

fn run_process(root: &Path, argv: &[String]) -> io::Result<ExitStatus> {
    // The baseline inherits its Node executable (`process.execPath`). A Rust
    // CLI only needs Node for this optional Web-management command, resolved
    // through the installation owner's PATH. No TS gateway/agent fallback is
    // introduced into the native startup path.
    let mut child = Command::new("node")
        .arg(root.join("scripts/update-web.mjs"))
        .args(argv)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().expect("piped updater stdout");
    let stderr = child.stderr.take().expect("piped updater stderr");

    // Drain both pipes concurrently: waiting for either stream first can
    // deadlock a verbose build. The bounded queue keeps streamed build output
    // bounded in memory. The baseline forwards both streams to CLI stdout.
    let relay_result = std::thread::scope(|scope| {
        let (sender, receiver) = sync_channel(16);
        let stdout_sender = sender.clone();
        let stdout_reader = scope.spawn(move || read_output(stdout, stdout_sender));
        let stderr_reader = scope.spawn(move || read_output(stderr, sender));
        let mut output = io::stdout().lock();
        for chunk in receiver {
            let _ = output.write_all(&chunk);
            let _ = output.flush();
        }
        let stdout_result = stdout_reader
            .join()
            .map_err(|_| io::Error::other("updater stdout reader failed"))?;
        let stderr_result = stderr_reader
            .join()
            .map_err(|_| io::Error::other("updater stderr reader failed"))?;
        stdout_result.and(stderr_result)
    });
    // Reap the exact child even if a pipe read failed. Signal termination has
    // no code, which the CLI maps to failure just as the baseline maps null.
    let status = child.wait();
    relay_result?;
    status
}

fn read_output(mut stream: impl Read, sender: SyncSender<Vec<u8>>) -> io::Result<()> {
    let mut buffer = [0_u8; 8192];
    let mut pending = Vec::with_capacity(buffer.len() + 3);
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => {
                let tail = decode_output(&mut pending, true);
                if !tail.is_empty() {
                    let _ = sender.send(tail);
                }
                return Ok(());
            }
            Ok(length) => {
                pending.extend_from_slice(&buffer[..length]);
                let decoded = decode_output(&mut pending, false);
                if !decoded.is_empty() && sender.send(decoded).is_err() {
                    return Ok(());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

/// Keep an incomplete UTF-8 suffix until the next read on this same pipe.
/// D1-UPDATE-UTF8-CHUNKS-001: the baseline decodes each data callback alone,
/// which can corrupt a valid character split across callbacks. Invalid bytes
/// and a truncated final sequence still use the usual replacement character.
fn decode_output(pending: &mut Vec<u8>, eof: bool) -> Vec<u8> {
    let consumed = if eof {
        pending.len()
    } else {
        let mut cursor = 0;
        loop {
            match std::str::from_utf8(&pending[cursor..]) {
                Ok(_) => break pending.len(),
                Err(error) => {
                    cursor += error.valid_up_to();
                    if let Some(length) = error.error_len() {
                        cursor += length;
                    } else {
                        break cursor;
                    }
                }
            }
        }
    };
    let decoded = String::from_utf8_lossy(&pending[..consumed])
        .into_owned()
        .into_bytes();
    pending.drain(..consumed);
    decoded
}

fn process_error(error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::NotFound => "spawn node ENOENT".to_owned(),
        io::ErrorKind::PermissionDenied => "spawn node EACCES".to_owned(),
        _ => error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_installation_ancestor_owns_the_updater() {
        let root = std::env::temp_dir().join(format!(
            "pdrs-c1-update-root-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let deep = root.join("outer/inner/bin");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(find_update_root(Some(&deep)), None);
        for directory in [&root, &root.join("outer/inner")] {
            std::fs::create_dir_all(directory.join("scripts")).unwrap();
            std::fs::write(directory.join("scripts/update-web.mjs"), "").unwrap();
        }
        assert_eq!(
            find_update_root(Some(&deep)),
            Some(root.join("outer/inner").as_path())
        );
        assert_eq!(find_update_root(None), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn output_decoder_preserves_split_characters_and_replaces_invalid_bytes() {
        let original = "中文🙂".as_bytes();
        for split in 0..=original.len() {
            let mut pending = original[..split].to_vec();
            let mut output = decode_output(&mut pending, false);
            pending.extend_from_slice(&original[split..]);
            output.extend(decode_output(&mut pending, true));
            assert_eq!(output, original, "byte split {split}");
            assert!(pending.is_empty());
        }
        let mut invalid = vec![b'a', 0xff, 0xe4, 0xb8];
        assert_eq!(decode_output(&mut invalid, false), "a�".as_bytes());
        assert_eq!(decode_output(&mut invalid, true), "�".as_bytes());
        assert!(invalid.is_empty());
    }
}
