//! Optional Git Web updater. This is not a Rust self-updater.
use super::{CommandError, UpdateResult};
#[cfg(unix)]
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, waitid};
use std::path::Path;
#[cfg(unix)]
use std::{
    io::{self, Read},
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};
#[cfg(unix)]
use std::{
    os::unix::process::CommandExt,
    process::{Child, ExitStatus},
};

pub const MISSING_UPDATER: &str = "This installation does not include the Git Web updater. Use application settings or update manually.";
/// Resolve the same optional Git Web installation for CLI and IM channels.
pub fn find_update_root(start: Option<&Path>) -> Option<&Path> {
    start?
        .ancestors()
        .find(|dir| dir.join("scripts/update-web.mjs").exists())
}
/// Shared executable, arguments, cwd and pipe contract; each caller retains
/// its own lifecycle and output consumer appropriate to its entrypoint.
pub fn command(root: &Path, args: &[String]) -> std::process::Command {
    let mut child = std::process::Command::new("node");
    child
        .arg(root.join("scripts/update-web.mjs"))
        .args(args)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    child.process_group(0);
    child
}
#[cfg(unix)]
pub struct OwnedUpdater {
    child: Child,
    group: Option<Pid>,
}

#[cfg(unix)]
impl OwnedUpdater {
    pub fn new(child: Child) -> Self {
        let group = Pid::from_raw(child.id() as i32).expect("spawn returned positive updater PID");
        Self {
            child,
            group: Some(group),
        }
    }

    pub fn exited(&mut self) -> io::Result<bool> {
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

    pub fn signal(&self, signal: Signal) {
        if let Some(group) = self.group {
            let _ = rustix::process::kill_process_group(group, signal);
        }
    }

    fn terminate_and_reap(&mut self) -> io::Result<()> {
        let signal_result = match self.group {
            Some(group) => match rustix::process::kill_process_group(group, Signal::KILL) {
                Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
                Err(error) => Err(io::Error::from(error)),
            },
            None => return Err(io::Error::other("updater process ownership lost")),
        };
        let reap_result = self.reap().map(|_| ());
        signal_result.and(reap_result)
    }

    pub fn reap(&mut self) -> io::Result<ExitStatus> {
        // The last group signal has already been sent. Dropping the anchor
        // before wait prevents any later path from targeting a reused PGID.
        self.group = None;
        self.child.wait()
    }
}

#[cfg(unix)]
impl Drop for OwnedUpdater {
    fn drop(&mut self) {
        if self.group.is_some() {
            self.signal(Signal::KILL);
            let _ = self.reap();
        }
    }
}

#[cfg(unix)]
type Outcome = Result<Option<UpdateResult>, CommandError>;
/// A single updater owns its process group and a repeatable completion receipt.
/// Dropping the handle requests cancellation; explicit cancellation waits for
/// leader reap and both output pipes to be closed before resolving.
#[cfg(unix)]
pub struct UpdateOperation {
    cancel: Arc<AtomicU8>,
    receipt: tokio::sync::watch::Receiver<Option<Outcome>>,
}
#[cfg(unix)]
impl UpdateOperation {
    async fn outcome(&mut self) -> Outcome {
        loop {
            if let Some(result) = self.receipt.borrow_and_update().clone() {
                return result;
            }
            self.receipt
                .changed()
                .await
                .map_err(|_| CommandError("updater supervisor stopped".into()))?;
        }
    }
    pub async fn result(&mut self) -> Result<UpdateResult, CommandError> {
        self.outcome()
            .await?
            .ok_or_else(|| CommandError("updater cancelled".into()))
    }
    pub async fn cancel_and_drain(&mut self) -> Result<(), CommandError> {
        let _ = self
            .cancel
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst);
        self.outcome().await.map(|_| ())
    }
}
#[cfg(unix)]
impl Drop for UpdateOperation {
    fn drop(&mut self) {
        let _ = self
            .cancel
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst);
    }
}
#[cfg(not(unix))]
pub struct UpdateOperation;
#[cfg(not(unix))]
impl UpdateOperation {
    pub async fn result(&mut self) -> Result<UpdateResult, CommandError> {
        Err(CommandError(
            "Git Web updater is unavailable on this platform.".into(),
        ))
    }
    pub async fn cancel_and_drain(&mut self) -> Result<(), CommandError> {
        Err(CommandError(
            "Git Web updater is unavailable on this platform.".into(),
        ))
    }
}

