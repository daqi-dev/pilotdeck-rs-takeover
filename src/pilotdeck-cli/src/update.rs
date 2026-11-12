//! Optional Git Web updater entry point.
//!
//! The fixed baseline's `src/runtime/updateCommand.ts:6-25` discovers the
//! updater shipped by an existing Git Web installation and starts that
//! installation's management script. The Web management component remains
//! outside the native backend scope: this module neither imports it into the
//! Rust runtime nor claims to update a standalone Rust distribution. A native
//! installation without the script fails before looking for Node.
use crate::{Failure, stderr_line};
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, waitid};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc::{SyncSender, sync_channel},
};
use std::time::{Duration, Instant};

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
    let signals = UpdateSignals::install()?;
    if let Some(signal) = signals.take() {
        finish_with_signal(signal);
    }
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
        .process_group(0)
        .spawn()?;
    let stdout = child.stdout.take().expect("piped updater stdout");
    let stderr = child.stderr.take().expect("piped updater stderr");
    let owner = OwnedUpdater::new(child);

    // Drain both pipes concurrently: waiting for either stream first can
    // deadlock a verbose build. The bounded queue keeps streamed build output
    // bounded in memory. The baseline forwards both streams to CLI stdout.
    std::thread::scope(|scope| {
        let output_finished = Arc::new(AtomicBool::new(false));
        let supervisor_finished = Arc::clone(&output_finished);
        // Cancellation must continue even if the user's stdout is blocked or
        // a separately-owned Web guardian keeps an inherited pipe open.
        let supervisor = scope.spawn(move || supervise(owner, signals, supervisor_finished));
        let (sender, receiver) = sync_channel(16);
        let stdout_sender = sender.clone();
        let stdout_reader = scope.spawn(move || read_output(stdout, stdout_sender));
        let stderr_reader = scope.spawn(move || read_output(stderr, sender));
        let mut output = io::stdout().lock();
        for chunk in receiver {
            let _ = output.write_all(&chunk);
            let _ = output.flush();
        }
        output_finished.store(true, Ordering::SeqCst);
        let stdout_result = stdout_reader
            .join()
            .map_err(|_| io::Error::other("updater stdout reader failed"))?;
        let stderr_result = stderr_reader
            .join()
            .map_err(|_| io::Error::other("updater stderr reader failed"))?;
        let status = supervisor
            .join()
            .map_err(|_| io::Error::other("updater supervisor failed"))?;
        stdout_result.and(stderr_result)?;
        status
    })
}

struct SignalRegistration {
    pending: Arc<AtomicUsize>,
    default_action: Arc<AtomicBool>,
    active: AtomicBool,
}

static UPDATE_SIGNALS: OnceLock<Result<Arc<SignalRegistration>, String>> = OnceLock::new();

struct UpdateSignals(Arc<SignalRegistration>);

impl UpdateSignals {
    fn install() -> io::Result<Self> {
        let registration = UPDATE_SIGNALS
            .get_or_init(|| {
                let registration = Arc::new(SignalRegistration {
                    pending: Arc::new(AtomicUsize::new(0)),
                    default_action: Arc::new(AtomicBool::new(true)),
                    active: AtomicBool::new(false),
                });
                for signal in [SIGINT, SIGTERM] {
                    signal_hook::flag::register_conditional_default(
                        signal,
                        Arc::clone(&registration.default_action),
                    )
                    .map_err(|error| error.to_string())?;
                    signal_hook::flag::register_usize(
                        signal,
                        Arc::clone(&registration.pending),
                        signal as usize,
                    )
                    .map_err(|error| error.to_string())?;
                }
                Ok(registration)
            })
            .as_ref()
            .map_err(|error| io::Error::other(error.clone()))?;
        if registration.active.swap(true, Ordering::SeqCst) {
            return Err(io::Error::other(
                "update signal listeners are already active",
            ));
        }
        registration.pending.store(0, Ordering::SeqCst);
        registration.default_action.store(false, Ordering::SeqCst);
        Ok(Self(Arc::clone(registration)))
    }

