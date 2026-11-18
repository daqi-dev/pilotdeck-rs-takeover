//! Live PTY runtime for the terminals domain (t015 PR-3).
//!
//! The Gateway process owns every live terminal ([`terminal_registry`]):
//! the pty master (nonblocking, polled through `tokio::io::AsyncFd`), the
//! retained output ring, the single-writer input lane, and the child
//! supervisor. The supervisor — not the close handler — reaps the child, and
//! only after every group signal for the terminal has been sent: the child
//! is the session leader of its own session (`setsid` + `TIOCSCTTY` through
//! `pilotdeck-child-signals`), so its PGID is its PID and reaping it early
//! would let a concurrent close's fallback `SIGKILL` hit a reused PGID
//! (`process_group.rs`'s WNOWAIT discipline).
//!
//! Task layout per terminal:
//! - a reader task: master readable -> append bytes to the ring -> notify;
//! - the supervisor task: observes the leader with WNOWAIT, executes the
//!   close chain on request (SIGHUP -> 3 s grace -> SIGKILL, or immediate
//!   SIGKILL for force), reaps, waits for the master's EOF, records the
//!   exit, and finalizes the SQLite row + `terminals.changed` event.
//!
//! Nothing here touches SQLite except through the store methods of
//! `domains/terminals.rs`; this file owns process and byte-stream mechanics.

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Stdio as StdStdio;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use agentcpm_pd_pty::{Master, PtyError, PtyPair};
use rustix::process::{Pid, Signal};
use tokio::io::AsyncFd;
use tokio::process::{Child, Command};
use tokio::sync::Notify;

/// Retained output window per terminal. The contract's byte-cursor + gap
/// semantics replace the TS baseline's 5000-chunk reconnect buffer
/// (`terminalSessionRegistry.js`); 4 MiB keeps a full screenful of scrollback
/// replayable after a detach.
pub(crate) const OUTPUT_RING_BYTES: usize = 4 * 1024 * 1024;

/// Max raw bytes per delivered output frame: keeps the serialized base64
/// `sub.event` (4/3 expansion + envelope overhead) comfortably under the
/// 1 MiB wire frame cap.
pub(crate) const OUTPUT_FRAME_BYTES: usize = 512 * 1024;

/// Grace period between the graceful group signal and the final SIGKILL
/// (the hooks executor's 3 s precedent).
const KILL_GRACE: Duration = Duration::from_secs(3);

/// Poll cadence of the supervisor's WNOWAIT leader check.
const SUPERVISOR_POLL: Duration = Duration::from_millis(50);

/// Bounded wait for the reader to observe the master's EOF after the child
/// is reaped, so the exit frame follows the very last retained bytes.
const EOF_DRAIN_GRACE: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Output ring
// ---------------------------------------------------------------------------

/// The retained byte window of one terminal's output stream. `base` is the
/// offset of `buf`'s first byte; `written` is the total ever appended (the
/// next offset). Bytes before `base` have been evicted by the capacity bound.
pub(crate) struct OutputRing {
    buf: VecDeque<u8>,
    base: u64,
    written: u64,
}

impl OutputRing {
    fn new() -> Self {
        Self {
            buf: VecDeque::new(),
            base: 0,
            written: 0,
        }
    }

    pub(crate) fn positions(&self) -> (u64, u64) {
        (self.base, self.written)
    }

    fn append(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.buf.push_back(*byte);
        }
        self.written = self.written.saturating_add(bytes.len() as u64);
        let overflow = self.buf.len().saturating_sub(OUTPUT_RING_BYTES);
        if overflow > 0 {
            self.buf.drain(..overflow);
            self.base = self.base.saturating_add(overflow as u64);
        }
    }

    /// Copies at most `max` bytes starting at `from`. `Ok(None)` means `from`
    /// is behind the retained window; `Ok(Some(data))` is the copy (possibly
    /// empty when caught up).
    fn copy_from(&self, from: u64, max: usize) -> Option<Vec<u8>> {
        if from < self.base {
            return None;
        }
        if from >= self.written {
            return Some(Vec::new());
        }
        let skip = (from - self.base) as usize;
        let end = self.buf.len().min(skip + max);
        Some(
            self.buf
                .iter()
                .skip(skip)
                .take(end - skip)
                .copied()
                .collect(),
        )
    }
}