#[cfg(unix)]
pub fn start(executable: Option<&Path>, check: bool) -> Result<UpdateOperation, CommandError> {
    let root = find_update_root(executable.and_then(Path::parent))
        .ok_or_else(|| CommandError(MISSING_UPDATER.into()))?;
    let args = [if check { "--check" } else { "--restart" }.to_owned()];
    let mut child = command(root, &args).spawn().map_err(spawn_error)?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let owner = OwnedUpdater::new(child);
    fn nonblocking(fd: impl std::os::fd::AsFd) -> Result<(), CommandError> {
        let flags = rustix::fs::fcntl_getfl(&fd).map_err(|e| CommandError(e.to_string().into()))?;
        rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK)
            .map_err(|e| CommandError(e.to_string().into()))
    }
    nonblocking(&stdout)?;
    nonblocking(&stderr)?;
    let cancel = Arc::new(AtomicU8::new(0));
    let worker_cancel = cancel.clone();
    let (sender, receipt) = tokio::sync::watch::channel(None);
    let _worker = std::thread::Builder::new()
        .name("pilotdeck-im-updater".into())
        .spawn(move || {
            let result = supervise(owner, &mut stdout, &mut stderr, &worker_cancel);
            drop(stdout);
            drop(stderr);
            sender.send_replace(Some(result));
        })
        .map_err(|error| CommandError(error.to_string().into()))?;
    Ok(UpdateOperation { cancel, receipt })
}
#[cfg(not(unix))]
pub fn start(_: Option<&Path>, _: bool) -> Result<UpdateOperation, CommandError> {
    Err(CommandError(
        "Git Web updater is unavailable on this platform.".into(),
    ))
}

#[cfg(unix)]
pub(super) async fn run(
    executable: Option<&Path>,
    check: bool,
) -> Result<UpdateResult, CommandError> {
    start(executable, check)?.result().await
}

#[cfg(not(unix))]
pub(super) async fn run(_: Option<&Path>, _: bool) -> Result<UpdateResult, CommandError> {
    Err(CommandError(
        "Git Web updater is unavailable on this platform.".into(),
    ))
}

/// Shared Node-style spawn error spelling used by both entrypoints.
pub fn spawn_error_message(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => "spawn node ENOENT".to_owned(),
        std::io::ErrorKind::PermissionDenied => "spawn node EACCES".to_owned(),
        _ => error.to_string(),
    }
}
#[cfg(unix)]
fn spawn_error(error: io::Error) -> CommandError {
    CommandError(spawn_error_message(&error).into())
}

#[cfg(unix)]
fn supervise(
    mut owner: OwnedUpdater,
    stdout: &mut impl Read,
    stderr: &mut impl Read,
    cancel: &AtomicU8,
) -> Result<Option<UpdateResult>, CommandError> {
    let mut a = [0u8; 8192];
    let mut b = [0u8; 8192];
    let mut a_open = true;
    let mut b_open = true;
    let mut tail = Vec::<u16>::new();
    loop {
        if cancel.load(Ordering::SeqCst) == 1 {
            // Drop retains the unreaped leader as PGID ownership anchor while
            // signaling its group, then reaps. Pipe readers close with us.
            owner
                .terminate_and_reap()
                .map_err(|e| CommandError(e.to_string().into()))?;
            return Ok(None);
        }
        let mut progress = false;
        for (stream, buffer, open) in [
            (&mut *stdout as &mut dyn Read, &mut a[..], &mut a_open),
            (&mut *stderr as &mut dyn Read, &mut b[..], &mut b_open),
        ] {
            if !*open {
                continue;
            }
            match stream.read(buffer) {
                Ok(0) => {
                    *open = false;
                    progress = true;
                }
                Ok(n) => {
                    progress = true;
                    tail.extend(String::from_utf8_lossy(&buffer[..n]).encode_utf16());
                    if tail.len() > 12000 {
                        tail.drain(..tail.len() - 12000);
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(CommandError(error.to_string().into())),
            }
        }
        if !a_open
            && !b_open
            && owner
                .exited()
                .map_err(|e| CommandError(e.to_string().into()))?
        {
            // Race caller-drop against completion while the waitable leader
            // still anchors the PGID; a winning drop must not be lost.
            if cancel
                .compare_exchange(0, 2, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                owner
                    .terminate_and_reap()
                    .map_err(|e| CommandError(e.to_string().into()))?;
                return Ok(None);
            }
            let status = owner
                .reap()
                .map_err(|e| CommandError(e.to_string().into()))?;
            return Ok(Some(UpdateResult {
                code: Some(status.code().unwrap_or(1)),
                output: super::trim(&pilotdeck_protocol::JsString::from_utf16(tail)),
            }));
        }
        if !progress {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
