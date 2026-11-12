//! Optional Git Web updater process contract, from cc7c4e6
//! `src/runtime/updateCommand.ts:6-25` and `src/cli/pilotdeck.ts:351-359`.
//! Every invocation runs a copied CLI with an isolated home and a local fake
//! Node executable. It cannot update an installation or contact a service.
#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct Installation {
    root: PathBuf,
    binary: PathBuf,
}

impl Installation {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "pdrs-c1-update-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).expect("unique update fixture");
        let root = fs::canonicalize(root).unwrap();
        assert!(
            !root
                .parent()
                .unwrap()
                .ancestors()
                .any(|ancestor| ancestor.join("scripts/update-web.mjs").exists()),
            "test installation has a contaminated updater ancestor: {}",
            root.display()
        );
        for directory in ["bin", "mock-bin", "home", "pilot", "elsewhere"] {
            fs::create_dir(root.join(directory)).unwrap();
        }
        let binary = root.join("bin/pilotdeck");
        // A shared read-only inode avoids repeatedly cold-loading a large
        // executable on macOS. Separate install paths still exercise root
        // discovery. Cross-filesystem fixtures retain the copy fallback.
        if fs::hard_link(env!("CARGO_BIN_EXE_pilotdeck"), &binary).is_err() {
            fs::copy(env!("CARGO_BIN_EXE_pilotdeck"), &binary).unwrap();
        }
        Self { root, binary }
    }

    fn updater(&self, body: &str) {
        fs::create_dir_all(self.root.join("scripts")).unwrap();
        fs::write(self.root.join("scripts/update-web.mjs"), "mock marker\n").unwrap();
        let executable = self.root.join("mock-bin/node");
        fs::write(&executable, format!(
            "#!/bin/sh\n/bin/ps -ww -p \"$$\" -o pgid=,lstart=,comm= > \"$PILOT_HOME/mock-runtime-$$.ps\"\nif [ -r /proc/$$/stat ]; then /bin/cat /proc/$$/stat > \"$PILOT_HOME/mock-runtime-$$.stat\"; fi\nprintf '%s' \"$$\" > \"$PILOT_HOME/mock-runtime.pid\"\n{body}\n"
        )).unwrap();
        fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn command(&self, argv: &[&str]) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .current_dir(self.root.join("elsewhere"))
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("PILOT_HOME", self.root.join("pilot"))
            .env(
                "PILOTDECK_CONFIG_PATH",
                self.root.join("pilot/pilotdeck.yaml"),
            )
            .env("PATH", self.root.join("mock-bin"))
            .env("C1_UPDATE_SENTINEL", "isolated environment")
            .arg("update")
            .args(argv)
            .process_group(0);
        command
    }

    fn run(&self, argv: &[&str]) -> Output {
        let stdout = self.root.join("stdout");
        let stderr = self.root.join("stderr");
        let mut child = self
            .command(argv)
            .stdout(fs::File::create(&stdout).unwrap())
            .stderr(fs::File::create(&stderr).unwrap())
            .spawn()
            .expect("run isolated update CLI");
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                // The updater now owns a separate group. Let the CLI perform
                // its bounded cleanup before any last-resort test cleanup.
                let pid = rustix::process::Pid::from_raw(child.id() as i32).unwrap();
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
                let cleanup_deadline = Instant::now() + Duration::from_secs(9);
                while child.try_wait().unwrap().is_none() && Instant::now() < cleanup_deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
                cleanup_mock_runtime(&self.root);
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "update did not finish within 30 seconds; pid={}, fixture={}, stdout_bytes={}, stderr_bytes={}",
                    child.id(),
                    self.root.display(),
                    fs::metadata(&stdout).map_or(0, |value| value.len()),
                    fs::metadata(&stderr).map_or(0, |value| value.len()),
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        Output {
            status,
            stdout: fs::read(stdout).unwrap(),
            stderr: fs::read(stderr).unwrap(),
        }
    }
}