// ---------------------------------------------------------------------------
// Exit state
// ---------------------------------------------------------------------------

/// The live-side exit observation, set once by the supervisor after the
/// leader is reaped AND the master's EOF has drained.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExitFacts {
    pub exit_code: Option<i32>,
    pub exit_signal: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CloseRequest {
    /// No close requested; the supervisor just observes the leader.
    None,
    /// SIGHUP the group, allow the grace period, then SIGKILL.
    Graceful { deadline: std::time::Instant },
    /// SIGKILL the group immediately.
    Force,
}

pub(crate) struct TerminalHandle {
    pub terminal_id: String,
    io: AsyncFd<Master>,
    ring: StdMutex<OutputRing>,
    /// Wakes on: output appended, EOF observed, exit recorded, close
    /// requested. The `terminals.output` stream source polls on this.
    pub notify: Arc<Notify>,
    /// Serializes master writes (the contract's single-writer lane).
    pub write_lock: tokio::sync::Mutex<()>,
    /// Once true, `terminals.input` answers TERMINAL_CLOSED.
    input_closed: std::sync::atomic::AtomicBool,
    exit: StdMutex<Option<ExitFacts>>,
    eof_seen: std::sync::atomic::AtomicBool,
    close_request: StdMutex<CloseRequest>,
}

impl TerminalHandle {
    /// Appends terminal output and wakes observers.
    pub(crate) fn push_output(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.ring.lock().expect("terminal ring").append(bytes);
        self.notify.notify_waiters();
    }

    /// `(earliest, latest)` byte offsets of the retained window.
    pub(crate) fn ring_positions(&self) -> (u64, u64) {
        self.ring.lock().expect("terminal ring").positions()
    }

    /// Copies retained bytes from `from`; `None` when `from` is behind the
    /// retained window (the caller must emit a gap and restart from
    /// [`TerminalHandle::ring_positions`]'s earliest).
    pub(crate) fn copy_output(&self, from: u64, max: usize) -> Option<Vec<u8>> {
        self.ring
            .lock()
            .expect("terminal ring")
            .copy_from(from, max)
    }

    /// The recorded exit, once the leader is reaped and the master drained.
    pub(crate) fn exit_facts(&self) -> Option<ExitFacts> {
        self.exit.lock().expect("terminal exit").clone()
    }

