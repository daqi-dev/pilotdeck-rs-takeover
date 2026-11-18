//! Integration tests for the PTY substrate (t015 PR-2).
//!
//! Determinism rules mirror the e2e suites: every read is deadline-bounded
//! through `Master::read_timeout`, every child wait is deadline-bounded,
//! and negative "no output" assertions use exactly one bounded quiet
//! window. Every spawned child runs with its cwd inside an isolated
//! temporary directory; nothing touches a real home or any running
//! instance. Failure hygiene: `PtySession` kills and reaps its child before
//! every failing assertion (and again on drop), so a panicking test never
//! leaves a session behind the test process, even under panic=abort
//! profiles where drop hygiene could not run.
//!
//! Verified locally on macOS; the Linux column (TIOCGPTPEER path, the EIO
//! end-of-file flavor normalized to `Ok(0)`, the openpt CLOEXEC flag) is
//! left to the integrator's cb8 run.

use std::{
    fs,
    io::Read as _,
    os::fd::{AsFd as _, AsRawFd as _},
    os::unix::process::ExitStatusExt as _,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicU32, Ordering},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use agentcpm_pd_pty::{Master, PtyError, PtyPair, Slave};
use pilotdeck_child_signals::{install_child_controlling_terminal, install_child_signal_reset};
use rustix::termios::LocalModes;

/// Diagnostic window cap: failing assertions quote at most this many of the
/// most recently observed bytes, never the whole stream.
const OBSERVED_TAIL_CAP: usize = 128;

/// POSIX reserves signal number 1 for SIGHUP on every supported platform.
const SIGHUP: i32 = 1;

