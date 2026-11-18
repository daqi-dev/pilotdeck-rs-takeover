//! The #386 human-approved exception: reset inherited child signals before exec.
//! The #552 human-approved addition: libuv's `detached` `setsid()` in the same
//! fork-to-exec window, for every spawn whose TS counterpart is detached (the
//! bash runner, background tasks, the ripgrep engine spawn and command hooks;
//! A6.5-BASH-OS-001 setsid half).
//! The t015 PR-1 addition (terminal PTY substrate): `setsid()` +
//! `TIOCSCTTY` so a spawned pty child owns its controlling terminal.
#![cfg(unix)]

use std::{
    io, os::fd::BorrowedFd, os::unix::io::RawFd, os::unix::process::CommandExt, process::Command,
};

// libuv's Unix child-init range is 1..32, excluding unchangeable signals.
// This is 29 signals on macOS and Linux; Linux 32/33 (glibc's internal
// realtime signals), and all other realtime dispositions, are left alone.
// Build the list at compile time, never by querying libc after fork.
const CHILD_SIGNALS: [libc::c_int; 29] = {
    let mut signals = [0; 29];
    let mut signal = 1;
    let mut index = 0;
    while signal < 32 {
        if signal != libc::SIGKILL && signal != libc::SIGSTOP {
            signals[index] = signal;
            index += 1;
        }
        signal += 1;
    }
    signals
};

/// Install before spawn, after any caller-owned pre-exec hooks. Tokio callers
/// pass `as_std_mut()`. Only the forked child executes this hook; std retains
/// ownership of cwd, stdio, exec-error reporting, and the child handle.
#[allow(unsafe_code)]
pub fn install_child_signal_reset(command: &mut Command) {
    // SAFETY: pre_exec runs only in the single-threaded child after fork. The
    // closure captures nothing and uses only stack/POD data, a constant list,
    // and async-signal-safe sigemptyset/sigaction/sigprocmask. A zeroed sigaction
    // is valid on macOS/Linux (integer fields and nullable sa_restorer); its
    // mask is explicitly initialized before use and its handler is SIG_DFL.
    // All pointers refer to live stack storage; null old-action/mask pointers
    // request no output. Each error immediately reads thread-local errno via
    // last_os_error, which constructs an inline OS error without allocation.
    // No locks, environment access, formatting, destructors, or panics occur.
    unsafe {
        command.pre_exec(|| {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = libc::SIG_DFL;
            if libc::sigemptyset(&mut action.sa_mask) != 0 {
                return Err(io::Error::last_os_error());
            }
            for signal in CHILD_SIGNALS {
                if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            // Unblock only after every disposition has been reset.
            if libc::sigprocmask(libc::SIG_SETMASK, &action.sa_mask, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// `detached: true` (#552): libuv's child init calls `setsid()`, so the
/// child leads a fresh session — its own process group with no controlling
/// terminal, so commands cannot read the gateway's terminal (`/dev/tty`
/// opens fail ENXIO). Install before `install_child_signal_reset`, which
/// stays the last hook per its own contract; the run order relative to the
/// resets is immaterial (the steps are independent, unlike in libuv where
/// the resets come first). Installers are the spawns whose TS counterparts
/// are detached: the bash runner (with its shared background-task spawn),
/// the ripgrep engine spawn and command hooks. The sidecar and MCP stdio
/// spawns mirror TS spawns that are not detached. Callers that use this
/// must not also request `process_group(0)`: the forked child would already
/// be a group leader and `setsid` would fail with EPERM.
#[allow(unsafe_code)]
pub fn install_child_detach_session(command: &mut Command) {
    // SAFETY: pre_exec runs only in the single-threaded child after fork. The
    // closure captures nothing and calls only setsid, an async-signal-safe
    // syscall with no locks, environment access, allocation, formatting, or
    // destructors. Its error path reports the child's errno through
    // last_os_error, which constructs an inline OS error without allocating.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// The t015 PR-1 addition (terminal PTY substrate): give the spawned child
/// the pty slave as its controlling terminal. The child first leads a fresh
/// session (`setsid()`, the same step #552 performs) and then claims the
/// slave with `ioctl(fd, TIOCSCTTY, 0)`; only the session leader itself can
/// acquire a controlling terminal, so both steps must run inside the
/// fork-to-exec window. Without a controlling terminal there is no foreground
/// process group: the line discipline's ISIG (Ctrl-C) and job control would
/// never reach the child, SIGWINCH from a resize would never arrive, and
/// the kernel would not hang it up when the pty master closes. Install
/// before `install_child_signal_reset`, which stays the last hook per its
/// own contract. Callers must not also use `install_child_detach_session`
/// (the second `setsid` would fail with EPERM) or `process_group(0)` (the
/// child would already be a group leader). The pty pair itself comes from
/// `agentcpm-pd-pty`; pass its slave fd, kept open across the spawn, and
/// close the parent's copies once the child owns its stdio. The install
/// duplicates that descriptor (close-on-exec, so the copy cannot outlive
/// exec) and the closure captures the duplicate, pinning the file
/// description itself: fd-number reuse between install and spawn cannot
/// redirect TIOCSCTTY at an unrelated file. The returned error reports
/// only that install-time duplication; later failures surface through
/// spawn, like every pre_exec hook.
#[allow(unsafe_code)]
pub fn install_child_controlling_terminal(
    command: &mut Command,
    slave_fd: RawFd,
) -> io::Result<()> {
    // SAFETY: the unsafe block covers two operations. BorrowedFd::borrow_raw
    // runs at install time in the caller's process: the reference lives only
    // for the duplication, and a stale fd number fails the dup with EBADF
    // instead of reaching the closure. pre_exec runs only in the
    // single-threaded child after fork; its closure captures one OwnedFd
    // duplicated from the caller's slave descriptor before the fork and
    // calls only setsid and the TIOCSCTTY ioctl, both async-signal-safe
    // syscalls wrapped by rustix's safe APIs, with no locks, environment
    // access, formatting, or panics. The captured descriptor's destructor
    // never runs in the child: exec replaces the image and std's post-fork
    // error path exits without unwinding. Each error converts to an inline
    // io::Error without allocating.
    unsafe {
        let slave = rustix::io::fcntl_dupfd_cloexec(BorrowedFd::borrow_raw(slave_fd), 0)?;
        command.pre_exec(move || {
            rustix::process::setsid()?;
            rustix::process::ioctl_tiocsctty(&slave)?;
            Ok(())
        });
    }
    Ok(())
}
