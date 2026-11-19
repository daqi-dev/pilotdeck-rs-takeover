//! installation.* end-to-end suite (t021).
//!
//! Every scenario drives the real `agentcpm-pd` binary with the embedded
//! gateway, a controlled local HTTP channel (009 §8.3), and an isolated
//! `AGENTCPM_PD_INSTALL_ROOT` install root — no touch of the developer's
//! real `~/.local`. The packages are built from the locally compiled test
//! binary through the product's own packaging library
//! (`public_v1::domains::installation::release`), so the whole loop
//! (manifest validation → verified download → unpack → generation commit →
//! atomic switch → restart handoff) runs the production code paths.
//!
//! The cancel/pause scenario intentionally races a slow (trickled) channel
//! body; the margins are generous (a ~6 s download vs. ~1 s control
//! round-trips).

#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256, Sha512};

use pilotdeck_gateway::public_v1::domains::installation::release::{
    self as release_lib, PackageFile,
};

use common_v1::*;

// ---------------------------------------------------------------------------
// Harness: CLI with an isolated install root + the channel server
// ---------------------------------------------------------------------------

/// One isolated environment: data root + install root (both under a temp
/// dir). Drop kills the serving gateway (the restart successor writes the
/// same ready.json the auto-started one did) and removes the directory.
struct Env {
    base: PathBuf,
    home: PathBuf,
    install_root: PathBuf,
}

impl Env {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "t021-e2e-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));
        let home = base.join("home");
        let install_root = base.join("install-root");
        std::fs::create_dir_all(&home).expect("mkdir home");
        std::fs::create_dir_all(&install_root).expect("mkdir install root");
        Self {
            base,
            home,
            install_root,
        }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if let Some(pid) = read_ready_pid(&self.home) {
            if let Some(raw) = rustix::process::Pid::from_raw(pid as i32) {
                let _ = rustix::process::kill_process(raw, rustix::process::Signal::KILL);
            }
        }
        std::thread::sleep(Duration::from_millis(60));
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// Runs one CLI invocation with the isolated home AND install root (the
/// running gateway inherits the env from the first invocation that starts
/// it, so every call in a scenario must go through here).
fn cli(home: &Path, install_root: &Path, args: &[&str]) -> CliOutcome {
    let mut cmd = Command::new(BIN);
    cmd.env("AGENTCPM_PD_HOME", home)
        .env("AGENTCPM_PD_INSTALL_ROOT", install_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(args);
    let deadline = Instant::now() + CLI_TIMEOUT;
    let mut child = cmd.spawn().expect("spawn agentcpm-pd");
    let stdout_pipe = child.stdout.take().expect("piped stdout");
    let stderr_pipe = child.stderr.take().expect("piped stderr");
    let stdout = std::thread::spawn(move || read_all(stdout_pipe));
    let stderr = std::thread::spawn(move || read_all(stderr_pipe));
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait agentcpm-pd") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("cli invocation timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    CliOutcome {
        code: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout.join().expect("read stdout")).into_owned(),
        stderr: String::from_utf8_lossy(&stderr.join().expect("read stderr")).into_owned(),
    }
}

fn read_all(mut pipe: impl std::io::Read) -> Vec<u8> {
    let mut buffer = Vec::new();
    let _ = std::io::Read::read_to_end(&mut pipe, &mut buffer);
    buffer
}

/// Spawns a CLI invocation without waiting, so control actions can race it.
fn spawn_cli(home: &Path, install_root: &Path, args: &[&str]) -> std::process::Child {
    let mut cmd = Command::new(BIN);
    cmd.env("AGENTCPM_PD_HOME", home)
        .env("AGENTCPM_PD_INSTALL_ROOT", install_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(args);
    cmd.spawn().expect("spawn agentcpm-pd")
}

fn read_child(child: std::process::Child) -> CliOutcome {
    let mut child = child;
    let stdout_pipe = child.stdout.take().expect("piped stdout");
    let stderr_pipe = child.stderr.take().expect("piped stderr");
    let stdout = std::thread::spawn(move || read_all(stdout_pipe));
    let stderr = std::thread::spawn(move || read_all(stderr_pipe));
    let deadline = Instant::now() + CLI_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait agentcpm-pd") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("cli invocation timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    CliOutcome {
        code: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout.join().expect("read stdout")).into_owned(),
        stderr: String::from_utf8_lossy(&stderr.join().expect("read stderr")).into_owned(),
    }
}

async fn json_cli(home: &Path, install_root: &Path, args: &[&str]) -> Value {
    let out = cli(home, install_root, args);
    let method = format!(
        "{}.{}",
        args[0].replace('-', "_"),
        args[1].replace('-', "_")
    );
    expect_ok(&method, &out)
}

/// The controlled local channel (009 §8.3): `release.json` + assets at
/// their manifest names. `trickle_ms` > 0 trickles the asset body out
/// slowly so control actions land mid-download.
async fn spawn_channel(
    manifest_bytes: Vec<u8>,
    assets: Vec<(String, Vec<u8>)>,
    trickle_ms: u64,
) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let manifest_bytes = manifest_bytes.clone();
            let assets = assets.clone();
            let trickle_ms = trickle_ms;
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut head = Vec::new();
                let mut chunk = [0u8; 2048];
                loop {
                    let read = socket.read(&mut chunk).await.unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    head.extend_from_slice(&chunk[..read]);
                    if head.windows(4).any(|window| window == b"\r\n\r\n") || head.len() > 64 * 1024
                    {
                        break;
                    }
                }
                let request_line = String::from_utf8_lossy(&head);
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .trim_start_matches('/')
                    .to_owned();
                let (body, content_type) = if path == "release.json" {
                    (manifest_bytes.clone(), "application/json")
                } else {
                    match assets.iter().find(|(name, _)| *name == path) {
                        Some((_, bytes)) => (bytes.clone(), "application/gzip"),
                        None => (Vec::new(), "application/octet-stream"),
                    }
                };
                let status = if body.is_empty() && path != "release.json" {
                    "404 Not Found"
                } else {
                    "200 OK"
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                if socket.write_all(response.as_bytes()).await.is_err() {
                    return;
                }
                if trickle_ms > 0 && path != "release.json" {
                    for piece in body.chunks(8 * 1024) {
                        if socket.write_all(piece).await.is_err() {
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(trickle_ms)).await;
                    }
                } else if socket.write_all(&body).await.is_err() {
                    return;
                }
                let _ = socket.shutdown().await;
            });
        }
    });
    format!("http://{addr}")
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn sha512_base64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(Sha512::digest(bytes))
}

