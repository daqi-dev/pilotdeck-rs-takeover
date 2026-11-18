//! PTY substrate for the terminals domain (t015 PR-2).
//!
//! Everything here rides rustix's safe APIs: `openpt`/`grantpt`/`unlockpt`/
//! `ptsname` (feature `pty`, which implies `fs`; `alloc` is ptsname's
//! buffer), `tcgetattr`/`tcsetattr`/`tcgetwinsize`/`tcsetwinsize` (feature
//! `termios`), and the plain read/write/fcntl wrappers from rustix's core
//! and `fs` modules. The crate contains no `unsafe` code — the workspace
//! `unsafe_code = "forbid"` applies unchanged. The single unavoidable
//! unsafe operation in the PTY stack is the child-side `setsid()` +
//! `TIOCSCTTY` pair that must run between fork and exec; it lives in
//! `pilotdeck-child-signals` (`install_child_controlling_terminal`) under
//! that crate's human-approved exception, not here.
//!
//! Platform notes. Linux obtains the slave fd through `TIOCGPTPEER` (no
//! ptsname race); every other supported platform resolves the path with
//! `ptsname`. Both slave paths open with `O_RDWR | O_NOCTTY | O_CLOEXEC`,
//! so the slave never leaks across exec and has no open-to-normalize
//! gap; the master's openpt flags can request close-on-exec only on
//! Linux, so on the other platforms the master is normalized through
//! `fcntl(F_DUPFD_CLOEXEC)` (rustix 1.1.5 has no safe close-on-exec
//! request for those openpt flags, leaving a small open-to-dup window
//! there). Inherited copies would keep the pty alive past the terminal's
//! death, masking the master's end-of-file and the kernel's hangup.
//! `O_NOCTTY` everywhere keeps the spawning process from acquiring the
//! pty as its own controlling terminal — the child claims it through the
//! pre_exec hook instead. On macOS, termios ioctls on the master only
//! work once the slave has been opened at least once; [`PtyPair::open`]
//! always opens the slave, so the master-side `attributes`/
//! `set_attributes` calls are safe for every pair this crate hands out.
//! Master reads normalize the end-of-file flavor: macOS reports a plain
//! EOF, Linux reports `EIO` once the last slave descriptor is closed,
//! and both surface as `Ok(0)`.
#![cfg(unix)]

use std::{
    fmt, io,
    os::fd::{AsFd, OwnedFd},
    os::unix::ffi::OsStringExt,
    path::PathBuf,
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

use rustix::{fs, pty, termios};

/// Sleep bounds for the nonblocking retry loop in [`Master::read_timeout`]:
/// each pause is a fraction of the remaining window, clamped between a 1 ms
/// floor (short windows stay responsive) and a 10 ms ceiling (long windows
/// do not spin the CPU). The substrate deliberately stays on the
/// pty/termios feature set (no poll or epoll).
const READ_TIMEOUT_POLL_FLOOR: Duration = Duration::from_millis(1);
const READ_TIMEOUT_POLL_CEILING: Duration = Duration::from_millis(10);

/// Errors of the PTY substrate. Hand-written instead of thiserror so the
/// workspace dependency set stays unchanged.
#[derive(Debug)]
pub enum PtyError {
    /// The OS error of a rustix syscall wrapper (openpt, grantpt, unlockpt,
    /// ptsname, open, read, write, fcntl, tcsetattr, ...), already carried
    /// as a `std::io::Error`.
    Io(io::Error),
}

impl fmt::Display for PtyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PtyError::Io(error) => write!(formatter, "pty os error: {error}"),
        }
    }
}

impl std::error::Error for PtyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PtyError::Io(error) => Some(error),
        }
    }
}

impl From<rustix::io::Errno> for PtyError {
    fn from(errno: rustix::io::Errno) -> Self {
        PtyError::Io(io::Error::from(errno))
    }
}

/// The master side of an open pseudoterminal: reads terminal output, writes
/// terminal input, and carries the window-size and line-discipline controls
/// the terminal runtime needs. Resizing through [`Master::set_winsize`]
/// makes the kernel deliver SIGWINCH to the pty's foreground process group
/// — no signal handling is needed on this side.
///
/// Concurrency: the intended shape is one reader plus one writer (the
/// terminal runtime's read loop and single-writer task). [`Master::read`]
/// and [`Master::write`] take no locks and never block each other;
/// [`Master::read_timeout`] and [`Master::set_nonblocking`] serialize on a
/// per-master lock because a timed read must briefly flip the descriptor's
/// shared `O_NONBLOCK` state — see their docs for what a racing plain read
/// or write observes.
pub struct Master {
    fd: OwnedFd,
    io_lock: Mutex<()>,
}

