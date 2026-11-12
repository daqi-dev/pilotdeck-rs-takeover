//! Real process startup with an inaccessible cwd must fail normally, never TODO.
#![cfg(unix)]
use std::process::Command;

#[test]
fn deleted_startup_directory_keeps_errno_and_exit_one() {
    let root = std::env::temp_dir().join(format!("pilotdeck-startup-cwd-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("home/.pilotdeck")).unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "deleted_cwd_helper", "--nocapture"])
        .env("PDRS_STARTUP_CWD_FIXTURE", &root)
        .env("HOME", root.join("home"))
        .env("PILOT_HOME", root.join("home/.pilotdeck"))
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(root);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "spawned only by the isolated cwd startup test"]
fn deleted_cwd_helper() {
    let root = std::path::PathBuf::from(
        std::env::var_os("PDRS_STARTUP_CWD_FIXTURE").expect("owned fixture"),
    );
    let cwd = root.join("removed");
    std::fs::create_dir(&cwd).unwrap();
    std::env::set_current_dir(&cwd).unwrap();
    std::fs::remove_dir(&cwd).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_pilotdeck"))
        .arg("--help")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        b"ENOENT: no such file or directory, uv_cwd\n"
    );
}
