//! Real CLI setup entry: each answer follows its prompt, and durable output is
//! compared with two executions of the fixed TS baseline. No network is used.
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const CASES: &str = include_str!("fixtures/c1-entry/cases.json");
const ORACLE: &str = include_str!("fixtures/c1-entry/oracle.json");
const SOURCE: &str = include_str!("fixtures/c1-entry/source.json");

struct Scratch(PathBuf);

impl Scratch {
    fn new(id: &str) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("pdrs-c1-setup-{id}-{}-{stamp}", std::process::id()));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

fn check_provenance() {
    let source: Value = serde_json::from_str(SOURCE).unwrap();
    assert_eq!(
        source["sourceCommit"],
        "cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85"
    );
    assert_eq!(source["nodeVersion"], "v22.22.0");
    for (key, content) in [
        ("casesSha256", CASES),
        ("oracleSha256", ORACLE),
        (
            "captureSha256",
            include_str!("fixtures/c1-entry/capture.mjs"),
        ),
    ] {
        assert_eq!(
            source[key],
            format!("{:x}", Sha256::digest(content.as_bytes())),
            "C1 fixture provenance {key}"
        );
    }
}

fn tree(root: &Path) -> Value {
    fn visit(root: &Path, dir: &Path, found: &mut serde_json::Map<String, Value>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, found);
            } else {
                found.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                    Value::String(fs::read_to_string(path).unwrap()),
                );
            }
        }
    }
    let mut found = serde_json::Map::new();
    visit(root, root, &mut found);
    Value::Object(found)
}

#[test]
fn setup_success_and_abandoned_input_match_fixed_baseline() {
    check_provenance();
    let cases: Vec<Value> = serde_json::from_str(CASES).unwrap();
    let oracle: Vec<Value> = serde_json::from_str(ORACLE).unwrap();
    assert_eq!(cases.len(), 5, "C1 setup case denominator");
    assert_eq!(oracle.len(), cases.len(), "C1 complete oracle");
    for case in cases {
        let id = case["id"].as_str().unwrap();
        let expected = oracle.iter().find(|item| item["id"] == id).unwrap();
        let scratch = Scratch::new(id);
        let home = scratch.0.join("home");
        let pilot = home.join(".pilotdeck");
        let project = scratch.0.join("project");
        fs::create_dir_all(&pilot).unwrap();
        fs::create_dir(&project).unwrap();
        fs::write(
            pilot.join("pilotdeck.yaml"),
            case["config"].as_str().unwrap(),
        )
        .unwrap();
        if let Some(files) = case["files"].as_object() {
            for (name, value) in files {
                assert_eq!(Path::new(name).file_name().unwrap(), name.as_str());
                let content = value.as_str().unwrap_or_else(|| {
                    // Existing credentials are unchanged by setup. Preserve
                    // the captured seed's object order and whitespace too.
                    let raw = expected["tree"][name].as_str().unwrap();
                    assert_eq!(serde_json::from_str::<Value>(raw).unwrap(), *value);
                    raw
                });
                fs::write(pilot.join(name), content).unwrap();
            }
        }
        let mut child = OwnedChild(
            Command::new(env!("CARGO_BIN_EXE_pilotdeck"))
                .args(
                    case["argv"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap()),
                )
                .env_clear()
                .env("HOME", &home)
                .env("USERPROFILE", &home)
                .env("CFFIXED_USER_HOME", &home)
                .env("PILOT_HOME", &pilot)
                .env("PILOTDECK_CONFIG_PATH", pilot.join("pilotdeck.yaml"))
                .env("TMPDIR", &scratch.0)
                .env("LANG", "C")
                .env("LC_ALL", "C")
                .env("TZ", "UTC")
                .env("TERM", "dumb")
                .env("NO_COLOR", "1")
                .env("DOCKER_CONTAINER", "1")
                .env("COMMIT_HASH", "c1-fixture")
                .current_dir(project)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut stdin = child.0.stdin.take();
        let mut stdout = child.0.stdout.take().unwrap();
        let mut stderr = child.0.stderr.take().unwrap();
        let (send, receive) = mpsc::channel();
        let output = thread::spawn(move || {
            let mut full = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let count = stdout.read(&mut buffer).unwrap();
                if count == 0 {
                    break;
                }
                full.extend_from_slice(&buffer[..count]);
                let _ = send.send(buffer[..count].to_vec());
            }
            full
        });
        let errors = thread::spawn(move || {
            let mut full = Vec::new();
            stderr.read_to_end(&mut full).unwrap();
            full
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = Vec::new();
        let mut cursor = 0;
        for answer in case["answers"].as_array().unwrap() {
            let prompt = answer[0].as_str().unwrap();
            loop {
                let text = String::from_utf8_lossy(&seen);
                if let Some(position) = text[cursor..].find(prompt) {
                    cursor += position + prompt.len();
                    break;
                }
                let left = deadline.saturating_duration_since(Instant::now());
                let chunk = receive.recv_timeout(left).unwrap_or_else(|error| {
                    panic!("C1 {id}: missing prompt {prompt:?}: {error}; output={text}")
                });
                seen.extend_from_slice(&chunk);
            }
            if let Some(answer) = answer[1].as_str() {
                writeln!(stdin.as_mut().unwrap(), "{answer}").unwrap();
                stdin.as_mut().unwrap().flush().unwrap();
            } else {
                drop(stdin.take());
            }
        }
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "C1 {id}: CLI did not exit");
            thread::sleep(Duration::from_millis(10));
        };
        let stdout = String::from_utf8(output.join().unwrap()).unwrap();
        let stderr = String::from_utf8(errors.join().unwrap()).unwrap();
        assert_eq!(status.code(), Some(0), "C1 {id}: {stderr}");
        assert!(stderr.is_empty(), "C1 {id}: unexpected stderr {stderr:?}");
        assert_eq!(
            stdout,
            expected["stdout"].as_str().unwrap(),
            "C1 {id}: stdout"
        );
        assert_eq!(tree(&pilot), expected["tree"], "C1 {id}: durable files");
    }
}
