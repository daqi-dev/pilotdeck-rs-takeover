//! Integration tests for the PTY substrate (t015 PR-2).
//!
//! Determinism rules mirror the e2e suites: every read is deadline-bounded
//! through [`Master::read_timeout`], every child wait is deadline-bounded,
//! and negative "no output" assertions use exactly one bounded quiet window.
//! Every spawned child runs with its cwd inside an isolated temporary
//! directory; nothing touches a real home or any running instance.
//!
//! Verified locally on macOS; the Linux column (TIOCGPTPEER path, EIO
//! end-of-file flavor) is left to the integrator's cb8 run.

use std::{
    fs,
    io::Read as _,
    os::fd::{AsFd as _, AsRawFd as _},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU32, Ordering},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use agentcpm_pd_pty::{Master, PtyError, PtyPair};
use pilotdeck_child_signals::{install_child_controlling_terminal, install_child_signal_reset};
use rustix::termios::LocalModes;

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

/// Spawn `program` with the pty slave as all three stdio streams and the
/// t015 hook pair installed — controlling terminal first, then the signal
/// reset that stays last per its own contract. The parent's slave side is
/// closed right after spawn so the master can observe the child's exit.
fn spawn_on_pty(
    dir: &Path,
    program: &str,
    args: &[&str],
    configure: impl FnOnce(&Master),
) -> (Master, Child) {
    let pair = PtyPair::open().expect("open pty pair");
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
    install_child_controlling_terminal(&mut command, pair.slave().as_fd().as_raw_fd());
    install_child_signal_reset(&mut command);
    let child = command.spawn().expect("spawn child on the pty");
    let (master, slave) = pair.into_master_slave();
    drop(slave);
    (master, child)
}

/// Bounded child wait: panics when the child outlives the deadline.
fn wait_bounded(child: &mut Child, what: &str) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().expect("child try_wait") {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "child did not exit within the deadline: {what}"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn count_occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

/// Read until `needle` appeared `expected` times, or fail at the deadline.
fn read_until_count(master: &Master, needle: &[u8], expected: usize, what: &str) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = Vec::new();
    let mut chunk = [0u8; 1024];
    while count_occurrences(&seen, needle) < expected {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what} in {seen:?}"
        );
        match master
            .read_timeout(&mut chunk, Duration::from_millis(200))
            .expect("bounded master read")
        {
            Some(0) => panic!("master hit end-of-file while waiting for {what}"),
            Some(count) => seen.extend_from_slice(&chunk[..count]),
            None => continue,
        }
    }
    seen
}

/// Exactly one bounded quiet window: no bytes may arrive in it.
fn assert_quiet(master: &Master, window: Duration, context: &str) {
    let mut chunk = [0u8; 1024];
    match master
        .read_timeout(&mut chunk, window)
        .expect("bounded master read")
    {
        None => {}
        Some(0) => panic!("master hit end-of-file during the quiet window ({context})"),
        Some(count) => panic!(
            "unexpected {count} bytes during the quiet window ({context}): {:?}",
            &chunk[..count]
        ),
    }
}

/// A fresh pair is usable without any child: the slave path resolves, the
/// kernel's cooked defaults (ECHO + ICANON) are visible through the master,
/// window sizes round-trip, a timed read on an idle master is a timeout
/// rather than a hang, and the nonblocking toggle surfaces WouldBlock.
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
    pair.master()
        .set_winsize(40, 100)
        .expect("set winsize again");
    assert_eq!(
        pair.master().winsize().expect("read winsize again"),
        (40, 100)
    );

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
    let (master, mut child) = spawn_on_pty(dir.path(), "cat", &[], |_| {});

    master.write(b"hello\n").expect("write to master");
    let seen = read_until_count(&master, b"hello\r\n", 2, "echo and cat's copy");
    assert_eq!(count_occurrences(&seen, b"hello\r\n"), 2);

    master.set_raw().expect("switch to raw mode");
    master.write(b"raw\n").expect("write in raw mode");
    let seen = read_until_count(&master, b"raw\n", 1, "cat's raw copy");
    assert_eq!(
        count_occurrences(&seen, b"raw\n"),
        1,
        "raw mode must not echo: {seen:?}"
    );
    assert!(
        !seen.contains(&b'\r'),
        "raw mode must not ONLCR-translate: {seen:?}"
    );
    // The echo-off half of raw mode: nothing follows cat's own copy.
    assert_quiet(&master, Duration::from_millis(300), "after raw write");

    drop(master);
    wait_bounded(&mut child, "cat after master close");
}

