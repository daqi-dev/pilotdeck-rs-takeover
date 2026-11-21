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
use std::fmt;
use std::io::ErrorKind;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Stdio as StdStdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use agentcpm_pd_pty::{Master, PtyError, PtyPair};
use rustix::process::{Pid, Signal};
use std::os::fd::{AsFd, AsRawFd};
use tokio::io::unix::AsyncFd;
use tokio::process::{Child, Command};
use tokio::sync::{Notify, watch};

/// Retained output window per terminal. The contract's byte-cursor + gap
/// semantics replace the TS baseline's 5000-chunk reconnect buffer
/// (`terminalSessionRegistry.js`); 4 MiB keeps a full screenful of scrollback
/// replayable after a detach.
pub(crate) const OUTPUT_RING_BYTES: usize = 4 * 1024 * 1024;

/// Max raw bytes per delivered output frame: keeps the serialized base64
/// `sub.event` (4/3 expansion + envelope overhead) comfortably under the
/// 1 MiB wire frame cap.
pub(crate) const OUTPUT_FRAME_BYTES: usize = 512 * 1024;

/// Bounded retention for exited handles in the live registry so late observers
/// can drain final output without leaking unbounded heap memory.
pub(crate) const MAX_EXITED_TERMINALS: usize = 64;

/// Aggregate cap across retained exited output rings (32 MiB).
pub(crate) const MAX_EXITED_RING_BYTES: usize = 32 * 1024 * 1024;

/// Maximum age of an exited handle before eviction to SQLite-only reads.
const EXITED_HANDLE_TTL: Duration = Duration::from_secs(300);

/// Grace period between the graceful group signal and the final SIGKILL
/// (the hooks executor's 3 s precedent).
const KILL_GRACE: Duration = Duration::from_secs(3);

/// Poll cadence of the supervisor's WNOWAIT leader check.
const SUPERVISOR_POLL: Duration = Duration::from_millis(50);

/// Bounded wait for the reader to observe the master's EOF after the child
/// is reaped, so the exit frame follows the very last retained bytes.
const EOF_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Bounded stall timeout waiting for PTY master writability without progress,
/// preventing a blocked child from holding `write_lock` indefinitely.
const INPUT_STALL_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum total duration for a single `write_input` call even while making
/// small incremental progress.
const INPUT_TOTAL_TIMEOUT: Duration = Duration::from_secs(15);

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

    fn len(&self) -> usize {
        self.buf.len()
    }

    fn shrink_to_fit(&mut self) {
        self.buf.shrink_to_fit();
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

#[derive(Debug)]
pub(crate) enum WriteInputError {
    ClosedBeforeWrite(PtyError),
    TimedOut { wrote_bytes: usize },
    FailedAfterPartialWrite { wrote_bytes: usize, error: PtyError },
    FailedCleanly(PtyError),
}

impl fmt::Display for WriteInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClosedBeforeWrite(e) => write!(f, "terminal input closed: {e}"),
            Self::TimedOut { wrote_bytes } => {
                write!(
                    f,
                    "terminal input timed out after writing {wrote_bytes} bytes"
                )
            }
            Self::FailedAfterPartialWrite { wrote_bytes, error } => {
                write!(
                    f,
                    "terminal input failed after writing {wrote_bytes} bytes: {error}"
                )
            }
            Self::FailedCleanly(e) => write!(f, "terminal input write failed: {e}"),
        }
    }
}

pub(crate) fn is_closed_pty_error(err: &PtyError) -> bool {
    match err {
        PtyError::Io(e) => {
            e.kind() == ErrorKind::BrokenPipe
                || matches!(
                    e.raw_os_error(),
                    Some(libc::EIO | libc::ENXIO | libc::EPIPE | libc::EBADF)
                )
        }
    }
}

pub(crate) struct TerminalHandle {
    pub terminal_id: String,
    io: StdMutex<Option<Arc<AsyncFd<Master>>>>,
    ring: StdMutex<OutputRing>,
    /// Wakes on: output appended, EOF observed, exit recorded, close
    /// requested. The `terminals.output` stream source polls on this.
    pub notify: Arc<Notify>,
    /// Serializes master writes (the contract's single-writer lane).
    pub write_lock: tokio::sync::Mutex<()>,
    /// Serializes `terminals.resize` calls so DB and PTY kernel stay ordered.
    pub resize_lock: tokio::sync::Mutex<()>,
    /// Once true, `terminals.input` answers TERMINAL_CLOSED.
    input_closed: AtomicBool,
    input_stop: watch::Sender<bool>,
    reader_stop: watch::Sender<bool>,
    leader_pgid: StdMutex<Option<Pid>>,
    hup_sent: AtomicBool,
    kill_sent: AtomicBool,
    exit: StdMutex<Option<ExitFacts>>,
    exited_at: StdMutex<Option<std::time::Instant>>,
    eof_seen: AtomicBool,
    close_request: StdMutex<CloseRequest>,
}

impl TerminalHandle {
    fn io_arc(&self) -> Option<Arc<AsyncFd<Master>>> {
        self.io.lock().expect("terminal io").clone()
    }

    #[cfg(test)]
    pub(crate) fn io_is_closed(&self) -> bool {
        self.io.lock().expect("terminal io").is_none()
    }

