//! Self-contained provenance checks for the unchanged provider fixtures.
//! The optional baseline verification remains read-only and does not regenerate expectations.
use pilotdeck_protocol::{JsValue, parse_json};
use sha2::{Digest, Sha256};
use std::{fs, path::Path, process::Command};
const BASELINE_COMMIT: &str = "cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85";
fn text(value: &JsValue) -> String {
    value
        .as_string()
        .expect("provenance string")
        .to_utf8()
        .expect("#988 provenance string must be valid UTF-8")
}

pub(crate) fn oracle(json: &str) -> JsValue {
    check_fixture_bytes(json);
    let oracle = parse_json(json).expect("oracle.json must be valid JSON");
    let baseline = std::env::var_os("PDRS_BASELINE");
    check_impl(&oracle, baseline.as_deref().map(Path::new))
        .unwrap_or_else(|message| panic!("{message}"));
    oracle
}
fn check_fixture_bytes(json: &str) {
    let manifest = parse_json(include_str!(
        "../../tests/fixtures/model-driver/manifest.json"
    ))
    .unwrap();
    assert_eq!(text(manifest.get("sourceCommit").unwrap()), BASELINE_COMMIT);
    let JsValue::Array(fixtures) = manifest.get("fixtures").unwrap() else {
        panic!("fixture manifest")
    };
    let digest = format!("{:x}", Sha256::digest(json.as_bytes()));
    assert!(
        fixtures
            .iter()
            .any(|fixture| text(fixture.get("sha256").unwrap()) == digest),
        "model driver fixture differs from the recorded original bytes"
    );
}
pub(crate) fn check_impl(oracle: &JsValue, baseline: Option<&Path>) -> Result<(), String> {
    // Older corpora use commit/baselineCommit and sources. Keep their bytes and
    // schema intact while applying the same checks to every replay.
    let commit = ["sourceCommit", "baselineCommit", "commit"]
        .into_iter()
        .find_map(|key| oracle.get(key))
        .ok_or_else(|| "#1109 oracle is missing its baseline commit".to_string())?;
    let recorded = text(commit);
    if recorded != BASELINE_COMMIT {
        return Err(format!(
            "#1109 oracle baseline commit mismatch: recorded {recorded} expected {BASELINE_COMMIT}"
        ));
    }
    let hashes = oracle
        .get("sourceSha256")
        .or_else(|| oracle.get("sources"))
        .ok_or_else(|| "#1109 oracle is missing sourceSha256/sources".to_string())?
        .as_object()
        .ok_or_else(|| "#1109 oracle source hashes must be an object".to_string())?;
    if hashes.is_empty() {
        return Err("#1109 oracle source hashes must not be empty".to_string());
    }
    if let Some(baseline) = baseline {
        let head = git(baseline, &["rev-parse", "HEAD"]);
        let head = String::from_utf8_lossy(&head).trim().to_string();
        if head != BASELINE_COMMIT {
            return Err(format!(
                "#1109 PDRS_BASELINE must be checked out at the fixed commit: HEAD is {head}"
            ));
        }
        let tracked = git(baseline, &["status", "--porcelain", "--untracked-files=no"]);
        if !tracked.is_empty() {
            return Err(format!(
                "#1109 PDRS_BASELINE has uncommitted changes to tracked files: {}",
                String::from_utf8_lossy(&tracked).replace('\n', "; ")
            ));
        }
    }
    for (source, recorded) in hashes.entries() {
        let source = source.to_utf8().expect("UTF-8 source path");
        if !(source.starts_with("src/") && source.split('/').all(|p| p != ".." && !p.is_empty())) {
            return Err(format!(
                "#1109 expected a baseline-relative source path: {source}"
            ));
        }
        let recorded = text(recorded);
        if recorded.len() != 64 || !recorded.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("#1109 missing or malformed hash for {source}"));
        }
        if let Some(baseline) = baseline {
            let committed = git(baseline, &["show", &format!("{BASELINE_COMMIT}:{source}")]);
            let expected = format!("{:x}", Sha256::digest(committed));
            if recorded != expected {
                return Err(format!(
                    "#1109 oracle hash for {source} does not match {BASELINE_COMMIT}"
                ));
            }
            let working = fs::read(baseline.join(&source))
                .map_err(|error| format!("#1109 baseline source {source}: {error}"))?;
            if format!("{:x}", Sha256::digest(working)) != expected {
                return Err(format!(
                    "#1109 baseline source {source} differs from {BASELINE_COMMIT}"
                ));
            }
        }
    }
    Ok(())
}

fn git(baseline: &Path, args: &[&str]) -> Vec<u8> {
    let mut command = Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    let output = command
        .args(["--no-replace-objects", "-C"])
        .arg(baseline)
        .args(args)
        .output()
        .expect("run git against PDRS_BASELINE");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
fn copied_fixtures_keep_original_bytes_and_reject_corruption() {
    for raw in [
        include_str!("../../tests/fixtures/model-driver/model-stream.json"),
        include_str!("../../tests/fixtures/model-driver/model-stream-retry-words.json"),
        include_str!("../../tests/fixtures/model-driver/model-stream-openai.json"),
        include_str!("../../tests/fixtures/model-driver/model-stream-google.json"),
        include_str!("../../tests/fixtures/model-driver/model-stream-responses.json"),
    ] {
        check_fixture_bytes(raw);
    }
    let changed = format!(
        "{} ",
        include_str!("../../tests/fixtures/model-driver/model-stream-responses.json")
    );
    assert!(std::panic::catch_unwind(|| check_fixture_bytes(&changed)).is_err());
}
#[test]
fn provenance_rejects_wrong_commit_and_empty_source_table() {
    let original = parse_json(include_str!(
        "../../tests/fixtures/model-driver/model-stream-openai.json"
    ))
    .unwrap();
    let mut changed = original.as_object().unwrap().clone();
    changed.insert("sourceCommit".into(), "wrong".into());
    assert!(check_impl(&JsValue::Object(changed), None).is_err());
    let mut changed = original.as_object().unwrap().clone();
    changed.insert("sourceSha256".into(), JsValue::Object(Default::default()));
    assert!(check_impl(&JsValue::Object(changed), None).is_err());
}