/// `Ok(0)` is reserved for the master's end-of-file, so a zero-length read
/// buffer is rejected instead of colliding with it.
fn require_read_buffer(buf: &[u8]) -> Result<(), PtyError> {
    if buf.is_empty() {
        return Err(PtyError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "master reads require a nonempty buffer; Ok(0) is reserved for end-of-file",
        )));
    }
    Ok(())
}

impl Master {
    /// Blocking read of terminal output. `Ok(0)` is the master's
    /// end-of-file: every slave descriptor is closed (macOS reports a
    /// plain EOF there, Linux reports `EIO`; both are normalized). Signal
    /// deliveries that interrupt the read (`SIGCHLD`, `SIGWINCH` from
    /// other children) are retried rather than surfaced as errors. A
    /// concurrent [`Master::read_timeout`] briefly flips the descriptor
    /// nonblocking; a racing plain read then surfaces `WouldBlock` —
    /// retry, or keep to the single-reader shape.
    pub fn read(&self, buf: &mut [u8]) -> Result<usize, PtyError> {
        require_read_buffer(buf)?;
        loop {
            match rustix::io::read(&self.fd, &mut *buf) {
                Err(rustix::io::Errno::INTR) => continue,
                Err(rustix::io::Errno::IO) => return Ok(0),
                other => return other.map_err(PtyError::from),
            }
        }
    }

    /// Read terminal output with a deadline; `Ok(None)` means the deadline
    /// expired without data. The feature set deliberately excludes
    /// poll/epoll, so this is a nonblocking read retried on an adaptive
    /// sleep (1 ms floor, 10 ms ceiling, scaled to the remaining window);
    /// production read loops should hand the fd to a real poller such as
    /// `tokio::io::AsyncFd`.
    ///
    /// Concurrency: the whole timed window — the `O_NONBLOCK` flip, the
    /// retry loop, and the restore — is serialized against other timed
    /// readers and [`Master::set_nonblocking`] on a per-master lock, so
    /// concurrent timed reads cannot corrupt each other's blocking state
    /// or pin a thread past its own deadline (a waiter's window starts
    /// when it acquires the lock). [`Master::read`] and [`Master::write`]
    /// take no lock; racing this window they surface `WouldBlock` instead
    /// of blocking silently.
    pub fn read_timeout(
        &self,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<Option<usize>, PtyError> {
        require_read_buffer(buf)?;
        // The lock is the whole point (see the concurrency note above):
        // held for the timed window, whose own deadline bounds any waiter.
        let _io = self
            .io_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let started = Instant::now();
        let saved = fs::fcntl_getfl(&self.fd)?;
        let was_nonblocking = saved.contains(fs::OFlags::NONBLOCK);
        if !was_nonblocking {
            fs::fcntl_setfl(&self.fd, saved | fs::OFlags::NONBLOCK)?;
        }
        let outcome = loop {
            match rustix::io::read(&self.fd, &mut *buf) {
                Err(rustix::io::Errno::AGAIN) | Err(rustix::io::Errno::INTR) => {
                    let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
                        break Ok(None);
                    };
                    if remaining.is_zero() {
                        break Ok(None);
                    }
                    let pause =
                        (remaining / 32).clamp(READ_TIMEOUT_POLL_FLOOR, READ_TIMEOUT_POLL_CEILING);
                    thread::sleep(pause);
                }
                Err(rustix::io::Errno::IO) => break Ok(Some(0)),
                other => break other.map(Some).map_err(PtyError::from),
            }
        };
        // Restore the shared blocking state before returning. A failed
        // restore must not discard a completed outcome: received data or a
        // read error wins (the descriptor is then left nonblocking, which
        // later reads surface as WouldBlock rather than as silence), and
        // only a bare timeout reports the restore failure instead.
        if !was_nonblocking && let Err(restore_error) = fs::fcntl_setfl(&self.fd, saved) {
            return match outcome {
                Ok(None) => Err(PtyError::from(restore_error)),
                completed => completed,
            };
        }
        outcome
    }

    /// Write terminal input. A single write call: in canonical mode the
    /// line-discipline input queue may accept only a prefix, so callers that
    /// must deliver everything loop on the unwritten remainder. A write
    /// racing a [`Master::read_timeout`] window may surface `WouldBlock`
    /// from the timed read's nonblocking flip — retry.
    pub fn write(&self, buf: &[u8]) -> Result<usize, PtyError> {
        Ok(rustix::io::write(&self.fd, buf)?)
    }

