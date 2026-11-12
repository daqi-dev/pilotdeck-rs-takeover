//! Optional Git Web updater process contract, from cc7c4e6
//! `src/runtime/updateCommand.ts:6-25` and `src/cli/pilotdeck.ts:351-359`.
//! Every invocation runs a copied CLI with an isolated home and a local fake
//! Node executable. It cannot update an installation or contact a service.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Output};
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
        for directory in ["bin", "mock-bin", "home", "pilot", "elsewhere"] {
            fs::create_dir(root.join(directory)).unwrap();
        }
        let binary = root.join("bin/pilotdeck");
        fs::copy(env!("CARGO_BIN_EXE_pilotdeck"), &binary).unwrap();
        Self { root, binary }
    }

    fn updater(&self, body: &str) {
        fs::create_dir_all(self.root.join("scripts")).unwrap();
        fs::write(self.root.join("scripts/update-web.mjs"), "mock marker\n").unwrap();
        let executable = self.root.join("mock-bin/node");
        fs::write(&executable, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn run(&self, argv: &[&str]) -> Output {
        let stdout = self.root.join("stdout");
        let stderr = self.root.join("stderr");
        let mut child = Command::new(&self.binary)
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
            .stdout(fs::File::create(&stdout).unwrap())
            .stderr(fs::File::create(&stderr).unwrap())
            .process_group(0)
            .spawn()
            .expect("run isolated update CLI");
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                // This still-live child was launched as its own group leader;
                // terminate only that exact test-owned group before reaping.
                let group = rustix::process::Pid::from_raw(child.id() as i32).unwrap();
                let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
                let _ = child.wait();
                panic!("update did not finish within 30 seconds");
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
        let _ = fs::remove_dir_all(&self.root);
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