/// Isolated per-test temporary directory, removed on drop.
struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> TestDir {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = format!(
            "agentcpm-pd-pty-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
            tag,
        );
        let path = std::env::temp_dir().join(unique);
        fs::create_dir_all(&path).expect("create isolated test directory");
        TestDir(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A spawned child plus its master. Failing assertions kill and reap the
/// child first (see [`PtySession::fail`]); dropping the session does it
/// again as a backstop.
struct PtySession {
    master: Option<Master>,
    child: Child,
}

impl PtySession {
    /// Spawn `program` with the pty slave as all three stdio streams and
    /// the t015 hook pair installed — controlling terminal first, then the
    /// signal reset that stays last per its own contract. The parent's
    /// slave side is closed right after spawn so the master can observe
    /// the child's exit. A 24x80 default window is set first (fresh pairs
    /// are 0x0 and zero sizes are rejected); `configure` may override it.
    fn spawn(
        dir: &Path,
        program: &str,
        args: &[&str],
        configure: impl FnOnce(&Master),
    ) -> PtySession {
        let (session, _slave) = Self::spawn_with_slave(dir, program, args, configure);
        session
    }

    /// [`PtySession::spawn`], but the parent keeps its slave side (for tests
    /// that read it after closing the master).
    fn spawn_with_slave(
        dir: &Path,
        program: &str,
        args: &[&str],
        configure: impl FnOnce(&Master),
    ) -> (PtySession, Slave) {
        let pair = PtyPair::open().expect("open pty pair");
        pair.master()
            .set_winsize(24, 80)
            .expect("default 24x80 window");
        configure(pair.master());
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::from(
                pair.slave().try_clone().expect("duplicate slave stdin"),
            ))
            .stdout(Stdio::from(
                pair.slave().try_clone().expect("duplicate slave stdout"),
            ))
            .stderr(Stdio::from(
                pair.slave().try_clone().expect("duplicate slave stderr"),
            ));
        install_child_controlling_terminal(&mut command, pair.slave().as_fd().as_raw_fd())
            .expect("install the controlling-terminal hook");
        install_child_signal_reset(&mut command);
        let child = command.spawn().expect("spawn child on the pty");
        let (master, slave) = pair.into_master_slave();
        (
            PtySession {
                master: Some(master),
                child,
            },
            slave,
        )
    }

    fn master(&self) -> &Master {
        self.master.as_ref().expect("master still open")
    }

    /// Close the master side: the kernel hangs up the session that owns the
    /// slave as its controlling terminal.
    fn close_master(&mut self) {
        drop(self.master.take());
    }

    /// Kill and reap before failing: a failing assertion must never leak
    /// the child, including panic=abort profiles where drop hygiene
    /// cannot run.
    fn fail(&mut self, message: String) -> ! {
        let _ = self.child.kill();
        let _ = self.child.wait();
        panic!("{message}");
    }

    /// Bounded child wait: kills and reaps before failing on the deadline.
    fn wait_exit(&mut self, what: &str) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().expect("child try_wait") {
                return status;
            }
            if Instant::now() >= deadline {
                self.fail(format!("child did not exit within the deadline: {what}"));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Read until `needle` has arrived `expected` times, or fail at the
    /// deadline. Returns the total occurrence count and a bounded tail of
    /// the most recent bytes (`OBSERVED_TAIL_CAP`); occurrences straddling
    /// read-chunk boundaries are counted through a short carry, so nothing
    /// unbounded is retained and failing messages quote the tail only.
    fn read_until_count(&mut self, needle: &[u8], expected: usize, what: &str) -> (usize, Vec<u8>) {
        assert!(!needle.is_empty(), "read_until_count needs a needle");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut occurrences = 0usize;
        let mut carry: Vec<u8> = Vec::new();
        let mut tail: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 1024];
        while occurrences < expected {
            if Instant::now() >= deadline {
                self.fail(format!(
                    "timed out waiting for {what}; recent bytes: {tail:?}"
                ));
            }
            let outcome = self
                .master()
                .read_timeout(&mut chunk, Duration::from_millis(200))
                .expect("bounded master read");
            match outcome {
                Some(0) => {
                    self.fail(format!(
                        "master hit end-of-file while waiting for {what}; recent bytes: {tail:?}"
                    ));
                }
                Some(count) => {
                    let mut joined = carry;
                    joined.extend_from_slice(&chunk[..count]);
                    // The carry is always shorter than the needle, so no
                    // occurrence inside it was ever counted: this pass
                    // counts exactly the occurrences whose last byte just
                    // arrived.
                    occurrences += count_occurrences(&joined, needle);
                    carry = if joined.len() < needle.len() {
                        joined
                    } else {
                        joined.split_off(joined.len() - (needle.len() - 1))
                    };
                    tail.extend_from_slice(&chunk[..count]);
                    let excess = tail.len().saturating_sub(OBSERVED_TAIL_CAP);
                    if excess > 0 {
                        tail.drain(..excess);
                    }
                }
                None => continue,
            }
        }
        (occurrences, tail)
    }

    /// Exactly one bounded quiet window: no bytes may arrive in it.
    fn assert_quiet(&mut self, window: Duration, context: &str) {
        let mut chunk = [0u8; 1024];
        let outcome = self
            .master()
            .read_timeout(&mut chunk, window)
            .expect("bounded master read");
        match outcome {
            None => {}
            Some(0) => {
                self.fail(format!(
                    "master hit end-of-file during the quiet window ({context})"
                ));
            }
            Some(count) => {
                self.fail(format!(
                    "unexpected {count} bytes during the quiet window ({context}): {:?}",
                    &chunk[..count.min(OBSERVED_TAIL_CAP)]
                ));
            }
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Backstop for the explicit fail() paths: kill is a harmless error
        // once the child already exited, and reaping here is what keeps
        // panics from leaking sessions.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn count_occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    assert!(!needle.is_empty(), "count_occurrences needs a needle");
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

/// A fresh pair is usable without any child: the slave path resolves, the
/// kernel's cooked defaults (ECHO + ICANON) are visible through the master,
/// window sizes round-trip while zero dimensions are rejected, an empty
/// read buffer is an error rather than a fake end-of-file, a timed read on
/// an idle master is a timeout rather than a hang, and the nonblocking
/// toggle surfaces WouldBlock.
#[test]
fn fresh_pair_paths_winsize_defaults_and_timeouts() {
    let pair = PtyPair::open().expect("open pty pair");

    let path = pair.slave_path().expect("resolve slave path");
    assert!(
        path.to_string_lossy().starts_with("/dev/"),
        "unexpected slave path {path:?}"
    );

    let attributes = pair.master().attributes().expect("read attributes");
    assert!(
        attributes.local_modes.contains(LocalModes::ECHO),
        "fresh ptys echo: {attributes:?}"
    );
    assert!(
        attributes.local_modes.contains(LocalModes::ICANON),
        "fresh ptys are canonical: {attributes:?}"
    );

    pair.master().set_winsize(33, 77).expect("set winsize");
    assert_eq!(pair.master().winsize().expect("read winsize"), (33, 77));
    // Zero dimensions are rejected: they would be indistinguishable from
    // the fresh, never-sized 0x0 pair.
    assert!(pair.master().set_winsize(0, 80).is_err());
    assert!(pair.master().set_winsize(24, 0).is_err());

    // Ok(0) is reserved for end-of-file, so empty buffers are an error
    // instead of colliding with it.
    let mut empty: [u8; 0] = [];
    assert!(matches!(
        pair.master().read(&mut empty),
        Err(PtyError::Io(error)) if error.kind() == std::io::ErrorKind::InvalidInput
    ));
    assert!(matches!(
        pair.master()
            .read_timeout(&mut empty, Duration::from_millis(50)),
        Err(PtyError::Io(error)) if error.kind() == std::io::ErrorKind::InvalidInput
    ));

    let mut chunk = [0u8; 16];
    let started = Instant::now();
    assert_eq!(
        pair.master()
            .read_timeout(&mut chunk, Duration::from_millis(50))
            .expect("timed read on the idle master"),
        None
    );
    assert!(
        started.elapsed() >= Duration::from_millis(40),
        "the timed read returned before its deadline"
    );

    pair.master()
        .set_nonblocking(true)
        .expect("set nonblocking");
    match pair.master().read(&mut chunk) {
        Err(PtyError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        other => panic!("expected WouldBlock from the idle nonblocking master, got {other:?}"),
    }
    pair.master()
        .set_nonblocking(false)
        .expect("restore blocking");
}

/// `cat` on the pty: a line written to the master comes back twice in the
/// cooked defaults (kernel ECHO plus cat's own copy, both ONLCR-processed),
/// and exactly once as raw bytes after `set_raw` (no echo, no output
/// processing). This is the echo round trip the terminals.* output path
/// rides on.
#[test]
fn cat_echo_round_trip_and_raw_mode() {
    let dir = TestDir::new("cat-echo");
    let mut session = PtySession::spawn(dir.path(), "cat", &[], |_| {});

    session.master().write(b"hello\n").expect("write to master");
    let (occurrences, _) = session.read_until_count(b"hello\r\n", 2, "echo and cat's copy");
    assert_eq!(occurrences, 2);

    session.master().set_raw().expect("switch to raw mode");
    session.master().write(b"raw\n").expect("write in raw mode");
    let (occurrences, tail) = session.read_until_count(b"raw\n", 1, "cat's raw copy");
    // Raw mode: exactly one arrival (no kernel echo — ECHO-on would echo the
    // same raw bytes a second time) and no ONLCR carriage returns anywhere
    // in the observed tail.
    assert_eq!(occurrences, 1, "raw mode must not echo; tail: {tail:?}");
    assert!(
        !tail.contains(&b'\r'),
        "raw mode must not ONLCR-translate: {tail:?}"
    );
    // The echo-off half of raw mode: nothing follows cat's own copy.
    session.assert_quiet(Duration::from_millis(300), "after raw write");

    session.close_master();
    session.wait_exit("cat after master close");
}

/// Termios settings applied through the master reach the pty the child is
/// sitting on, verified with `sh -c` readers: with ECHO left on the echoed
/// line comes back, and after clearing ECHO through the master the `read`
/// still completes (exit 0) while nothing is echoed.
#[test]
fn termios_settings_reach_the_sh_child() {
    let dir = TestDir::new("sh-termios");

    let mut session = PtySession::spawn(dir.path(), "sh", &["-c", "read line"], |_| {});
    session.master().write(b"y\n").expect("write to master");
    session.read_until_count(b"y\r\n", 1, "echo of the line");
    assert_eq!(session.wait_exit("sh with echo").code(), Some(0));

    // `sleep 1` holds the session open so the quiet window observes echo
    // absence rather than the child's exit.
    let mut session = PtySession::spawn(dir.path(), "sh", &["-c", "read line; sleep 1"], |_| {});
    let mut attributes = session.master().attributes().expect("read attributes");
    attributes.local_modes -= LocalModes::ECHO;
    session
        .master()
        .set_attributes(&attributes)
        .expect("clear echo");
    session.master().write(b"n\n").expect("write to master");
    session.assert_quiet(Duration::from_millis(300), "echo cleared");
    assert_eq!(session.wait_exit("sh without echo").code(), Some(0));
}

/// Window sizes set before spawn are what the child's `stty size` reports,
/// and a mid-session resize reads back consistently through the master.
#[test]
fn resize_reaches_the_child_and_reads_back() {
    let dir = TestDir::new("resize");

    let mut session = PtySession::spawn(dir.path(), "sh", &["-c", "stty size"], |master| {
        master.set_winsize(33, 77).expect("set initial winsize");
    });
    session.read_until_count(b"33 77\r\n", 1, "stty size output");
    assert_eq!(session.wait_exit("stty size").code(), Some(0));

    let mut session = PtySession::spawn(dir.path(), "cat", &[], |master| {
        master.set_winsize(24, 80).expect("set initial winsize");
    });
    session
        .master()
        .set_winsize(40, 100)
        .expect("resize mid-session");
    assert_eq!(
        session.master().winsize().expect("read winsize back"),
        (40, 100)
    );
    session.close_master();
    session.wait_exit("cat after resize");
}

/// Resize signals the foreground group from the child's side: the kernel's
/// SIGWINCH reaches a child that traps it (exiting 7 through its trap). A
/// foreground group exists only because the child claimed the controlling
/// terminal, so this pins the ctty contract together with the HUP case.
/// The blocker is the `read` builtin on purpose: a trapped signal arriving
/// while sh waits inside an external command (`sleep`) defers the trap
/// until that command completes (POSIX), which the deadline must not
/// depend on; a builtin read is interrupted immediately. `echo ready`
/// proves the trap is installed before the resize, and nothing is ever
/// written, so only the signal can end the read.
#[test]
fn resize_signals_the_foreground_child() {
    let dir = TestDir::new("resize-signal");
    let mut session = PtySession::spawn(
        dir.path(),
        "sh",
        &["-c", "trap 'exit 7' WINCH; echo ready; read line"],
        |_| {},
    );
    session.read_until_count(b"ready\r\n", 1, "trap installed marker");
    session.master().set_winsize(41, 101).expect("resize");
    assert_eq!(
        session.wait_exit("sh under SIGWINCH").code(),
        Some(7),
        "the resize must signal the foreground group"
    );
}

/// The controlling-terminal contract, child side: closing the master makes
/// the kernel hang up the session — the child dies BY SIGHUP (signal 1),
/// the direct, trap-free observation of the delivered signal (a
/// trap-based observation would defer behind whatever external command the
/// child happens to run, and an ignored HUP would prove nothing). Without
/// the hook the sleep would outlive the wait deadline and the test would
/// fail.
#[test]
fn closing_the_master_delivers_sighup_to_the_session() {
    let dir = TestDir::new("sighup");
    let mut session = PtySession::spawn(dir.path(), "sleep", &["10"], |_| {});
    session.close_master();
    let status = session.wait_exit("sleep after the master closed");
    assert_eq!(
        status.signal(),
        Some(SIGHUP),
        "closing the master must hang up the controlling session"
    );
}

/// The master's end-of-file after the child exits: macOS reports a plain
/// EOF, Linux reports `EIO` once the last slave descriptor closes — both
/// are normalized to `Some(0)`/`Ok(0)` rather than surfaced as an error.
/// The Linux EIO flavor is exercised by the integrator's cb8 run.
#[test]
fn master_reads_report_end_of_file_after_the_child_exits() {
    let dir = TestDir::new("master-eof");
    let mut session = PtySession::spawn(dir.path(), "sh", &["-c", "read line"], |_| {});
    session.master().write(b"e\n").expect("write to master");
    session.read_until_count(b"e\r\n", 1, "echo of the line");
    assert_eq!(session.wait_exit("sh after reading").code(), Some(0));
    let mut buffer = [0u8; 16];
    assert_eq!(
        session
            .master()
            .read_timeout(&mut buffer, Duration::from_secs(3))
            .expect("timed read"),
        Some(0),
        "the master must report end-of-file, not an error"
    );
    assert_eq!(
        session.master().read(&mut buffer).expect("blocking read"),
        0,
        "end-of-file is sticky"
    );
}

/// Closing the master hangs the session up: the child dies promptly, and
/// a reader holding the slave side sees a clean end (EOF on macOS, EIO on
/// Linux) instead of blocking forever.
#[test]
fn closing_the_master_ends_the_slave_side() {
    let dir = TestDir::new("close-master");
    let (mut session, slave) = PtySession::spawn_with_slave(dir.path(), "cat", &[], |_| {});

    // Sanity: the pty is alive (both the echo and cat's copy).
    session.master().write(b"ping\n").expect("write to master");
    session.read_until_count(b"ping\r\n", 2, "echo and cat's copy");

    let mut slave_file = fs::File::from(slave);
    session.close_master();
    session.wait_exit("cat after the master closed");

    let (outcome_tx, outcome_rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut buffer = [0u8; 64];
        let _ = outcome_tx.send(slave_file.read(&mut buffer));
    });
    let outcome = outcome_rx
        .recv_timeout(Duration::from_secs(3))
        .unwrap_or_else(|_| panic!("slave read never ended after the master closed"));
    match outcome {
        Ok(0) => {} // EOF (macOS)
        Ok(count) => panic!("unexpected {count} bytes from the hung-up pty"),
        Err(error) if error.raw_os_error() == Some(5) => {} // EIO (Linux)
        Err(other) => panic!("unexpected slave read error after hangup: {other}"),
    }
    reader.join().expect("join the slave reader");
}