    /// Toggle `O_NONBLOCK` on the master descriptor. Serialized against
    /// timed reads so the toggle cannot interleave with their
    /// flip-and-restore.
    pub fn set_nonblocking(&self, nonblocking: bool) -> Result<(), PtyError> {
        let _io = self
            .io_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut flags = fs::fcntl_getfl(&self.fd)?;
        if flags.contains(fs::OFlags::NONBLOCK) != nonblocking {
            flags.set(fs::OFlags::NONBLOCK, nonblocking);
            fs::fcntl_setfl(&self.fd, flags)?;
        }
        Ok(())
    }

    /// `tcgetwinsize` of the pty, as `(rows, columns)`.
    pub fn winsize(&self) -> Result<(u16, u16), PtyError> {
        let winsize = termios::tcgetwinsize(&self.fd)?;
        Ok((winsize.ws_row, winsize.ws_col))
    }

    /// `tcsetwinsize(rows, columns)`. The kernel turns this into SIGWINCH
    /// for the pty's foreground process group. Zero dimensions are
    /// rejected: a fresh pair is 0x0 until its first `set_winsize`, and an
    /// accepted zero would be indistinguishable from "never sized" —
    /// callers that spawn children must set real dimensions first (the
    /// terminals.create contract always supplies them).
    pub fn set_winsize(&self, rows: u16, cols: u16) -> Result<(), PtyError> {
        if rows == 0 || cols == 0 {
            return Err(PtyError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pty winsize dimensions must be nonzero",
            )));
        }
        termios::tcsetwinsize(
            &self.fd,
            termios::Winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
        )?;
        Ok(())
    }

    /// `tcgetattr` of the pty's shared line discipline. The returned value
    /// is rustix's `Termios`; mutate its flag fields and hand it to
    /// [`Master::set_attributes`].
    pub fn attributes(&self) -> Result<termios::Termios, PtyError> {
        Ok(termios::tcgetattr(&self.fd)?)
    }

    /// `tcsetattr` with `TCSADRAIN`: apply the line-discipline settings once
    /// pending output has drained.
    pub fn set_attributes(&self, attributes: &termios::Termios) -> Result<(), PtyError> {
        termios::tcsetattr(&self.fd, termios::OptionalActions::Drain, attributes)?;
        Ok(())
    }

    /// Switch the line discipline to raw mode (`cfmakeraw`):
    /// byte-at-a-time input, no echo, no signal or output processing.
    /// Gateway terminals keep the cooked kernel defaults; this is the mode
    /// an interactive attach applies to its own tty, exposed here for
    /// parity and for tests.
    pub fn set_raw(&self) -> Result<(), PtyError> {
        let mut attributes = self.attributes()?;
        attributes.make_raw();
        self.set_attributes(&attributes)
    }
}

impl AsFd for Master {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// The slave side of an open pseudoterminal, wired to the child's stdio
/// before spawn.
pub struct Slave {
    fd: OwnedFd,
}

impl Slave {
    /// Duplicate the slave descriptor (one per stdio stream) with
    /// close-on-exec preserved, so the copies handed to
    /// `std::process::Stdio` between creation and spawn cannot leak into
    /// unrelated children. The child itself still receives them the normal
    /// way: std dup2's the descriptor onto 0/1/2, which clears
    /// close-on-exec exactly where it must stay open.
    pub fn try_clone(&self) -> Result<Slave, PtyError> {
        Ok(Slave {
            fd: rustix::io::fcntl_dupfd_cloexec(&self.fd, 0)?,
        })
    }
}

impl AsFd for Slave {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl From<Slave> for std::process::Stdio {
    fn from(slave: Slave) -> Self {
        std::process::Stdio::from(slave.fd)
    }
}

impl From<Slave> for std::fs::File {
    fn from(slave: Slave) -> Self {
        std::fs::File::from(slave.fd)
    }
}

/// An open pseudoterminal pair: the master the terminal runtime keeps, and
/// the slave the child receives as stdio. Dropping the pair closes both
/// descriptors — closing the master makes the kernel hang up the session
/// that owns the slave as its controlling terminal, and after spawn the
/// caller must close its own slave copies ([`PtyPair::into_master_slave`])
/// so the master can observe the child's exit as end-of-file.
pub struct PtyPair {
    master: Master,
    slave: Slave,
}

impl PtyPair {
    /// Open a fresh pseudoterminal: `posix_openpt(O_RDWR | O_NOCTTY)` +
    /// `grantpt` + `unlockpt`, then the slave descriptor — `TIOCGPTPEER` on
    /// Linux, `ptsname` + `open(O_RDWR | O_NOCTTY | O_CLOEXEC)` elsewhere.
    /// A fresh pair is 0x0 until [`Master::set_winsize`]; callers that
    /// spawn children must size it first. Both descriptors are
    /// close-on-exec before the pair is handed out (see the platform
    /// notes).
    pub fn open() -> Result<PtyPair, PtyError> {
        let master = pty::openpt(openpt_flags())?;
        pty::grantpt(&master)?;
        pty::unlockpt(&master)?;
        let slave = open_slave(&master)?;
        Ok(PtyPair {
            master: Master {
                fd: master_cloexec(master)?,
                io_lock: Mutex::new(()),
            },
            slave: Slave { fd: slave },
        })
    }