/// One complete, valid release package for the CURRENT platform. When
/// `with_real_binary` the packaged `bin/agentcpm-pd` is the actual test
/// binary, so a restart can really relaunch from the new generation.
fn build_package(
    scratch: &Path,
    version: &str,
    tag: &str,
    source_sha: &str,
    with_real_binary: bool,
) -> (Value, Vec<u8>, String, String) {
    let dir = scratch.join(format!("pkg-{version}-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::create_dir_all(dir.join("manifest")).unwrap();
    let build_id = release_lib::build_id(version, source_sha, "ffffffffffff");
    let binary = dir.join("bin").join("agentcpm-pd");
    if with_real_binary {
        std::fs::copy(BIN, &binary).unwrap();
    } else {
        std::fs::write(&binary, b"#!/bin/sh\necho agentcpm-pd e2e t021\n").unwrap();
    }
    let manifest_json = json!({
        "schemaVersion": 2,
        "product": "agentcpm-pd",
        "version": version,
        "buildId": build_id,
        "tag": tag,
        "date": "2026-11-18",
        "buildTime": "2026-11-18T00:00:00Z",
        "sourceSha": source_sha,
        "contractDigest": "ffffffffffff",
        "assets": [],
    });
    std::fs::write(
        dir.join("manifest").join("release.json"),
        manifest_json.to_string(),
    )
    .unwrap();
    let sums = release_lib::render_sha256sums(&[
        ("bin/agentcpm-pd".to_owned(), binary.clone()),
        (
            "manifest/release.json".to_owned(),
            dir.join("manifest").join("release.json"),
        ),
    ])
    .unwrap();
    std::fs::write(dir.join("SHA256SUMS.txt"), &sums).unwrap();

    let (platform, arch) = release_lib::current_platform_pair();
    let asset_name = format!("agentcpm-pd-{version}-{platform}-{arch}.tar.gz");
    let archive_path = dir.join(&asset_name);
    let mut files = vec![
        PackageFile {
            relative_path: "bin/agentcpm-pd".to_owned(),
            source: binary,
            mode: 0o755,
        },
        PackageFile {
            relative_path: "manifest/release.json".to_owned(),
            source: dir.join("manifest").join("release.json"),
            mode: 0o644,
        },
        PackageFile {
            relative_path: "SHA256SUMS.txt".to_owned(),
            source: dir.join("SHA256SUMS.txt"),
            mode: 0o644,
        },
    ];
    if with_real_binary {
        // Also ship the optional compat shim slot: a second executable from
        // the same build (009 §2.2) — here the same test binary under the
        // shim name. The sums are re-rendered over the full file set
        // before the archive is written.
        let shim = dir.join("bin").join("pilotdeck");
        std::fs::write(&shim, std::fs::read(BIN).unwrap()).unwrap();
        let final_sums = release_lib::render_sha256sums(&[
            (
                "bin/agentcpm-pd".to_owned(),
                dir.join("bin").join("agentcpm-pd"),
            ),
            (
                "bin/pilotdeck".to_owned(),
                dir.join("bin").join("pilotdeck"),
            ),
            (
                "manifest/release.json".to_owned(),
                dir.join("manifest").join("release.json"),
            ),
        ])
        .unwrap();
        std::fs::write(dir.join("SHA256SUMS.txt"), &final_sums).unwrap();
        files.insert(
            1,
            PackageFile {
                relative_path: "bin/pilotdeck".to_owned(),
                source: shim,
                mode: 0o755,
            },
        );
    }
    release_lib::write_release_package(&archive_path, &files).unwrap();
    let archive = std::fs::read(&archive_path).unwrap();

    let manifest_json = json!({
        "schemaVersion": 2,
        "product": "agentcpm-pd",
        "version": version,
        "buildId": build_id,
        "tag": tag,
        "date": "2026-11-18",
        "buildTime": "2026-11-18T00:00:00Z",
        "sourceSha": source_sha,
        "contractDigest": "ffffffffffff",
        "assets": [{
            "name": asset_name,
            "size": archive.len(),
            "sha256": sha256_hex(&archive),
            "sha512": sha512_base64(&archive),
            "platform": platform,
            "arch": arch,
            "kind": "archive",
        }],
    });
    (manifest_json, archive, asset_name, build_id)
}

fn write_channel_config(home: &Path, url: &str) {
    std::fs::create_dir_all(home.join("installation")).unwrap();
    std::fs::write(
        home.join("installation").join("channel.json"),
        json!({"channels": {"stable": url}}).to_string(),
    )
    .unwrap();
}

fn init_home(home: &Path, install_root: &Path) {
    let out = cli(
        home,
        install_root,
        &["instance", "init", "--output", "json"],
    );
    expect_ok("instance.initialize", &out);
}

/// Polls `operations get` until the operation is terminal. `--no-start`
/// keeps the poll from racing a restart handoff with an auto-started
/// gateway (only the handoff successor may claim the instance during the
/// restart window), and transient connection failures are retried.
fn wait_operation_terminal(home: &Path, install_root: &Path, operation_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let out = cli(
            home,
            install_root,
            &[
                "operations",
                "get",
                "--output",
                "json",
                "--no-start",
                "--operation-id",
                operation_id,
            ],
        );
        if out.code == 0
            && let Ok(value) = serde_json::from_str::<Value>(
                out.stdout
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or(""),
            )
            && value["outcome"] == "succeeded"
        {
            let state = value["result"]["operation"]["state"].as_str().unwrap_or("");
            if matches!(
                state,
                "succeeded" | "failed" | "cancelled" | "interrupted" | "result_unknown"
            ) {
                return value["result"]["operation"].clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "operation `{operation_id}` never became terminal; last output:\n{}\n{}",
            out.stdout,
            out.stderr
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn submit_and_wait(home: &Path, install_root: &Path, args: &[&str], operation: &str) -> Value {
    let out = cli(home, install_root, args);
    let receipt = expect_ok(operation, &out);
    let operation_id = receipt["operationId"]
        .as_str()
        .expect("operationId")
        .to_owned();
    wait_operation_terminal(home, install_root, &operation_id)
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// `installation.inspect`: the local identity report, source mode before
/// any install, all fields per the contract.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inspect_reports_the_source_identity() {
    let env = Env::new("inspect");
    init_home(&env.home, &env.install_root);

    let result = json_cli(
        &env.home,
        &env.install_root,
        &["installation", "inspect", "--output", "json"],
    )
    .await;
    assert_eq!(result["productName"], "agentcpm-pd");
    assert_eq!(result["installationMode"], "source");
    assert_eq!(result["updateSupported"], false);
    assert_eq!(result["reasonCode"], "development");
    assert_eq!(result["activeUpdateOperationId"], Value::Null);
    assert!(result["buildId"].as_str().is_some_and(|id| !id.is_empty()));
    assert!(result["version"].as_str().is_some_and(|v| !v.is_empty()));
    assert!(
        result["platform"]
            .as_str()
            .is_some_and(|platform| platform.contains('/'))
    );

    // An explicit --installation-root overrides the environment.
    let other = Env::new("inspect-other");
    let result = json_cli(
        &env.home,
        &env.install_root,
        &[
            "installation",
            "inspect",
            "--output",
            "json",
            "--installation-root",
            other.install_root.to_str().unwrap(),
        ],
    )
    .await;
    assert_eq!(result["installationMode"], "source");
}

/// `installation.check`: a configured channel reports the update; an
/// unconfigured channel is a usage error (never a hang, never a fake
/// green).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_reports_channel_updates_and_configuration_errors() {
    let env = Env::new("check");
    init_home(&env.home, &env.install_root);
    let scratch = env.install_root.parent().unwrap().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();

    // Unconfigured channel: INVALID_ARGUMENT.
    let out = cli(
        &env.home,
        &env.install_root,
        &[
            "installation",
            "check",
            "--output",
            "json",
            "--request-key",
            "e2e-check-0",
        ],
    );
    assert_eq!(
        expect_failed_code("installation.check", &out),
        "INVALID_ARGUMENT"
    );

    // A dead channel URL: UPSTREAM_ERROR in the operation completion.
    write_channel_config(&env.home, "http://127.0.0.1:1");
    let op = submit_and_wait(
        &env.home,
        &env.install_root,
        &[
            "installation",
            "check",
            "--output",
            "json",
            "--request-key",
            "e2e-check-1",
        ],
        "installation.check",
    );
    assert_eq!(op["state"], "failed");
    assert_eq!(op["result"]["error"]["code"], "UPSTREAM_ERROR");

    // A live channel with a newer release: updateAvailable + the release.
    let (manifest, archive, asset_name, _build_id) = build_package(
        &scratch,
        "9.9.9",
        "v2026.11.18",
        "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        false,
    );
    let base = spawn_channel(
        manifest.to_string().into_bytes(),
        vec![(asset_name.clone(), archive.clone())],
        0,
    )
    .await;
    write_channel_config(&env.home, &base);

    let op = submit_and_wait(
        &env.home,
        &env.install_root,
        &[
            "installation",
            "check",
            "--output",
            "json",
            "--request-key",
            "e2e-check-2",
        ],
        "installation.check",
    );
    assert_eq!(op["state"], "succeeded");
    let result = &op["result"];
    assert_eq!(result["updateAvailable"], true);
    let releases = result["releases"].as_array().unwrap();
    assert_eq!(releases.len(), 1);
    assert_eq!(releases[0]["releaseId"], "v2026.11.18");
    assert_eq!(releases[0]["version"], "9.9.9");
    assert_eq!(releases[0]["verified"], true);
    assert_eq!(releases[0]["requiresRestart"], true);
}

fn expect_failed_code(method: &str, out: &CliOutcome) -> String {
    assert_ne!(out.code, 0, "`{method}` must exit non-zero");
    let line = out
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .last()
        .unwrap_or("");
    let value: Value = serde_json::from_str(line)
        .unwrap_or_else(|error| panic!("`{method}` stdout is not JSON ({error}): {line}"));
    assert_eq!(value["outcome"], "failed", "`{method}` envelope: {value}");
    value["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// The apply transaction: verified install, atomic switch, inspect flips to
/// managed, then an explicit rollback to the retained older generation
/// (009 §3.3 / §5.1) — including the BUILD_CONFLICT CAS.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_installs_switches_and_rolls_back() {
    let env = Env::new("apply");
    init_home(&env.home, &env.install_root);
    let scratch = env.install_root.parent().unwrap().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();

    let (manifest_v1, archive_v1, asset_v1, build_v1) = build_package(
        &scratch,
        "8.8.8",
        "v2026.10.01",
        "dddddddddddddddddddddddddddddddddddddddd",
        false,
    );
    let (manifest_v2, archive_v2, asset_v2, build_v2) = build_package(
        &scratch,
        "9.9.9",
        "v2026.11.18",
        "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        false,
    );

    // v1 first.
    let base = spawn_channel(
        manifest_v1.to_string().into_bytes(),
        vec![(asset_v1.clone(), archive_v1.clone())],
        0,
    )
    .await;
    write_channel_config(&env.home, &base);
    let op = submit_and_wait(
        &env.home,
        &env.install_root,
        &[
            "installation",
            "apply",
            "--output",
            "json",
            "--request-key",
            "e2e-apply-1",
            "--release-id",
            "v2026.10.01",
            "--expected-build-id",
            &build_v1,
        ],
        "installation.apply",
    );
    assert_eq!(op["state"], "succeeded");
    assert_eq!(op["result"]["buildId"], build_v1.as_str());
    assert_eq!(op["result"]["restartRequired"], true);

    // inspect now describes the managed installation.
    let result = json_cli(
        &env.home,
        &env.install_root,
        &["installation", "inspect", "--output", "json"],
    )
    .await;
    assert_eq!(result["installationMode"], "managed");
    assert_eq!(result["version"], "8.8.8");
    assert_eq!(result["buildId"], build_v1.as_str());
    assert_eq!(result["updateSupported"], true);
    assert_eq!(result["reasonCode"], Value::Null);

    // The channel moves to v2: check sees the update, apply installs it.
    let base = spawn_channel(
        manifest_v2.to_string().into_bytes(),
        vec![(asset_v2.clone(), archive_v2.clone())],
        0,
    )
    .await;
    write_channel_config(&env.home, &base);

    // BUILD_CONFLICT first: a stale expectedBuildId refuses (the CAS).
    let op = submit_and_wait(
        &env.home,
        &env.install_root,
        &[
            "installation",
            "apply",
            "--output",
            "json",
            "--request-key",
            "e2e-apply-conflict",
            "--release-id",
            "v2026.11.18",
            "--expected-build-id",
            &build_v1,
        ],
        "installation.apply",
    );
    assert_eq!(op["state"], "failed");
    assert_eq!(op["result"]["error"]["code"], "BUILD_CONFLICT");

    // A corrupted body: INTEGRITY_ERROR, the running install untouched.
    let mut corrupted = archive_v2.clone();
    let last = corrupted.len() - 1;
    corrupted[last] = corrupted[last].wrapping_add(1);
    let base_bad = spawn_channel(
        manifest_v2.to_string().into_bytes(),
        vec![(asset_v2.clone(), corrupted)],
        0,
    )
    .await;
    write_channel_config(&env.home, &base_bad);
    let op = submit_and_wait(
        &env.home,
        &env.install_root,
        &[
            "installation",
            "apply",
            "--output",
            "json",
            "--request-key",
            "e2e-apply-integrity",
            "--release-id",
            "v2026.11.18",
            "--expected-build-id",
            &build_v2,
        ],
        "installation.apply",
    );
    assert_eq!(op["state"], "failed");
    assert_eq!(op["result"]["error"]["code"], "INTEGRITY_ERROR");
    let result = json_cli(
        &env.home,
        &env.install_root,
        &["installation", "inspect", "--output", "json"],
    )
    .await;
    assert_eq!(
        result["buildId"],
        build_v1.as_str(),
        "the old version survives a failed update"
    );

    // The good v2: install, previous generation retained.
    write_channel_config(&env.home, &base);
    let op = submit_and_wait(
        &env.home,
        &env.install_root,
        &[
            "installation",
            "apply",
            "--output",
            "json",
            "--request-key",
            "e2e-apply-2",
            "--release-id",
            "v2026.11.18",
            "--expected-build-id",
            &build_v2,
        ],
        "installation.apply",
    );
    assert_eq!(op["state"], "succeeded");
    assert_eq!(op["result"]["previousBuildId"], build_v1.as_str());
    assert_eq!(op["result"]["recoveryAvailable"], true);
    assert!(
        op["result"]["retainedGenerations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == build_v1.as_str())
    );

    // Explicit rollback (009 §5.1): the old releaseId resolves from the
    // retained local generation — the channel now only serves v2, and no
    // network is needed.
    let op = submit_and_wait(
        &env.home,
        &env.install_root,
        &[
            "installation",
            "apply",
            "--output",
            "json",
            "--request-key",
            "e2e-apply-rollback",
            "--release-id",
            "v2026.10.01",
            "--expected-build-id",
            &build_v1,
        ],
        "installation.apply",
    );
    assert_eq!(op["state"], "succeeded");
    assert_eq!(op["result"]["buildId"], build_v1.as_str());
    let result = json_cli(
        &env.home,
        &env.install_root,
        &["installation", "inspect", "--output", "json"],
    )
    .await;
    assert_eq!(result["buildId"], build_v1.as_str());
}

/// The control plane over a slow download: pause parks it, resume
/// continues it, cancel aborts with no half installation (009 §3.4).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_update_pauses_resumes_and_cancels() {
    let env = Env::new("control");
    init_home(&env.home, &env.install_root);
    let scratch = env.install_root.parent().unwrap().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();

    let (manifest, archive, asset_name, build_id) = build_package(
        &scratch,
        "9.9.9",
        "v2026.11.18",
        "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        false,
    );
    // A big body trickled out slowly: ~8 s of download time, so the control
    // round-trips land comfortably inside it.
    let big = [&archive[..], &vec![0u8; 1024 * 1024][..]].concat();
    let mut manifest = manifest;
    manifest["assets"][0]["size"] = json!(big.len());
    manifest["assets"][0]["sha256"] = json!(sha256_hex(&big));
    manifest["assets"][0]["sha512"] = json!(sha512_base64(&big));
    let base = spawn_channel(
        manifest.to_string().into_bytes(),
        vec![(asset_name.clone(), big)],
        60,
    )
    .await;
    write_channel_config(&env.home, &base);

    // Start the apply without waiting for it.
    let child = spawn_cli(
        &env.home,
        &env.install_root,
        &[
            "installation",
            "apply",
            "--output",
            "json",
            "--request-key",
            "e2e-control-1",
            "--release-id",
            "v2026.11.18",
            "--expected-build-id",
            &build_id,
        ],
    );

    // Wait for the download phase, then pause mid-flight.
    let operation_id = wait_running_operation(&env);
    let result = json_cli(
        &env.home,
        &env.install_root,
        &[
            "installation",
            "control-update",
            "--output",
            "json",
            "--request-key",
            "e2e-pause-1",
            "--operation-id",
            &operation_id,
            "--action",
            "pause",
        ],
    )
    .await;
    assert_eq!(result["accepted"], true);
    assert_eq!(result["state"], "paused");
    assert_eq!(result["allowedActions"], json!(["resume", "cancel"]));

    // A pause while verification started would be INVALID_STATE — but the
    // paused download has not reached it; resume continues.
    let result = json_cli(
        &env.home,
        &env.install_root,
        &[
            "installation",
            "control-update",
            "--output",
            "json",
            "--request-key",
            "e2e-resume-1",
            "--operation-id",
            &operation_id,
            "--action",
            "resume",
        ],
    )
    .await;
    assert_eq!(result["state"], "downloading");

    // Cancel mid-download: no half installation survives.
    let result = json_cli(
        &env.home,
        &env.install_root,
        &[
            "installation",
            "control-update",
            "--output",
            "json",
            "--request-key",
            "e2e-cancel-1",
            "--operation-id",
            &operation_id,
            "--action",
            "cancel",
        ],
    )
    .await;
    assert_eq!(result["accepted"], true);
    assert_eq!(result["state"], "cancelling");

    let out = read_child(child);
    let receipt = expect_ok("installation.apply", &out);
    assert_eq!(receipt["operationId"], operation_id.as_str());

    let op = wait_operation_terminal(&env.home, &env.install_root, &operation_id);
    assert_eq!(op["state"], "cancelled");
    let result = json_cli(
        &env.home,
        &env.install_root,
        &["installation", "inspect", "--output", "json"],
    )
    .await;
    assert_eq!(
        result["installationMode"], "source",
        "cancel leaves no installation behind"
    );

    // Control of an unknown operation: NOT_FOUND.
    let out = cli(
        &env.home,
        &env.install_root,
        &[
            "installation",
            "control-update",
            "--output",
            "json",
            "--request-key",
            "e2e-ctl-none",
            "--operation-id",
            "op-none",
            "--action",
            "cancel",
        ],
    );
    assert_eq!(
        expect_failed_code("installation.control_update", &out),
        "NOT_FOUND"
    );
}

/// Finds the running installation.apply operation (the just-started one).
fn wait_running_operation(env: &Env) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let out = cli(
            &env.home,
            &env.install_root,
            &[
                "operations",
                "list",
                "--output",
                "json",
                "--scope",
                "{\"kind\":\"instance\"}",
            ],
        );
        if out.code == 0 {
            let value: Value = serde_json::from_str(
                out.stdout
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or(""),
            )
            .unwrap_or(Value::Null);
            if let Some(items) = value["result"]["items"].as_array() {
                for item in items {
                    if item["operationType"] == "installation.apply"
                        && item["state"] == "running"
                        && let Some(id) = item["operationId"].as_str()
                    {
                        return id.to_owned();
                    }
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "no running installation.apply found"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `installation.restart`: the full process handoff — drain, successor
/// spawn from the new generation, self-termination, operation completed by
/// the successor, and the new gateway serving afterwards (009 §7).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_replaces_the_gateway_process() {
    let env = Env::new("restart");
    init_home(&env.home, &env.install_root);
    let scratch = env.install_root.parent().unwrap().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();

    // A package carrying the REAL test binary, so the successor genuinely
    // serves from the new generation.
    let (manifest, archive, asset_name, build_id) = build_package(
        &scratch,
        "9.9.9",
        "v2026.11.18",
        "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        true,
    );
    let base = spawn_channel(
        manifest.to_string().into_bytes(),
        vec![(asset_name.clone(), archive.clone())],
        0,
    )
    .await;
    write_channel_config(&env.home, &base);

    let op = submit_and_wait(
        &env.home,
        &env.install_root,
        &[
            "installation",
            "apply",
            "--output",
            "json",
            "--request-key",
            "e2e-restart-apply",
            "--release-id",
            "v2026.11.18",
            "--expected-build-id",
            &build_id,
        ],
        "installation.apply",
    );
    assert_eq!(op["state"], "succeeded");

    // Warm gateway: note the current pid, then restart.
    let _ = json_cli(
        &env.home,
        &env.install_root,
        &["server", "status", "--output", "json"],
    )
    .await;
    let previous_pid = read_ready_pid(&env.home).expect("a running gateway");

    // BUILD_CONFLICT refuses without touching the service.
    let op = submit_and_wait(
        &env.home,
        &env.install_root,
        &[
            "installation",
            "restart",
            "--output",
            "json",
            "--request-key",
            "e2e-restart-conflict",
            "--param-target",
            "ownedService",
            "--expected-build-id",
            "0.0.0+000000000000.000000000000",
        ],
        "installation.restart",
    );
    assert_eq!(op["state"], "failed");
    assert_eq!(op["result"]["error"]["code"], "BUILD_CONFLICT");
    assert_eq!(
        read_ready_pid(&env.home),
        Some(previous_pid),
        "a refused restart leaves the service running"
    );

    // The real handoff. The CLI prints the accepted receipt and normally
    // exits before the old gateway terminates itself; a connection drop
    // after the receipt is also honest (007 §6.2) — the operation id is
    // recovered from the listing in that case.
    let out = cli(
        &env.home,
        &env.install_root,
        &[
            "installation",
            "restart",
            "--output",
            "json",
            "--request-key",
            "e2e-restart-1",
            "--param-target",
            "ownedService",
            "--expected-build-id",
            &build_id,
            "--drain-timeout-ms",
            "30000",
        ],
    );
    let operation_id = if out.code == 0 {
        let receipt = expect_ok("installation.restart", &out);
        receipt["operationId"].as_str().unwrap().to_owned()
    } else {
        find_latest_operation(&env, "installation.restart")
    };

    // The successor completes the operation; tolerate the gateway gap.
    let deadline = Instant::now() + Duration::from_secs(60);
    let op = loop {
        let op = wait_operation_terminal(&env.home, &env.install_root, &operation_id);
        if op["state"] == "succeeded" {
            break op;
        }
        assert!(
            Instant::now() < deadline,
            "restart operation did not succeed: {op}"
        );
        std::thread::sleep(Duration::from_millis(500));
    };
    assert_eq!(op["result"]["healthy"], true);
    assert_eq!(op["result"]["buildId"], build_id.as_str());
    assert_ne!(
        op["result"]["newPid"].as_u64(),
        Some(u64::from(previous_pid)),
        "a new gateway process is serving"
    );

    // ready.json now describes the successor, and the successor serves the
    // same data root (a business command still works).
    let new_pid = read_ready_pid(&env.home).expect("the successor wrote ready.json");
    assert_ne!(new_pid, previous_pid);
    let status = json_cli(
        &env.home,
        &env.install_root,
        &["server", "status", "--output", "json"],
    )
    .await;
    assert_eq!(status["serviceState"], "ready");

    // Inspect still describes the managed installation.
    let result = json_cli(
        &env.home,
        &env.install_root,
        &["installation", "inspect", "--output", "json"],
    )
    .await;
    assert_eq!(result["buildId"], build_id.as_str());
}

fn read_ready_pid(home: &Path) -> Option<u32> {
    let raw = std::fs::read_to_string(home.join("run").join("ready.json")).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value
        .get("pid")
        .and_then(Value::as_u64)
        .map(|pid| pid as u32)
}

/// The newest operation of one type, for recovering an operation id when a
/// receipt could not be printed (the restart connection drop).
fn find_latest_operation(env: &Env, operation_type: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let out = cli(
            &env.home,
            &env.install_root,
            &[
                "operations",
                "list",
                "--output",
                "json",
                "--no-start",
                "--scope",
                "{\"kind\":\"instance\"}",
                "--operation-type",
                operation_type,
            ],
        );
        if out.code == 0
            && let Ok(value) = serde_json::from_str::<Value>(
                out.stdout
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or(""),
            )
            && let Some(items) = value["result"]["items"].as_array()
            && let Some(last) = items.last()
            && let Some(id) = last["operationId"].as_str()
        {
            return id.to_owned();
        }
        assert!(
            Instant::now() < deadline,
            "no `{operation_type}` operation found; last output:\n{}",
            out.stdout
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// `installation.diagnose`: the redacted report artifact with honest
/// statuses (source mode reports `unsupported`, never a green placeholder).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diagnose_publishes_the_redacted_report() {
    let env = Env::new("diagnose");
    init_home(&env.home, &env.install_root);

    let op = submit_and_wait(
        &env.home,
        &env.install_root,
        &[
            "installation",
            "diagnose",
            "--output",
            "json",
            "--request-key",
            "e2e-diagnose-1",
            "--checks",
            "installation,service,runtime,storage,channels,mcp",
        ],
        "installation.diagnose",
    );
    assert_eq!(op["state"], "succeeded");
    let result = &op["result"];
    assert_eq!(result["redacted"], true);
    assert!(
        result["reportArtifactId"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );

    let checks = result["checks"].as_array().unwrap().clone();
    let by_name = |name: &str| -> Value {
        checks
            .iter()
            .find(|check| check["name"] == json!(name))
            .unwrap_or_else(|| panic!("missing check `{name}`: {checks:?}"))
            .clone()
    };
    // Source mode: the managed-layout check is unsupported, not pass.
    assert_eq!(by_name("installation.manifest")["status"], "unsupported");
    assert_eq!(by_name("service.status")["status"], "pass");
    assert_eq!(by_name("runtime.health")["status"], "pass");
    assert_eq!(by_name("storage.dataRoot")["status"], "pass");
    assert_eq!(by_name("storage.stateDb")["status"], "pass");

    // The report artifact is retrievable through the artifact surface.
    let artifact_id = result["reportArtifactId"].as_str().unwrap();
    let out = cli(
        &env.home,
        &env.install_root,
        &[
            "artifacts",
            "get",
            "--output",
            "json",
            "--artifact-id",
            artifact_id,
        ],
    );
    let artifact = expect_ok("artifacts.get", &out);
    assert_eq!(artifact["artifact"]["mediaType"], "application/json");
}
