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
//! ptsname race) and requests close-on-exec through the openpt flags too;
//! every other supported platform resolves the path with `ptsname` and
//! opens it with the same `O_RDWR | O_NOCTTY` discipline. rustix's openpt
//! flags can only request close-on-exec on Linux, so both sides are
//! normalized through `fcntl(F_DUPFD_CLOEXEC)`: the pair never leaks into
//! children spawned from other threads, whose inherited copies would keep
//! the pty alive past the terminal's death and mask the master's
//! end-of-file and the kernel's hangup. `O_NOCTTY` everywhere keeps the
//! spawning process from acquiring the pty as its own controlling terminal
//! — the child claims it through the pre_exec hook instead. On macOS,
//! termios ioctls on the master only work once the slave has been opened
//! at least once; [`PtyPair::open`] always opens the slave, so the
//! master-side `attributes`/`set_attributes` calls are safe for every pair
//! this crate hands out.
#![cfg(unix)]

use std::{
    fmt, io,
    os::fd::{AsFd, OwnedFd},
    os::unix::ffi::OsStringExt,
    path::PathBuf,
    time::{Duration, Instant},
};

use rustix::{fs, pty, termios};

/// Interval between nonblocking retries in [`Master::read_timeout`]. The
/// substrate deliberately stays on the pty/termios feature set (no poll or
/// epoll), so a timed read is a short sleep-polled loop.
const READ_TIMEOUT_POLL_INTERVAL: Duration = Duration::from_millis(1);

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
pub struct Master {
    fd: OwnedFd,
}

impl Master {
    /// Blocking read of terminal output. `Ok(0)` is the master's
    /// end-of-file: every slave descriptor is closed.
    pub fn read(&self, buf: &mut [u8]) -> Result<usize, PtyError> {
        Ok(rustix::io::read(&self.fd, buf)?)
    }

    /// Read terminal output with a deadline; `Ok(None)` means the deadline
    /// expired without data. Implemented as a nonblocking read retried on a
    /// 1 ms sleep (the feature set deliberately excludes poll/epoll);
    /// production read loops should hand the fd to a real poller such as
    /// `tokio::io::AsyncFd`. The blocking/nonblocking state the fd had on
    /// entry is restored before returning.
    pub fn read_timeout(
        &self,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<Option<usize>, PtyError> {
        let deadline = Instant::now() + timeout;
        let saved = fs::fcntl_getfl(&self.fd)?;
        if !saved.contains(fs::OFlags::NONBLOCK) {
            fs::fcntl_setfl(&self.fd, saved | fs::OFlags::NONBLOCK)?;
        }
        let outcome = loop {
            match rustix::io::read(&self.fd, &mut *buf) {
                Ok(count) => break Ok(Some(count)),
                Err(rustix::io::Errno::AGAIN) => {
                    if Instant::now() >= deadline {
                        break Ok(None);
                    }
                    std::thread::sleep(READ_TIMEOUT_POLL_INTERVAL);
                }
                Err(other) => break Err(PtyError::from(other)),
            }
        };
        if !saved.contains(fs::OFlags::NONBLOCK) {
            fs::fcntl_setfl(&self.fd, saved)?;
        }
        outcome
    }

    /// Write terminal input. A single write call: in canonical mode the
    /// line-discipline input queue may accept only a prefix, so callers that
    /// must deliver everything loop on the unwritten remainder.
    pub fn write(&self, buf: &[u8]) -> Result<usize, PtyError> {
        Ok(rustix::io::write(&self.fd, buf)?)
    }

    /// Toggle `O_NONBLOCK` on the master descriptor.
    pub fn set_nonblocking(&self, nonblocking: bool) -> Result<(), PtyError> {
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
    /// for the pty's foreground process group.
    pub fn set_winsize(&self, rows: u16, cols: u16) -> Result<(), PtyError> {
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
    /// Linux, `ptsname` + `open(O_RDWR | O_NOCTTY)` elsewhere. Both sides
    /// are normalized to close-on-exec before the pair is handed out.
    pub fn open() -> Result<PtyPair, PtyError> {
        let master = pty::openpt(openpt_flags())?;
        pty::grantpt(&master)?;
        pty::unlockpt(&master)?;
        let slave = open_slave(&master)?;
        Ok(PtyPair {
            master: Master {
                fd: with_cloexec(master)?,
            },
            slave: Slave {
                fd: with_cloexec(slave)?,
            },
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
/// where the platform's openpt flags support it (Linux), closing the
/// open-to-normalization window below.
fn openpt_flags() -> pty::OpenptFlags {
    let flags = pty::OpenptFlags::RDWR | pty::OpenptFlags::NOCTTY;
    #[cfg(target_os = "linux")]
    let flags = flags | pty::OpenptFlags::CLOEXEC;
    flags
}

/// Normalize a descriptor to close-on-exec through `F_DUPFD_CLOEXEC`. The
/// terminal runtime spawns unrelated children from other threads; any
/// inherited copy of the pair would keep the pty alive past the terminal's
/// death, masking the master's end-of-file and the kernel's hangup. The
/// duplicate shares the same file description; the original closes when it
/// drops. On non-Linux platforms a small open-to-dup window remains where
/// a concurrently spawning thread could still inherit the original —
/// rustix 1.1.5 has no safe close-on-exec request for those openpt flags.
fn with_cloexec(fd: OwnedFd) -> Result<OwnedFd, PtyError> {
    Ok(rustix::io::fcntl_dupfd_cloexec(&fd, 0)?)
}

/// Obtain the slave descriptor once the master is unlocked. Linux opens it
/// straight off the master with `TIOCGPTPEER` (no ptsname race); every other
/// supported platform resolves the path and opens it with the same
/// `O_RDWR | O_NOCTTY` discipline.
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
            fs::OFlags::RDWR | fs::OFlags::NOCTTY,
            fs::Mode::empty(),
        )?)
    }
}