    /// The master side.
    pub fn master(&self) -> &Master {
        &self.master
    }

    /// The slave side, for stdio wiring before spawn.
    pub fn slave(&self) -> &Slave {
        &self.slave
    }

    /// Split the pair so each side can be dropped, polled, or converted
    /// (`Stdio::from(slave)`) independently — in particular so the parent's
    /// slave side can be closed right after spawn.
    pub fn into_master_slave(self) -> (Master, Slave) {
        (self.master, self.slave)
    }

    /// Resolve the slave device path with `ptsname`. On Linux the descriptor
    /// from `TIOCGPTPEER` makes the path unnecessary; it stays useful for
    /// identity reporting on the other platforms.
    pub fn slave_path(&self) -> Result<PathBuf, PtyError> {
        let path = pty::ptsname(&self.master.fd, Vec::new())?;
        Ok(PathBuf::from(std::ffi::OsString::from_vec(
            path.into_bytes(),
        )))
    }
}

/// The openpt request flags. `RDWR` for a bidirectional master, `NOCTTY` so
/// the spawning process never claims the pty as its own controlling
/// terminal (the child does that through the pre_exec hook), and `CLOEXEC`
/// where the platform's openpt flags support it (Linux), making the master
/// close-on-exec atomically at open time there.
fn openpt_flags() -> pty::OpenptFlags {
    let flags = pty::OpenptFlags::RDWR | pty::OpenptFlags::NOCTTY;
    #[cfg(target_os = "linux")]
    let flags = flags | pty::OpenptFlags::CLOEXEC;
    flags
}

/// Normalize the master to close-on-exec. On Linux the openpt flags already
/// requested it atomically; on the other supported platforms openpt cannot,
/// so the descriptor is re-duplicated through `F_DUPFD_CLOEXEC` (the
/// duplicate shares the file description; the original closes when it
/// drops). Only the master needs this — both slave paths carry close-on-
/// exec in their open flags on every platform, so they have no
/// open-to-normalize window. On non-Linux a small open-to-dup window for
/// the master remains where a concurrently spawning thread could inherit
/// the original: rustix 1.1.5 has no safe close-on-exec request for those
/// openpt flags.
fn master_cloexec(master: OwnedFd) -> Result<OwnedFd, PtyError> {
    #[cfg(target_os = "linux")]
    {
        Ok(master)
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(rustix::io::fcntl_dupfd_cloexec(&master, 0)?)
    }
}

/// Obtain the slave descriptor once the master is unlocked. Linux opens it
/// straight off the master with `TIOCGPTPEER` (no ptsname race) — rustix
/// deliberately shares its `OpenptFlags` between `openpt` and
/// `ioctl_tiocgptpeer`, and the kernel honors the open(2)-style
/// RDWR/NOCTTY/CLOEXEC set passed here (verified on Linux by the cb8
/// run). Every other supported platform resolves the path with `ptsname`
/// and opens it with the same flags, so the slave is close-on-exec at
/// open time on every platform.
fn open_slave(master: &OwnedFd) -> Result<OwnedFd, PtyError> {
    #[cfg(target_os = "linux")]
    {
        Ok(pty::ioctl_tiocgptpeer(master, openpt_flags())?)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let path = pty::ptsname(master, Vec::new())?;
        Ok(fs::open(
            path,
            fs::OFlags::RDWR | fs::OFlags::NOCTTY | fs::OFlags::CLOEXEC,
            fs::Mode::empty(),
        )?)
    }
}