impl Drop for Installation {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("retained failed update fixture: {}", self.root.display());
        } else {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

#[test]
fn update_forwards_arguments_installation_cwd_environment_and_closed_stdin() {
    let installation = Installation::new();
    installation.updater(
        r#"printf 'cwd=%s\n' "$PWD"
printf 'home=%s\npilot=%s\nmarker=%s\n' "$HOME" "$PILOT_HOME" "$C1_UPDATE_SENTINEL"
for value do printf 'arg=%s\n' "$value"; done
if read -r value; then printf 'stdin=open\n'; else printf 'stdin=eof\n'; fi"#,
    );
    let output = installation.run(&["--check", "--restart", "argument with spaces"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "cwd={}\nhome={}\npilot={}\nmarker=isolated environment\narg={}\narg=--check\narg=--restart\narg=argument with spaces\nstdin=eof\n",
            installation.root.display(),
            installation.root.join("home").display(),
            installation.root.join("pilot").display(),
            installation.root.join("scripts/update-web.mjs").display()
        )
    );
}

#[test]
fn update_merges_child_stderr_into_stdout_without_adding_newlines() {
    let installation = Installation::new();
    installation.updater("printf 'out'; printf 'err' >&2");
    let output = installation.run(&[]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text == "outerr" || text == "errout", "{text:?}");
}

#[test]
fn update_maps_noop_and_failure_exit_codes() {
    let installation = Installation::new();
    for (child_code, cli_code) in [(0, 0), (2, 0), (1, 1), (7, 1), (255, 1)] {
        installation.updater(&format!("exit {child_code}"));
        let output = installation.run(&[]);
        assert_eq!(output.status.code(), Some(cli_code), "child {child_code}");
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn update_signal_termination_is_failure_without_false_success() {
    let installation = Installation::new();
    installation.updater("kill -TERM $$");
    let output = installation.run(&[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn update_without_installation_resource_does_not_require_node() {
    let installation = Installation::new();
    let output = installation.run(&["--check"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "Update failed: This installation does not include the Git Web updater. Use application settings or update manually.\n"
    );
}

#[test]
fn update_with_missing_optional_runtime_reports_spawn_failure() {
    let installation = Installation::new();
    installation.updater("exit 0");
    fs::remove_file(installation.root.join("mock-bin/node")).unwrap();
    let output = installation.run(&[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr, b"Update failed: spawn node ENOENT\n");
}

#[test]
fn update_drains_large_output_from_both_pipes() {
    let installation = Installation::new();
    installation.updater(
        "i=0; while [ $i -lt 2000 ]; do printf 'stdout-line-012345678901234567890123456789\n'; printf 'stderr-line-012345678901234567890123456789\n' >&2; i=$((i+1)); done",
    );
    let output = installation.run(&[]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let text = String::from_utf8(output.stdout).unwrap();
    // Pipes may be interleaved at arbitrary read boundaries; byte counts are
    // independent of scheduling and catch truncation and a missing stream.
    assert_eq!(
        text.len(),
        2 * 2000 * "stdout-line-012345678901234567890123456789\n".len()
    );
    assert_eq!(text.bytes().filter(|byte| *byte == b'u').count(), 2000);
    assert_eq!(text.bytes().filter(|byte| *byte == b'r').count(), 4000);
}

#[test]
fn update_preserves_complete_unicode_from_both_output_streams() {
    let installation = Installation::new();
    installation.updater(
        "i=0; while [ $i -lt 2000 ]; do printf '中文🙂'; printf '错误信息' >&2; i=$((i+1)); done",
    );
    let output = installation.run(&[]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let text = String::from_utf8(output.stdout).expect("complete UTF-8 output");
    assert_eq!(text.chars().count(), 7 * 2000);
    for character in "中文🙂错误信息".chars() {
        assert_eq!(
            text.chars().filter(|value| *value == character).count(),
            2000
        );
    }
}

#[test]
fn update_keeps_utf8_across_read_boundaries_and_replaces_invalid_final_bytes() {
    let installation = Installation::new();
    // The complete character crosses an 8192-byte read boundary. The next
    // bytes are intentionally invalid/truncated and must remain replacements.
    installation.updater("printf '%8191s' ''; printf '中文🙂'; printf '\\377\\344\\270'");
    let output = installation.run(&[]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{}中文🙂��", " ".repeat(8191))
    );
}

/// Invoked only by the local mock Node executable below. The helper and its
/// worker deliberately ignore graceful termination, so the real CLI must
/// force-stop its own group without leaving the worker's HTTP connection open.
#[test]
#[ignore = "subprocess fixture; exercised by update cancellation tests"]
fn update_cancel_helper() {
    let directory = PathBuf::from(std::env::var_os("C1_UPDATE_HELPER_DIR").unwrap());
    let observed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register_usize(signal, observed.clone(), signal as usize).unwrap();
    }
    if std::env::var("C1_UPDATE_HELPER_ROLE").unwrap() == "worker" {
        record_identity(&directory, "worker");
        let mut connection =
            TcpStream::connect(std::env::var("C1_UPDATE_HELPER_ADDR").unwrap()).unwrap();
        connection
            .write_all(b"GET /apply HTTP/1.1\r\nHost: local-mock\r\n\r\n")
            .unwrap();
        fs::write(directory.join("worker.pid"), std::process::id().to_string()).unwrap();
        loop {
            let signal = observed.swap(0, Ordering::SeqCst);
            if signal != 0 {
                fs::write(directory.join("worker.signal"), signal.to_string()).unwrap();
            }
            // Keep the connection owned by this exact descendant alive.
            assert!(connection.peer_addr().is_ok());
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    record_identity(&directory, "updater");
    fs::write(
        directory.join("updater.pid"),
        std::process::id().to_string(),
    )
    .unwrap();
    let mut worker = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "update_cancel_helper",
            "--nocapture",
        ])
        .env("C1_UPDATE_HELPER_ROLE", "worker")
        .spawn()
        .unwrap();
    loop {
        assert!(
            worker.try_wait().unwrap().is_none(),
            "worker unexpectedly exited"
        );
        let signal = observed.swap(0, Ordering::SeqCst);
        if signal != 0 {
            fs::write(directory.join("updater.signal"), signal.to_string()).unwrap();
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct ProcessIdentity {
    pid: i32,
    group: i32,
    birth: String,
    command: String,
}

impl ProcessIdentity {
    fn inspect(pid: i32) -> Option<Self> {
        let output = Command::new("/bin/ps")
            .args(["-ww", "-p", &pid.to_string(), "-o", "pgid=,lstart=,comm="])
            .output()
            .ok()?;
        let text = String::from_utf8(output.stdout).ok()?;
        #[cfg(target_os = "linux")]
        let birth_override = Some({
            let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            stat.rsplit_once(')')?
                .1
                .split_whitespace()
                .nth(19)?
                .to_owned()
        });
        #[cfg(not(target_os = "linux"))]
        let birth_override = None;
        Self::from_snapshot(pid, &text, birth_override)
    }

    fn from_snapshot(pid: i32, text: &str, birth_override: Option<String>) -> Option<Self> {
        let fields: Vec<_> = text.split_whitespace().collect();
        if fields.len() < 7 {
            return None;
        }
        let group = fields[0].parse().ok()?;
        let birth = birth_override.unwrap_or_else(|| fields[1..6].join(" "));
        Some(Self {
            pid,
            group,
            birth,
            command: fields[6..].join(" "),
        })
    }

    fn still_matches(&self) -> bool {
        Self::inspect(self.pid).is_some_and(|current| {
            // exec changes comm but keeps the owned process's birth and group.
            current.pid == self.pid && current.group == self.group && current.birth == self.birth
        })
    }
}

fn cleanup_mock_runtime(directory: &std::path::Path) {
    let pilot = directory.join("pilot");
    let Some(pid) = fs::read_to_string(pilot.join("mock-runtime.pid"))
        .ok()
        .and_then(|text| text.parse::<i32>().ok())
    else {
        return;
    };
    let Ok(snapshot) = fs::read_to_string(pilot.join(format!("mock-runtime-{pid}.ps"))) else {
        return;
    };
    #[cfg(target_os = "linux")]
    let birth = fs::read_to_string(pilot.join(format!("mock-runtime-{pid}.stat")))
        .ok()
        .and_then(|stat| {
            Some(
                stat.rsplit_once(')')?
                    .1
                    .split_whitespace()
                    .nth(19)?
                    .to_owned(),
            )
        });
    #[cfg(target_os = "linux")]
    if birth.is_none() {
        return;
    }
    #[cfg(not(target_os = "linux"))]
    let birth = None;
    if let Some(identity) = ProcessIdentity::from_snapshot(pid, &snapshot, birth)
        && identity.group == identity.pid
        && identity.still_matches()
    {
        let _ = rustix::process::kill_process_group(
            rustix::process::Pid::from_raw(identity.group).unwrap(),
            rustix::process::Signal::KILL,
        );
    }
}

fn record_identity(directory: &std::path::Path, name: &str) {
    let identity =
        ProcessIdentity::inspect(std::process::id() as i32).expect("helper process identity");
    let temporary = directory.join(format!("{name}.identity.tmp"));
    fs::write(&temporary, serde_json::to_vec(&identity).unwrap()).unwrap();
    fs::rename(temporary, directory.join(format!("{name}.identity.json"))).unwrap();
}

struct CancelCleanup {
    child: Child,
    directory: PathBuf,
    complete: bool,
}

impl Drop for CancelCleanup {
    fn drop(&mut self) {
        if self.complete {
            return;
        }
        // Covers a failure while the mock shell is exec'ing the helper,
        // before that helper has published its more specific identity files.
        cleanup_mock_runtime(&self.directory);
        let identity = |name: &str| -> Option<ProcessIdentity> {
            serde_json::from_slice(
                &fs::read(self.directory.join(format!("{name}.identity.json"))).ok()?,
            )
            .ok()
        };
        // No cached PGID is signalled after a failed assertion. The fixture's
        // own process records its birth identity before announcing readiness;
        // revalidate that identity before cleanup, including on early panic.
        let updater = identity("updater");
        if let Some(updater) = updater.filter(|identity| identity.still_matches())
            && updater.group == updater.pid
        {
            let _ = rustix::process::kill_process_group(
                rustix::process::Pid::from_raw(updater.group).unwrap(),
                rustix::process::Signal::KILL,
            );
        }
        if let Some(worker) = identity("worker").filter(|identity| identity.still_matches()) {
            let _ = rustix::process::kill_process(
                rustix::process::Pid::from_raw(worker.pid).unwrap(),
                rustix::process::Signal::KILL,
            );
        }
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn assert_update_cancellation(signal: rustix::process::Signal, process_group: bool) {
    let installation = Installation::new();
    let helper = std::env::current_exe().unwrap();
    let quoted_helper = format!("'{}'", helper.to_str().unwrap().replace('\'', "'\\''"));
    installation.updater(&format!(
        "exec {quoted_helper} --ignored --exact update_cancel_helper --nocapture"
    ));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let child = installation
        .command(&[])
        .env("C1_UPDATE_HELPER_DIR", &installation.root)
        .env("C1_UPDATE_HELPER_ROLE", "updater")
        .env(
            "C1_UPDATE_HELPER_ADDR",
            listener.local_addr().unwrap().to_string(),
        )
        .stdout(fs::File::create(installation.root.join("stdout")).unwrap())
        .stderr(fs::File::create(installation.root.join("stderr")).unwrap())
        .spawn()
        .unwrap();
    let cli_pid = rustix::process::Pid::from_raw(child.id() as i32).unwrap();
    let mut cleanup = CancelCleanup {
        child,
        directory: installation.root.clone(),
        complete: false,
    };
    let ready_deadline = Instant::now() + Duration::from_secs(30);
    let mut updater_pid = None;
    let mut worker_pid = None;
    let mut connection = loop {
        updater_pid = updater_pid.or_else(|| read_pid(&installation.root.join("updater.pid")));
        worker_pid = worker_pid.or_else(|| read_pid(&installation.root.join("worker.pid")));
        if updater_pid.is_some() && worker_pid.is_some() {
            match listener.accept() {
                Ok((connection, _)) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("accept mock update connection: {error}"),
            }
        }
        if Instant::now() >= ready_deadline || cleanup.child.try_wait().unwrap().is_some() {
            panic!(
                "update cancellation fixture did not become ready: {}",
                installation.root.display()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let updater_pid = updater_pid.unwrap();
    let worker_pid = worker_pid.unwrap();
    connection.set_nonblocking(false).unwrap();
    connection
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut request = [0_u8; 256];
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        let length = connection.read(&mut request).unwrap();
        assert!(length > 0, "worker closed before its request completed");
        header.extend_from_slice(&request[..length]);
    }
    if process_group {
        rustix::process::kill_process_group(cli_pid, signal).unwrap();
    } else {
        rustix::process::kill_process(cli_pid, signal).unwrap();
    }
    let cancelled_at = Instant::now();
    let status = loop {
        if let Some(status) = cleanup.child.try_wait().unwrap() {
            break status;
        }
        if cancelled_at.elapsed() >= Duration::from_secs(10) {
            panic!(
                "update cancellation exceeded ten seconds: {}",
                installation.root.display()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(
        status.signal(),
        Some(signal.as_raw()),
        "parent cancellation signal"
    );
    assert_eq!(
        fs::read_to_string(installation.root.join("updater.signal")).unwrap(),
        signal.as_raw().to_string()
    );
    assert_eq!(
        fs::read_to_string(installation.root.join("worker.signal")).unwrap(),
        signal.as_raw().to_string()
    );
    // This EOF is observed before any test cleanup. A killed CLI with a live
    // updater/worker would keep this mock apply connection open and fail.
    assert_eq!(
        connection.read(&mut request).unwrap(),
        0,
        "mock apply connection still open"
    );
    assert!(
        !process_is_live(updater_pid),
        "updater survived cancellation"
    );
    assert!(!process_is_live(worker_pid), "worker survived cancellation");
    cleanup.complete = true;
}

fn read_pid(path: &std::path::Path) -> Option<rustix::process::Pid> {
    fs::read_to_string(path)
        .ok()?
        .parse()
        .ok()
        .and_then(rustix::process::Pid::from_raw)
}

fn process_is_live(pid: rustix::process::Pid) -> bool {
    let output = Command::new("/bin/ps")
        .args(["-p", &pid.as_raw_nonzero().to_string(), "-o", "stat="])
        .output()
        .expect("observe exact test-owned process");
    let state = String::from_utf8_lossy(&output.stdout);
    let state = state.trim();
    !state.is_empty() && !state.starts_with('Z')
}

#[test]
fn update_cancels_owned_worker_when_cli_pid_receives_sigint() {
    assert_update_cancellation(rustix::process::Signal::INT, false);
}

#[test]
fn update_cancels_owned_worker_when_cli_pid_receives_sigterm() {
    assert_update_cancellation(rustix::process::Signal::TERM, false);
}

#[test]
fn update_cancels_owned_worker_when_foreground_group_receives_sigint() {
    assert_update_cancellation(rustix::process::Signal::INT, true);
}

#[test]
fn update_cancels_owned_worker_when_foreground_group_receives_sigterm() {
    assert_update_cancellation(rustix::process::Signal::TERM, true);
}