    /// Whether the master has observed EOF (all slave descriptors closed).
    pub(crate) fn eof_seen(&self) -> bool {
        self.eof_seen.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Whether `terminals.input` is still accepted.
    pub(crate) fn inputs_open(&self) -> bool {
        !self.input_closed.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn mark_input_closed(&self) {
        self.input_closed
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn record_exit(&self, facts: ExitFacts) {
        *self.exit.lock().expect("terminal exit") = Some(facts);
        self.notify.notify_waiters();
    }

    fn mark_eof(&self) {
        self.eof_seen
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Requests terminal closure: graceful (SIGHUP -> grace -> SIGKILL) or
    /// force (immediate SIGKILL). Safe to call repeatedly; the supervisor
    /// owns the actual signaling. A force request upgrades an in-flight
    /// graceful one (the contract allows force only as an explicit call, and
    /// an explicit force must not wait out an earlier request's grace).
    pub(crate) fn request_close(&self, graceful: bool) {
        {
            let mut request = self.close_request.lock().expect("terminal close");
            let upgrade = match (&*request, graceful) {
                (CloseRequest::None, _) => true,
                (_, false) => !matches!(*request, CloseRequest::Force),
                _ => false,
            };
            if upgrade {
                *request = if graceful {
                    CloseRequest::Graceful {
                        deadline: std::time::Instant::now()
                            .checked_add(KILL_GRACE)
                            .unwrap_or_else(std::time::Instant::now),
                    }
                } else {
                    CloseRequest::Force
                };
            }
        }
        // Close always ends input acceptance immediately, whatever the mode.
        self.mark_input_closed();
        self.notify.notify_waiters();
    }

    /// Writes terminal input to the master, nonblocking loop on the same
    /// AsyncFd the reader polls.
    pub(crate) async fn write_input(&self, bytes: &[u8]) -> Result<(), PtyError> {
        let mut rest = bytes;
        while !rest.is_empty() {
            let mut ready = self.io.writable().await;
            match ready.get_ref().write(rest) {
                Ok(0) => {
                    return Err(PtyError::Io(std::io::Error::other(
                        "pty master write returned 0",
                    )));
                }
                Ok(n) => rest = &rest[n..],
                Err(PtyError::Io(e)) if e.kind() == ErrorKind::WouldBlock => {
                    ready.clear_ready();
                }
                Err(other) => return Err(other),
            }
        }
        Ok(())
    }

    /// Resizes the pty window; the kernel forwards SIGWINCH to the pty's
    /// foreground process group (no signal handling needed on this side).
    pub(crate) fn set_winsize(&self, rows: u16, cols: u16) -> Result<(), PtyError> {
        self.io.get_ref().set_winsize(rows, cols)
    }

    fn take_close_request(&self) -> CloseRequest {
        self.close_request.lock().expect("terminal close").clone()
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

fn terminal_registry() -> &'static StdMutex<Vec<Arc<TerminalHandle>>> {
    static REGISTRY: OnceLock<StdMutex<Vec<Arc<TerminalHandle>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| StdMutex::new(Vec::new()))
}

/// Looks up a live terminal by id.
pub(crate) fn live_terminal(terminal_id: &str) -> Option<Arc<TerminalHandle>> {
    terminal_registry()
        .lock()
        .expect("terminal registry")
        .iter()
        .find(|handle| handle.terminal_id == terminal_id)
        .cloned()
}

fn register_live(handle: Arc<TerminalHandle>) {
    terminal_registry()
        .lock()
        .expect("terminal registry")
        .push(handle);
}

/// Number of live terminals (including exited ones whose ring is still
/// retained for observers).
pub(crate) fn live_terminal_count() -> usize {
    terminal_registry().lock().expect("terminal registry").len()
}

// ---------------------------------------------------------------------------
// Spawn + supervision
// ---------------------------------------------------------------------------

/// Everything the caller needs to run the domain operation.
pub(crate) struct SpawnedTerminal {
    pub handle: Arc<TerminalHandle>,
    pub child: Child,
}

/// Spawns the interactive shell on a fresh pty pair. Mirrors the TS baseline
/// (`ui/server/index.js` /shell spawn): `bash`, the resolved project
/// directory as cwd, `TERM=xterm-256color`, `COLORTERM=truecolor`,
/// `FORCE_COLOR=3` on top of the inherited environment. The pre_exec pair is
/// installed in the contract order — controlling terminal first
/// (setsid + TIOCSCTTY, so job control and ISIG work on the slave), signal
/// reset last (its own documented ordering rule). `process_group(0)` must
/// NOT be added: the setsid'd child is already its group leader and
/// setpgid would fail with EPERM (`pilotdeck-child-signals` lib.rs warning).
pub(crate) fn spawn_shell(
    terminal_id: &str,
    cwd: &Path,
    rows: u16,
    cols: u16,
) -> Result<SpawnedTerminal, PtyError> {
    let (master, slave) = PtyPair::open()?.into_master_slave();
    master.set_winsize(rows, cols)?;
    master.set_nonblocking(true)?;
    let slave_fd = slave.as_fd().as_raw_fd();
    let slave_stdout = slave.try_clone()?;
    let slave_stderr = slave.try_clone()?;

    let mut command = Command::new("bash");
    command
        .stdin(StdStdio::from(slave))
        .stdout(StdStdio::from(slave_stdout))
        .stderr(StdStdio::from(slave_stderr))
        .current_dir(cwd)
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .env("FORCE_COLOR", "3");
    pilotdeck_child_signals::install_child_controlling_terminal(command.as_std_mut(), slave_fd);
    pilotdeck_child_signals::install_child_signal_reset(command.as_std_mut());

    let child = command.spawn().map_err(PtyError::Io)?;
    let handle = Arc::new(TerminalHandle {
        terminal_id: terminal_id.to_owned(),
        io: AsyncFd::new(master).map_err(PtyError::Io)?,
        ring: StdMutex::new(OutputRing::new()),
        notify: Arc::new(Notify::new()),
        write_lock: tokio::sync::Mutex::new(()),
        input_closed: std::sync::atomic::AtomicBool::new(false),
        exit: StdMutex::new(None),
        eof_seen: std::sync::atomic::AtomicBool::new(false),
        close_request: StdMutex::new(CloseRequest::None),
    });
    register_live(handle.clone());
    Ok(SpawnedTerminal { handle, child })
}

/// Runs the terminal's post-spawn lifecycle: the reader task drains the
/// master into the ring; the supervisor observes the leader without reaping
/// it (WNOWAIT) so the PGID cannot be reused under a pending group signal,
/// executes the close chain on request, reaps, waits for the master EOF, and
/// reports the exit through `on_exit`. `on_exit` performs the durable
/// finalization (SQLite row + `terminals.changed`).
pub(crate) fn supervise(
    handle: Arc<TerminalHandle>,
    mut child: Child,
    on_exit: impl FnOnce(ExitFacts) + Send + 'static,
) {
    spawn_reader(handle.clone());
    tokio::spawn(async move {
        // The setsid'd child is its own session and group leader, so its
        // PID is the PGID of everything in the terminal's session.
        let pgid: Option<Pid> = child.id().and_then(|pid| Pid::from_raw(pid as i32));
        loop {
            let request = handle.take_close_request();
            match request {
                CloseRequest::Force => {
                    crate::process_group::kill(pgid, Signal::KILL);
                }
                CloseRequest::Graceful { deadline } => {
                    crate::process_group::kill(pgid, Signal::HUP);
                    if std::time::Instant::now() >= deadline {
                        crate::process_group::kill(pgid, Signal::KILL);
                    }
                }
                CloseRequest::None => {}
            }
            if pgid.is_none_or(leader_exited) {
                break;
            }
            tokio::time::sleep(SUPERVISOR_POLL).await;
        }
        // Every group signal that will ever be sent for this terminal has
        // been sent: only now may the leader be reaped (its PGID stops
        // being reused once it is a zombie, so the wait above was the
        // guarded window).
        let status = child.wait().await;
        // Give the reader a bounded moment to observe EOF and drain the
        // ring's tail, so the exit frame follows the last retained bytes.
        let eof_deadline = std::time::Instant::now()
            .checked_add(EOF_DRAIN_GRACE)
            .unwrap_or_else(std::time::Instant::now);
        while !handle.eof_seen() && std::time::Instant::now() < eof_deadline {
            tokio::time::sleep(SUPERVISOR_POLL).await;
        }
        let facts = match status {
            Ok(status) => ExitFacts {
                exit_code: status.code(),
                exit_signal: status.signal().and_then(signal_name),
            },
            Err(_) => ExitFacts {
                exit_code: None,
                exit_signal: None,
            },
        };
        handle.record_exit(facts.clone());
        on_exit(facts);
    });
}

fn spawn_reader(handle: Arc<TerminalHandle>) {
    tokio::spawn(async move {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let mut ready = handle.io.readable().await;
            match ready.get_ref().read(&mut buf) {
                // The master's EOF: every slave descriptor is closed (the
                // shell and its children are gone).
                Ok(0) => {
                    handle.mark_eof();
                    break;
                }
                Ok(n) => {
                    ready.clear_ready();
                    handle.push_output(&buf[..n]);
                }
                Err(PtyError::Io(e)) if e.kind() == ErrorKind::WouldBlock => {
                    ready.clear_ready();
                }
                Err(_) => {
                    handle.mark_eof();
                    break;
                }
            }
        }
    });
}

/// The signal half of the exit report (`exitSignal`), aligned with the
/// grep engine's signal-name table precedent (`builtin/grep.rs`): the
/// signals a shell session plausibly dies of by name, anything else by
/// number.
fn signal_name(raw: i32) -> Option<String> {
    let name = match raw {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        6 => "SIGABRT",
        8 => "SIGFPE",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        other => return Some(format!("SIG{other}")),
    };
    Some(name.to_owned())
}

/// WNOWAIT leader observation, keyed on the terminal's own group leader
/// (`process_group.rs`'s discipline: the leader stays unreaped — its PGID
/// cannot be reused — until every group signal has been sent).
fn leader_exited(pgid: Pid) -> bool {
    crate::process_group::leader_exited_unreaped(pgid).unwrap_or(false)
}