    pub(crate) fn leader_pgid(&self) -> Option<Pid> {
        *self.leader_pgid.lock().expect("terminal pgid")
    }

    fn clear_leader_pgid(&self) {
        *self.leader_pgid.lock().expect("terminal pgid") = None;
    }

    pub(crate) fn foreground_pgid(&self) -> Option<Pid> {
        let io = self.io_arc()?;
        rustix::termios::tcgetpgrp(io.get_ref()).ok()
    }

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

    fn ring_byte_len(&self) -> usize {
        self.ring.lock().expect("terminal ring").len()
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

    fn exited_at(&self) -> Option<std::time::Instant> {
        *self.exited_at.lock().expect("terminal exited_at")
    }

    /// Whether the master has observed EOF (all slave descriptors closed).
    pub(crate) fn eof_seen(&self) -> bool {
        self.eof_seen.load(Ordering::SeqCst)
    }

    /// Whether `terminals.input` is still accepted.
    pub(crate) fn inputs_open(&self) -> bool {
        !self.input_closed.load(Ordering::SeqCst)
    }

    fn mark_input_closed(&self) {
        self.input_closed.store(true, Ordering::SeqCst);
        let _ = self.input_stop.send(true);
    }

    fn record_exit(&self, facts: ExitFacts) {
        self.mark_input_closed();
        self.ring.lock().expect("terminal ring").shrink_to_fit();
        *self.exit.lock().expect("terminal exit") = Some(facts);
        *self.exited_at.lock().expect("terminal exited_at") = Some(std::time::Instant::now());
        self.notify.notify_waiters();
    }

    fn mark_eof(&self) {
        self.eof_seen.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    fn signal_session(&self, signal: Signal) {
        if signal == Signal::HUP && self.hup_sent.swap(true, Ordering::SeqCst) {
            return;
        }
        if signal == Signal::KILL && self.kill_sent.swap(true, Ordering::SeqCst) {
            return;
        }
        let leader = self.leader_pgid();
        if leader.is_none() {
            return;
        }
        let fg = self.foreground_pgid();
        if let Some(fg_pgid) = fg
            && Some(fg_pgid) != leader
        {
            crate::process_group::kill(Some(fg_pgid), signal);
        }
        crate::process_group::kill(leader, signal);
    }

    async fn close_master_and_stop_reader(&self, reader_handle: &mut tokio::task::JoinHandle<()>) {
        self.mark_input_closed();
        let _ = self.reader_stop.send(true);
        let dropped_io = self.io.lock().expect("terminal io").take();
        drop(dropped_io);
        if tokio::time::timeout(Duration::from_millis(250), &mut *reader_handle)
            .await
            .is_err()
        {
            reader_handle.abort();
            let _ = (&mut *reader_handle).await;
        }
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

    /// Writes terminal input to the master with bounded progress timeout,
    /// close-signal cancellation, and EINTR retry.
    pub(crate) async fn write_input(&self, bytes: &[u8]) -> Result<(), WriteInputError> {
        let mut stop_rx = self.input_stop.subscribe();
        if *stop_rx.borrow() || !self.inputs_open() {
            return Err(WriteInputError::ClosedBeforeWrite(PtyError::Io(
                std::io::Error::new(ErrorKind::BrokenPipe, "terminal input closed"),
            )));
        }
        let Some(io) = self.io_arc() else {
            return Err(WriteInputError::ClosedBeforeWrite(PtyError::Io(
                std::io::Error::from_raw_os_error(libc::EIO),
            )));
        };
        if bytes.is_empty() {
            return Ok(());
        }

        let mut wrote = 0usize;
        let overall_deadline = tokio::time::Instant::now() + INPUT_TOTAL_TIMEOUT;
        let mut last_progress = tokio::time::Instant::now();

        while wrote < bytes.len() {
            let step_deadline = (last_progress + INPUT_STALL_TIMEOUT).min(overall_deadline);
            let mut ready = tokio::select! {
                biased;
                _ = stop_rx.changed() => {
                    return Err(if wrote == 0 {
                        WriteInputError::ClosedBeforeWrite(PtyError::Io(std::io::Error::new(
                            ErrorKind::BrokenPipe,
                            "terminal input closed",
                        )))
                    } else {
                        WriteInputError::FailedAfterPartialWrite {
                            wrote_bytes: wrote,
                            error: PtyError::Io(std::io::Error::new(
                                ErrorKind::BrokenPipe,
                                "terminal input closed during write",
                            )),
                        }
                    });
                }
                res = io.writable() => match res {
                    Ok(guard) => guard,
                    Err(e) => {
                        let pty_err = PtyError::Io(e);
                        return Err(if wrote == 0 {
                            WriteInputError::ClosedBeforeWrite(pty_err)
                        } else {
                            WriteInputError::FailedAfterPartialWrite {
                                wrote_bytes: wrote,
                                error: pty_err,
                            }
                        });
                    }
                },
                _ = tokio::time::sleep_until(step_deadline) => {
                    return Err(WriteInputError::TimedOut { wrote_bytes: wrote });
                }
            };

            loop {
                match ready.get_inner().write(&bytes[wrote..]) {
                    Ok(0) => {
                        let err =
                            PtyError::Io(std::io::Error::other("pty master write returned 0"));
                        return Err(if wrote == 0 {
                            WriteInputError::FailedCleanly(err)
                        } else {
                            WriteInputError::FailedAfterPartialWrite {
                                wrote_bytes: wrote,
                                error: err,
                            }
                        });
                    }
                    Ok(n) => {
                        wrote += n;
                        last_progress = tokio::time::Instant::now();
                        break;
                    }
                    Err(PtyError::Io(e)) if e.kind() == ErrorKind::WouldBlock => {
                        ready.clear_ready();
                        break;
                    }
                    Err(PtyError::Io(e)) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(other) => {
                        return Err(if wrote > 0 {
                            WriteInputError::FailedAfterPartialWrite {
                                wrote_bytes: wrote,
                                error: other,
                            }
                        } else if is_closed_pty_error(&other) {
                            WriteInputError::ClosedBeforeWrite(other)
                        } else {
                            WriteInputError::FailedCleanly(other)
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Resizes the pty window; the kernel forwards SIGWINCH to the pty's
    /// foreground process group (no signal handling needed on this side).
    pub(crate) fn set_winsize(&self, rows: u16, cols: u16) -> Result<(), PtyError> {
        let Some(io) = self.io_arc() else {
            return Err(PtyError::Io(std::io::Error::from_raw_os_error(libc::EIO)));
        };
        io.get_ref().set_winsize(rows, cols)
    }

    fn take_close_request(&self) -> CloseRequest {
        self.close_request.lock().expect("terminal close").clone()
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RegistryState {
    handles: Vec<Arc<TerminalHandle>>,
    reserved_slots: usize,
}

#[derive(Clone, Default)]
pub(crate) struct TerminalRegistry {
    inner: Arc<StdMutex<RegistryState>>,
}

fn prune_exited_handles_locked(state: &mut RegistryState) {
    let now = std::time::Instant::now();
    state.handles.retain(|handle| {
        let Some(exited_at) = handle.exited_at() else {
            return true;
        };
        now.saturating_duration_since(exited_at) < EXITED_HANDLE_TTL
    });

    loop {
        let mut exited: Vec<(usize, std::time::Instant, usize)> = state
            .handles
            .iter()
            .enumerate()
            .filter_map(|(idx, h)| h.exited_at().map(|at| (idx, at, h.ring_byte_len())))
            .collect();
        let total_ring: usize = exited.iter().map(|(_, _, len)| *len).sum();
        if exited.len() <= MAX_EXITED_TERMINALS && total_ring <= MAX_EXITED_RING_BYTES {
            break;
        }
        exited.sort_by_key(|&(_, at, _)| at);
        let Some(&(victim_idx, _, _)) = exited.first() else {
            break;
        };
        state.handles.remove(victim_idx);
    }
}

pub(crate) struct LiveSlotGuard {
    registry: Arc<StdMutex<RegistryState>>,
    committed: bool,
}

impl LiveSlotGuard {
    pub(crate) fn commit(mut self, handle: Arc<TerminalHandle>) {
        let mut state = self.registry.lock().expect("terminal registry");
        prune_exited_handles_locked(&mut state);
        state.reserved_slots = state.reserved_slots.saturating_sub(1);
        state.handles.push(handle);
        self.committed = true;
    }
}

impl Drop for LiveSlotGuard {
    fn drop(&mut self) {
        if !self.committed {
            let mut state = self.registry.lock().expect("terminal registry");
            state.reserved_slots = state.reserved_slots.saturating_sub(1);
        }
    }
}

impl TerminalRegistry {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn prune(&self) {
        let mut state = self.inner.lock().expect("terminal registry");
        prune_exited_handles_locked(&mut state);
    }

    /// Looks up a live (or retained exited) terminal by id.
    pub(crate) fn live_terminal(&self, terminal_id: &str) -> Option<Arc<TerminalHandle>> {
        self.prune();
        self.inner
            .lock()
            .expect("terminal registry")
            .handles
            .iter()
            .find(|handle| handle.terminal_id == terminal_id)
            .cloned()
    }

    pub(crate) fn try_reserve_live_slot(&self, max_live: usize) -> Option<LiveSlotGuard> {
        let mut state = self.inner.lock().expect("terminal registry");
        prune_exited_handles_locked(&mut state);
        let live = state
            .handles
            .iter()
            .filter(|handle| handle.exit_facts().is_none())
            .count()
            + state.reserved_slots;
        if live >= max_live {
            return None;
        }
        state.reserved_slots += 1;
        Some(LiveSlotGuard {
            registry: self.inner.clone(),
            committed: false,
        })
    }

    /// Number of live (not yet exited) terminals plus in-flight reserved slots.
    #[cfg(test)]
    pub(crate) fn live_terminal_count(&self) -> usize {
        let state = self.inner.lock().expect("terminal registry");
        state
            .handles
            .iter()
            .filter(|handle| handle.exit_facts().is_none())
            .count()
            + state.reserved_slots
    }

    pub(crate) fn close_all_live_terminals(&self, graceful: bool) {
        let handles: Vec<Arc<TerminalHandle>> = self
            .inner
            .lock()
            .expect("terminal registry")
            .handles
            .iter()
            .filter(|handle| handle.exit_facts().is_none())
            .cloned()
            .collect();
        for handle in handles {
            handle.request_close(graceful);
            if !graceful {
                handle.signal_session(Signal::KILL);
            }
        }
    }

    #[cfg(test)]
    fn handle_len(&self) -> usize {
        self.inner.lock().expect("terminal registry").handles.len()
    }

    #[cfg(test)]
    fn clear_handles(&self) {
        self.inner
            .lock()
            .expect("terminal registry")
            .handles
            .clear();
    }

    #[cfg(test)]
    fn total_ring_bytes(&self) -> usize {
        self.inner
            .lock()
            .expect("terminal registry")
            .handles
            .iter()
            .map(|h| h.ring_byte_len())
            .sum()
    }
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
    if let Err(error) =
        pilotdeck_child_signals::install_child_controlling_terminal(command.as_std_mut(), slave_fd)
    {
        return Err(PtyError::Io(std::io::Error::other(format!(
            "install_child_controlling_terminal failed: {error}"
        ))));
    }
    pilotdeck_child_signals::install_child_signal_reset(command.as_std_mut());

    let mut child = command.spawn().map_err(PtyError::Io)?;
    let pgid = child.id().and_then(|pid| Pid::from_raw(pid as i32));
    let mut spawn_guard = crate::process_group::GroupGuard(pgid);
    let io = match AsyncFd::new(master) {
        Ok(io) => io,
        Err(e) => {
            spawn_guard.signal(Signal::KILL);
            spawn_guard.disarm();
            let _ = child.start_kill();
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                rt.spawn(async move {
                    let _ = child.wait().await;
                });
            }
            return Err(PtyError::Io(e));
        }
    };
    spawn_guard.disarm();

    let (input_stop, _) = watch::channel(false);
    let (reader_stop, _) = watch::channel(false);
    let handle = Arc::new(TerminalHandle {
        terminal_id: terminal_id.to_owned(),
        io: StdMutex::new(Some(Arc::new(io))),
        ring: StdMutex::new(OutputRing::new()),
        notify: Arc::new(Notify::new()),
        write_lock: tokio::sync::Mutex::new(()),
        resize_lock: tokio::sync::Mutex::new(()),
        input_closed: AtomicBool::new(false),
        input_stop,
        reader_stop,
        leader_pgid: StdMutex::new(pgid),
        hup_sent: AtomicBool::new(false),
        kill_sent: AtomicBool::new(false),
        exit: StdMutex::new(None),
        exited_at: StdMutex::new(None),
        eof_seen: AtomicBool::new(false),
        close_request: StdMutex::new(CloseRequest::None),
    });
    Ok(SpawnedTerminal { handle, child })
}

/// Cleans up a spawned but not-yet-committed terminal (e.g. DB error or
/// idempotent replay race) so neither process group, child, nor master fd leaks.
pub(crate) async fn abort_spawned(mut spawned: SpawnedTerminal) {
    spawned.handle.mark_input_closed();
    spawned.handle.signal_session(Signal::KILL);
    drop(spawned.handle.io.lock().expect("terminal io").take());
    let _ = spawned.child.kill().await;
    let _ = spawned.child.wait().await;
    spawned.handle.clear_leader_pgid();
}

/// Runs the terminal's post-spawn lifecycle: the reader task drains the
/// master into the ring; the supervisor observes the leader without reaping
/// it (WNOWAIT) so the PGID cannot be reused under a pending group signal,
/// executes the close chain on request, reaps, waits for the master EOF,
/// closes the master fd + stops the reader, and reports the exit through
/// `on_exit`. `on_exit` performs the durable finalization (SQLite row +
/// `terminals.changed`).
pub(crate) fn supervise(
    registry: TerminalRegistry,
    handle: Arc<TerminalHandle>,
    mut child: Child,
    on_exit: impl FnOnce(ExitFacts) + Send + 'static,
) {
    let mut reader_handle = spawn_reader(handle.clone());
    tokio::spawn(async move {
        // The setsid'd child is its own session and group leader, so its
        // PID is the PGID of the shell; foreground jobs may sit in a separate
        // foreground PGID discovered via `tcgetpgrp(master)`.
        let pgid: Option<Pid> = handle.leader_pgid();
        let mut group_guard = crate::process_group::GroupGuard(pgid);
        let mut reaped_status: Option<std::io::Result<std::process::ExitStatus>> = None;

        loop {
            let notified = handle.notify.notified();
            tokio::pin!(notified);

            let request = handle.take_close_request();
            match request {
                CloseRequest::Force => {
                    handle.signal_session(Signal::KILL);
                }
                CloseRequest::Graceful { deadline } => {
                    handle.signal_session(Signal::HUP);
                    if std::time::Instant::now() >= deadline {
                        handle.signal_session(Signal::KILL);
                    }
                }
                CloseRequest::None => {}
            }
            if let Some(status_opt) = poll_leader_exit(&mut child, pgid) {
                reaped_status = status_opt;
                break;
            }
            let sleep_dur = match request {
                CloseRequest::Graceful { deadline } => deadline
                    .saturating_duration_since(std::time::Instant::now())
                    .min(SUPERVISOR_POLL),
                _ => SUPERVISOR_POLL,
            };
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep(sleep_dur) => {}
            }
        }
        // Every group signal that will ever be sent for this terminal has
        // been sent: only now may the leader be reaped (its PGID stops
        // being reused once it is a zombie, so the wait above was the
        // guarded window).
        let status = match reaped_status {
            Some(status) => status,
            None => child.wait().await,
        };
        handle.clear_leader_pgid();
        group_guard.disarm();

        // Give the reader a bounded moment to observe EOF and drain the
        // ring's tail, so the exit frame follows the last retained bytes.
        let eof_deadline = tokio::time::Instant::now() + EOF_DRAIN_GRACE;
        while !handle.eof_seen() && tokio::time::Instant::now() < eof_deadline {
            let notified = handle.notify.notified();
            tokio::pin!(notified);
            if handle.eof_seen() {
                break;
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep_until(eof_deadline) => {}
            }
        }
        handle
            .close_master_and_stop_reader(&mut reader_handle)
            .await;

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
        registry.prune();
        on_exit(facts);
    });
}

fn spawn_reader(handle: Arc<TerminalHandle>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut stop_rx = handle.reader_stop.subscribe();
        let Some(io) = handle.io_arc() else {
            handle.mark_eof();
            return;
        };
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            if *stop_rx.borrow() {
                break;
            }
            let mut ready = tokio::select! {
                biased;
                _ = stop_rx.changed() => break,
                res = io.readable() => match res {
                    Ok(guard) => guard,
                    Err(_) => {
                        handle.mark_eof();
                        break;
                    }
                },
            };
            let mut reads_in_turn = 0usize;
            let (eof_or_err, hit_slice) = loop {
                match ready.get_inner().read(&mut buf) {
                    // The master's EOF: every slave descriptor is closed (the
                    // shell and its children are gone).
                    Ok(0) => break (true, false),
                    Ok(n) => {
                        // Do NOT clear readiness on Ok(n): Tokio's edge-triggered
                        // AsyncFd only clears readiness after WouldBlock.
                        handle.push_output(&buf[..n]);
                        reads_in_turn += 1;
                        if reads_in_turn >= 16 {
                            break (false, true);
                        }
                    }
                    Err(PtyError::Io(e)) if e.kind() == ErrorKind::WouldBlock => {
                        ready.clear_ready();
                        break (false, false);
                    }
                    Err(PtyError::Io(e)) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => break (true, false),
                }
            };
            drop(ready);
            if eof_or_err {
                handle.mark_eof();
                break;
            }
            if hit_slice {
                tokio::task::yield_now().await;
            }
        }
        drop(io);
    })
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

/// Observes leader exit without reaping while WNOWAIT is supported, falling
/// back to `try_wait` on no-WNOWAIT platforms (or `ENOSYS`) and treating
/// `ECHILD` as already exited so the supervisor never loops forever.
fn poll_leader_exit(
    child: &mut Child,
    pgid: Option<Pid>,
) -> Option<Option<std::io::Result<std::process::ExitStatus>>> {
    let Some(pgid) = pgid else {
        return Some(None);
    };
    #[cfg(not(any(
        target_os = "cygwin",
        target_os = "horizon",
        target_os = "openbsd",
        target_os = "redox",
        target_os = "wasi"
    )))]
    {
        match crate::process_group::leader_exited_unreaped(pgid) {
            Ok(true) | Err(rustix::io::Errno::CHILD) => Some(None),
            Ok(false) => None,
            Err(rustix::io::Errno::NOSYS) => match child.try_wait() {
                Ok(Some(status)) => Some(Some(Ok(status))),
                Err(e) => Some(Some(Err(e))),
                Ok(None) => None,
            },
            Err(_) => None,
        }
    }
    #[cfg(any(
        target_os = "cygwin",
        target_os = "horizon",
        target_os = "openbsd",
        target_os = "redox",
        target_os = "wasi"
    ))]
    {
        match child.try_wait() {
            Ok(Some(status)) => Some(Some(Ok(status))),
            Err(e) => Some(Some(Err(e))),
            Ok(None) => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests（真实 pty 生命周期——terminals.rs 的测试层不 spawn，合同行为的
// 端到端由 CLI e2e 覆盖；registry 记账的回归在此钉住）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::domains::automation::tests::{dispatch_ok, fixture};
    use crate::public_v1::runtime::PublicV1Runtime;
    use serde_json::{Value, json};
    use std::sync::Arc;

    static PTY_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// 建-关-建回归（t022 复审 P1-2）：`live_terminal_count` 曾把已退出的
    /// 句柄也计入在途名额——顺序建满 32 个（每个都关到 exited）后，
    /// `terminals.create` 对本实例永久 RESOURCE_LIMIT（当前 0 个在跑）。
    /// 修复后 exited 句柄保留在 registry（迟到观察者仍能重放 ring / 读
    /// exit facts），但不再占名额：33 轮建-关之后再建，全程不触限。
    #[tokio::test]
    async fn create_close_cycles_do_not_ratchet_the_live_terminal_cap() {
        let _guard = PTY_TEST_LOCK.lock().await;
        let fx = fixture("pty-cap").await;
        fx.rt.terminal_registry().clear_handles();
        // 项目 + 已声明根（terminals.create 解析工作目录；无 git 依赖）。
        let declared_root = fx._tmp.0.join("root-pty-cap");
        std::fs::create_dir_all(declared_root.join("ws")).unwrap();
        fx.rt
            .store()
            .declare_authorized_root("root-pty-cap", &declared_root)
            .unwrap();
        fx.rt
            .store()
            .register_project(&json!({
                "instanceId": fx.instance_id,
                "projectId": "proj-pty-cap",
                "name": "Pty Cap Fixture",
                "environmentId": "local",
                "root": {"instanceId": fx.instance_id, "rootId": "root-pty-cap",
                         "relativePath": "ws"},
            }))
            .unwrap();

        // 33 轮建-关。修复前：第 33 次 create 撞上 32 个已退出句柄的陈旧
        // 计数 → RESOURCE_LIMIT；修复后：exited 不计数，全程成功。
        for round in 0..33 {
            let created = create_terminal(&fx.rt, round).await;
            let terminal_id = created["terminalId"].as_str().unwrap().to_owned();
            let server_epoch = created["serverEpoch"].as_str().unwrap().to_owned();
            // force 关闭：kill 链立即 SIGKILL，收敛不依赖 shell 的信号处置。
            close_terminal(&fx.rt, &terminal_id, &server_epoch, round).await;
            wait_terminal_exited(&fx.rt, &terminal_id, &format!("round {round}")).await;
            let handle = fx
                .rt
                .terminal_registry()
                .live_terminal(&terminal_id)
                .expect("exited handle retained");
            assert!(
                handle.io_is_closed(),
                "round {round}: exited terminal must close its master fd"
            );
            assert!(
                !handle.inputs_open(),
                "round {round}: exited terminal must close input acceptance"
            );
        }

        // 名额随退出回收：再建一个必须成功，且在途计数是 1 而非 33+。
        let created = create_terminal(&fx.rt, 33).await;
        assert_eq!(created["state"], "running");
        assert_eq!(
            fx.rt.terminal_registry().live_terminal_count(),
            1,
            "只有最后一个未退出终端计入在途名额"
        );
        // exited 句柄仍保留在 registry（在 MAX_EXITED_TERMINALS=64 上限内）。
        assert_eq!(
            fx.rt.terminal_registry().handle_len(),
            34,
            "33 个已退出句柄 + 1 个在跑，全部保留"
        );

        // 收尾：关掉最后一个终端，不给测试进程遗留 bash。
        let terminal_id = created["terminalId"].as_str().unwrap().to_owned();
        let server_epoch = created["serverEpoch"].as_str().unwrap().to_owned();
        close_terminal(&fx.rt, &terminal_id, &server_epoch, 33).await;
        wait_terminal_exited(&fx.rt, &terminal_id, "cleanup").await;
        assert_eq!(fx.rt.terminal_registry().live_terminal_count(), 0);
    }

    #[tokio::test]
    async fn spawn_reader_drains_over_64k_burst_without_dropping_readiness() {
        let _guard = PTY_TEST_LOCK.lock().await;
        let tmp = crate::public_v1::test_support::TempDir::new("pty-burst");
        let reg = TerminalRegistry::new();
        let slot = reg.try_reserve_live_slot(64).expect("slot");
        let spawned = spawn_shell("term-burst", &tmp.0, 24, 80).expect("spawn shell");
        let handle = spawned.handle.clone();
        slot.commit(handle.clone());
        let exited_flag = Arc::new(AtomicBool::new(false));
        let exited_done = exited_flag.clone();
        supervise(reg, handle.clone(), spawned.child, move |_| {
            exited_done.store(true, Ordering::SeqCst);
        });

        handle
            .write_input(b"head -c 196608 /dev/zero | tr '\\0' 'A'; echo __BURST_DONE__\n")
            .await
            .expect("write burst command");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let marker = b"__BURST_DONE__";
        loop {
            let bytes = handle.copy_output(0, 512 * 1024).unwrap_or_default();
            let a_count = bytes.iter().filter(|&&b| b == b'A').count();
            if a_count >= 196_608 && bytes.windows(marker.len()).any(|w| w == marker) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "reader stalled draining >64KiB burst (saw {} bytes, {} 'A's)",
                bytes.len(),
                a_count
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        handle.request_close(false);
        let exit_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !exited_flag.load(Ordering::SeqCst) {
            assert!(std::time::Instant::now() < exit_deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(handle.io_is_closed());
        assert!(!handle.inputs_open());
    }

    #[tokio::test]
    async fn exited_handles_evict_above_cap_and_ring_budget() {
        let _guard = PTY_TEST_LOCK.lock().await;
        let reg = TerminalRegistry::new();

        // Seed MAX_EXITED_TERMINALS + 10 exited handles without spawning shells.
        for i in 0..(MAX_EXITED_TERMINALS + 10) {
            let (master, _slave) = PtyPair::open().unwrap().into_master_slave();
            master.set_nonblocking(true).unwrap();
            let (input_stop, _) = watch::channel(true);
            let (reader_stop, _) = watch::channel(true);
            let handle = Arc::new(TerminalHandle {
                terminal_id: format!("evict-cap-{i}"),
                io: StdMutex::new(Some(Arc::new(AsyncFd::new(master).unwrap()))),
                ring: StdMutex::new(OutputRing::new()),
                notify: Arc::new(Notify::new()),
                write_lock: tokio::sync::Mutex::new(()),
                resize_lock: tokio::sync::Mutex::new(()),
                input_closed: AtomicBool::new(true),
                input_stop,
                reader_stop,
                leader_pgid: StdMutex::new(None),
                hup_sent: AtomicBool::new(false),
                kill_sent: AtomicBool::new(false),
                exit: StdMutex::new(None),
                exited_at: StdMutex::new(None),
                eof_seen: AtomicBool::new(true),
                close_request: StdMutex::new(CloseRequest::None),
            });
            drop(handle.io.lock().unwrap().take());
            handle.record_exit(ExitFacts {
                exit_code: Some(0),
                exit_signal: None,
            });
            let slot = reg.try_reserve_live_slot(128).unwrap();
            slot.commit(handle);
        }
        assert_eq!(reg.handle_len(), MAX_EXITED_TERMINALS);
        assert!(reg.live_terminal("evict-cap-0").is_none());
        assert!(
            reg.live_terminal(&format!("evict-cap-{}", MAX_EXITED_TERMINALS + 9))
                .is_some()
        );

        // Now verify aggregate ring memory budget (MAX_EXITED_RING_BYTES = 32 MiB):
        // 12 handles x 4 MiB = 48 MiB must prune down to <= 8 handles (<= 32 MiB).
        reg.clear_handles();
        let big_chunk = vec![b'Z'; OUTPUT_RING_BYTES];
        for i in 0..12 {
            let (master, _slave) = PtyPair::open().unwrap().into_master_slave();
            master.set_nonblocking(true).unwrap();
            let (input_stop, _) = watch::channel(true);
            let (reader_stop, _) = watch::channel(true);
            let handle = Arc::new(TerminalHandle {
                terminal_id: format!("evict-mem-{i}"),
                io: StdMutex::new(Some(Arc::new(AsyncFd::new(master).unwrap()))),
                ring: StdMutex::new(OutputRing::new()),
                notify: Arc::new(Notify::new()),
                write_lock: tokio::sync::Mutex::new(()),
                resize_lock: tokio::sync::Mutex::new(()),
                input_closed: AtomicBool::new(true),
                input_stop,
                reader_stop,
                leader_pgid: StdMutex::new(None),
                hup_sent: AtomicBool::new(false),
                kill_sent: AtomicBool::new(false),
                exit: StdMutex::new(None),
                exited_at: StdMutex::new(None),
                eof_seen: AtomicBool::new(true),
                close_request: StdMutex::new(CloseRequest::None),
            });
            handle.push_output(&big_chunk);
            drop(handle.io.lock().unwrap().take());
            handle.record_exit(ExitFacts {
                exit_code: Some(0),
                exit_signal: None,
            });
            let slot = reg.try_reserve_live_slot(128).unwrap();
            slot.commit(handle);
        }
        let remaining_bytes = reg.total_ring_bytes();
        assert!(
            remaining_bytes <= MAX_EXITED_RING_BYTES,
            "remaining exited ring bytes {remaining_bytes} exceeded cap {MAX_EXITED_RING_BYTES}"
        );
        assert!(reg.live_terminal("evict-mem-0").is_none());
        assert!(reg.live_terminal("evict-mem-11").is_some());
    }

    #[tokio::test]
    async fn force_close_kills_interactive_bash_foreground_job_and_closes_master() {
        let _guard = PTY_TEST_LOCK.lock().await;
        let tmp = crate::public_v1::test_support::TempDir::new("pty-fg-kill");
        let reg = TerminalRegistry::new();
        let slot = reg.try_reserve_live_slot(64).expect("slot");
        let spawned = spawn_shell("term-fg-kill", &tmp.0, 24, 80).expect("spawn shell");
        let handle = spawned.handle.clone();
        slot.commit(handle.clone());
        let exited_flag = Arc::new(AtomicBool::new(false));
        let exited_done = exited_flag.clone();
        supervise(reg, handle.clone(), spawned.child, move |_| {
            exited_done.store(true, Ordering::SeqCst);
        });

        let leader_pgid = handle.leader_pgid().expect("leader pgid");
        handle
            .write_input(b"set -m; sleep 300\n")
            .await
            .expect("start foreground sleep");

        // Wait until bash creates a distinct foreground process group for sleep 300.
        let fg_deadline = std::time::Instant::now() + Duration::from_secs(5);
        let fg_pgid = loop {
            if let Some(fg) = handle.foreground_pgid()
                && fg != leader_pgid
            {
                break fg;
            }
            assert!(
                std::time::Instant::now() < fg_deadline,
                "foreground job did not enter its own PGID"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        };

        handle.request_close(false);
        let exit_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !exited_flag.load(Ordering::SeqCst) {
            assert!(std::time::Instant::now() < exit_deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(handle.io_is_closed(), "master fd must close on exit");
        assert!(!handle.inputs_open(), "record_exit closes input acceptance");
        // Both bash and its foreground `sleep 300` PGID must be gone.
        let reap_deadline = std::time::Instant::now() + Duration::from_secs(2);
        while crate::process_group::group_exists(fg_pgid)
            && std::time::Instant::now() < reap_deadline
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !crate::process_group::group_exists(fg_pgid),
            "foreground job PGID {fg_pgid:?} survived terminals.close(force)"
        );
    }

    #[tokio::test]
    async fn eof_drain_grace_closes_master_even_with_background_slave_holder() {
        let _guard = PTY_TEST_LOCK.lock().await;
        let tmp = crate::public_v1::test_support::TempDir::new("pty-bg-holder");
        let reg = TerminalRegistry::new();
        let slot = reg.try_reserve_live_slot(64).expect("slot");
        let spawned = spawn_shell("term-bg-holder", &tmp.0, 24, 80).expect("spawn shell");
        let handle = spawned.handle.clone();
        slot.commit(handle.clone());
        let exited_flag = Arc::new(AtomicBool::new(false));
        let exited_done = exited_flag.clone();
        supervise(reg, handle.clone(), spawned.child, move |_| {
            exited_done.store(true, Ordering::SeqCst);
        });

        let pid_file = tmp.0.join("bg.pid");
        let cmd = format!(
            "set -m; sleep 30 & echo $! > '{}'; exit 0\n",
            pid_file.display()
        );
        handle.write_input(cmd.as_bytes()).await.expect("write cmd");

        // Supervisor should still finish after EOF_DRAIN_GRACE (2s) and close master fd,
        // even while the background sleep still holds the slave fd.
        let exit_deadline = std::time::Instant::now() + Duration::from_secs(8);
        while !exited_flag.load(Ordering::SeqCst) {
            assert!(
                std::time::Instant::now() < exit_deadline,
                "supervisor hung waiting for EOF with background slave holder"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            handle.io_is_closed(),
            "master fd must close when EOF_DRAIN_GRACE expires"
        );

        if let Ok(raw) = std::fs::read_to_string(&pid_file)
            && let Ok(bg_pid) = raw.trim().parse::<i32>()
            && let Some(pid) = Pid::from_raw(bg_pid)
        {
            crate::process_group::kill(Some(pid), Signal::KILL);
            let _ = rustix::process::kill_process(pid, Signal::KILL);
        }
    }

    #[tokio::test]
    async fn write_input_unblocks_on_close_and_eintr_or_eof() {
        let _guard = PTY_TEST_LOCK.lock().await;
        let (master, slave) = PtyPair::open().unwrap().into_master_slave();
        master.set_nonblocking(true).unwrap();
        let (input_stop, _) = watch::channel(false);
        let (reader_stop, _) = watch::channel(false);
        let handle = Arc::new(TerminalHandle {
            terminal_id: "term-write-unblock".to_owned(),
            io: StdMutex::new(Some(Arc::new(AsyncFd::new(master).unwrap()))),
            ring: StdMutex::new(OutputRing::new()),
            notify: Arc::new(Notify::new()),
            write_lock: tokio::sync::Mutex::new(()),
            resize_lock: tokio::sync::Mutex::new(()),
            input_closed: AtomicBool::new(false),
            input_stop,
            reader_stop,
            leader_pgid: StdMutex::new(None),
            hup_sent: AtomicBool::new(false),
            kill_sent: AtomicBool::new(false),
            exit: StdMutex::new(None),
            exited_at: StdMutex::new(None),
            eof_seen: AtomicBool::new(false),
            close_request: StdMutex::new(CloseRequest::None),
        });

        // Requesting close wakes and rejects new or stalled writes cleanly.
        let h2 = handle.clone();
        let writer = tokio::spawn(async move { h2.write_input(&[b'x'; 256 * 1024]).await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        handle.request_close(false);
        let res = tokio::time::timeout(Duration::from_secs(2), writer)
            .await
            .expect("write_input must unblock promptly on close")
            .expect("join writer");
        assert!(res.is_err());
        assert!(!handle.inputs_open());

        // `record_exit` also marks inputs closed immediately.
        drop(slave);
        handle.record_exit(ExitFacts {
            exit_code: Some(0),
            exit_signal: None,
        });
        assert!(!handle.inputs_open());
    }

    async fn create_terminal(rt: &Arc<PublicV1Runtime>, round: usize) -> Value {
        let result = dispatch_ok(
            rt,
            "terminals.create",
            json!({"projectId": "proj-pty-cap", "rows": 24, "columns": 80}),
            Some(&format!("pty-cap-create-{round}")),
        )
        .await;
        result["terminal"].clone()
    }

    async fn close_terminal(
        rt: &Arc<PublicV1Runtime>,
        terminal_id: &str,
        server_epoch: &str,
        round: usize,
    ) {
        dispatch_ok(
            rt,
            "terminals.close",
            json!({"terminalId": terminal_id, "expectedServerEpoch": server_epoch,
                   "mode": "force"}),
            Some(&format!("pty-cap-close-{round}")),
        )
        .await;
    }

    /// 轮询到 `exited`：`record_exit` 先于落库终态，行到 exited 时该句柄
    /// 已退出在途计数。
    async fn wait_terminal_exited(rt: &Arc<PublicV1Runtime>, terminal_id: &str, tag: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let result = dispatch_ok(
                rt,
                "terminals.get",
                json!({"terminalId": terminal_id}),
                None,
            )
            .await;
            let got = &result["terminal"];
            if got["state"] == "exited" {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{tag}: terminal `{terminal_id}` never exited (last state `{}`)",
                got["state"]
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}