    fn take(&self) -> Option<i32> {
        match self.0.pending.swap(0, Ordering::SeqCst) as i32 {
            signal @ (SIGINT | SIGTERM) => Some(signal),
            _ => None,
        }
    }
}

impl Drop for UpdateSignals {
    fn drop(&mut self) {
        // signal-hook retains its OS handler; a conditional default restores
        // the normal process behavior outside this command's owned lifetime.
        self.0.default_action.store(true, Ordering::SeqCst);
        self.0.active.store(false, Ordering::SeqCst);
    }
}

struct OwnedUpdater {
    child: Child,
    group: Option<Pid>,
}

impl OwnedUpdater {
    fn new(child: Child) -> Self {
        let group = Pid::from_raw(child.id() as i32).expect("spawn returned positive updater PID");
        Self {
            child,
            group: Some(group),
        }
    }

    fn exited(&mut self) -> io::Result<bool> {
        let group = self.group.expect("updater has not been reaped");
        match waitid(
            WaitId::Pid(group),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        ) {
            Ok(status) => Ok(status.is_some()),
            Err(rustix::io::Errno::INTR) => Ok(false),
            Err(error) => {
                if error == rustix::io::Errno::CHILD {
                    // An externally reaped PID is no longer an ownership
                    // certificate. Never signal that numeric group again.
                    self.group = None;
                }
                Err(error.into())
            }
        }
    }

    fn signal(&self, signal: Signal) {
        if let Some(group) = self.group {
            let _ = rustix::process::kill_process_group(group, signal);
        }
    }

    fn reap(&mut self) -> io::Result<ExitStatus> {
        // The last group signal has already been sent. Dropping the anchor
        // before wait prevents any later path from targeting a reused PGID.
        self.group = None;
        self.child.wait()
    }
}

impl Drop for OwnedUpdater {
    fn drop(&mut self) {
        if self.group.is_some() {
            self.signal(Signal::KILL);
            let _ = self.reap();
        }
    }
}

fn supervise(
    mut owner: OwnedUpdater,
    signals: UpdateSignals,
    output_finished: Arc<AtomicBool>,
) -> io::Result<ExitStatus> {
    loop {
        if let Some(signal) = signals.take() {
            // D1-UPDATE-CANCEL-OWNERSHIP-001: only this newly-created group
            // belongs to the CLI. Web server tasks and explicitly detached
            // Web build guardians have separate ownership and are not killed.
            owner.signal(if signal == SIGINT {
                Signal::INT
            } else {
                Signal::TERM
            });
            let grace = Instant::now() + Duration::from_secs(3);
            while Instant::now() < grace && signals.take().is_none() {
                std::thread::sleep(Duration::from_millis(5));
            }
            // Keep the leader waitable (including as a zombie) through this
            // final signal, so its PID/group cannot be reused underneath us.
            owner.signal(Signal::KILL);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match owner.exited() {
                    Ok(true) => {
                        if let Err(error) = owner.reap() {
                            stderr_line(&format!(
                                "Update cancellation could not reap the updater: {error}"
                            ));
                        }
                        break;
                    }
                    Ok(false) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Ok(false) => {
                        stderr_line("Update cancellation could not confirm updater termination.");
                        break;
                    }
                    Err(error) => {
                        stderr_line(&format!(
                            "Update cancellation could not confirm updater termination: {error}"
                        ));
                        break;
                    }
                }
            }
            // Preserve the parent's observable signal status after cleanup,
            // rather than converting user cancellation into an update result.
            finish_with_signal(signal);
        }
        if output_finished.load(Ordering::SeqCst) && owner.exited()? {
            return owner.reap();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn finish_with_signal(signal: i32) -> ! {
    let _ = signal_hook::low_level::emulate_default_handler(signal);
    // INT and TERM terminate above; retain a failing fallback if the platform
    // cannot emulate its default handler.
    std::process::exit(128 + signal)
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