/// Termios settings applied through the master reach the pty the child is
/// sitting on, verified with `sh -c` readers: with ECHO left on the echoed
/// line comes back, and after clearing ECHO through the master the `read`
/// still completes (exit 0) while nothing is echoed.
#[test]
fn termios_settings_reach_the_sh_child() {
    let dir = TestDir::new("sh-termios");

    let (master, mut child) = spawn_on_pty(dir.path(), "sh", &["-c", "read line"], |_| {});
    master.write(b"y\n").expect("write to master");
    read_until_count(&master, b"y\r\n", 1, "echo of the line");
    assert_eq!(wait_bounded(&mut child, "sh with echo").code(), Some(0));

    // `sleep 1` holds the session open so the quiet window observes echo
    // absence rather than the child's exit.
    let (master, mut child) = spawn_on_pty(dir.path(), "sh", &["-c", "read line; sleep 1"], |_| {});
    let mut attributes = master.attributes().expect("read attributes");
    attributes.local_modes -= LocalModes::ECHO;
    master.set_attributes(&attributes).expect("clear echo");
    master.write(b"n\n").expect("write to master");
    assert_quiet(&master, Duration::from_millis(300), "echo cleared");
    assert_eq!(wait_bounded(&mut child, "sh without echo").code(), Some(0));
}

/// Window sizes set before spawn are what the child's `stty size` reports,
/// and a mid-session resize reads back consistently through the master.
#[test]
fn resize_reaches_the_child_and_reads_back() {
    let dir = TestDir::new("resize");

    let (master, mut child) = spawn_on_pty(dir.path(), "sh", &["-c", "stty size"], |master| {
        master.set_winsize(33, 77).expect("set initial winsize");
    });
    read_until_count(&master, b"33 77\r\n", 1, "stty size output");
    assert_eq!(wait_bounded(&mut child, "stty size").code(), Some(0));

    let (master, mut child) = spawn_on_pty(dir.path(), "cat", &[], |master| {
        master.set_winsize(24, 80).expect("set initial winsize");
    });
    master.set_winsize(40, 100).expect("resize mid-session");
    assert_eq!(master.winsize().expect("read winsize back"), (40, 100));
    drop(master);
    wait_bounded(&mut child, "cat after resize");
}

/// Closing the master hangs the session up: the child dies promptly, and a
/// reader holding the slave side sees a clean end (EOF on macOS, EIO on
/// Linux) instead of blocking forever.
#[test]
fn closing_the_master_ends_the_slave_side() {
    let dir = TestDir::new("close-master");
    let pair = PtyPair::open().expect("open pty pair");
    let mut command = Command::new("cat");
    command
        .current_dir(dir.path())
        .stdin(Stdio::from(
            pair.slave().try_clone().expect("duplicate slave stdin"),
        ))
        .stdout(Stdio::from(
            pair.slave().try_clone().expect("duplicate slave stdout"),
        ))
        .stderr(Stdio::from(
            pair.slave().try_clone().expect("duplicate slave stderr"),
        ));
    install_child_controlling_terminal(&mut command, pair.slave().as_fd().as_raw_fd());
    install_child_signal_reset(&mut command);
    let mut child = command.spawn().expect("spawn cat on the pty");
    let (master, slave) = pair.into_master_slave();

    // Sanity: the pty is alive (both the echo and cat's copy).
    master.write(b"ping\n").expect("write to master");
    read_until_count(&master, b"ping\r\n", 2, "echo and cat's copy");

    let mut slave_file = fs::File::from(slave);
    drop(master);
    wait_bounded(&mut child, "cat after the master closed");

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
