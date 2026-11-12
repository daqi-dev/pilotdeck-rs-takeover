//! S1 fault/retry tests use the real transcript writer and its existing
//! StorageStimulus. Formal RPC/process-restart coverage lives in the fixture.
use super::*;
use crate::storage::{JsonlTranscriptStore, TranscriptStore, stimulus::StorageStimulus};
use std::time::Duration;

struct Harness {
    root: PathBuf,
    home: PathBuf,
    project: PathBuf,
    control: PathBuf,
    stimulus: Arc<StorageStimulus>,
}
impl Harness {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("pdrs-s1-restore-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let project = root.join("project");
        let control = root.join("control");
        for path in [&home, &project, &control] {
            std::fs::create_dir_all(path).expect("S1 own fixture directory");
        }
        let stimulus = Arc::new(StorageStimulus::control(control.clone()));
        Self {
            root,
            home,
            project,
            control,
            stimulus,
        }
    }
    fn store(&self) -> Arc<dyn TranscriptStore> {
        Arc::new(JsonlTranscriptStore::with_stimulus(Some(
            self.stimulus.clone(),
        )))
    }
    fn key(&self, suffix: &str) -> JsString {
        format!(
            "{}-{suffix}",
            self.root.file_name().unwrap().to_string_lossy()
        )
        .into()
    }
    fn target(&self, key: &JsString) -> PathBuf {
        crate::runtime::session::transcript_locks::session_transcript_path(
            &self.home,
            &self.project,
            key,
        )
    }
    fn arm(&self, kind: &str, id: &str) {
        std::fs::write(
            self.control.join("stimulus.json"),
            format!(
                r#"{{"writer":{{"{kind}":{{"id":"{id}","key":"accepted_input","boundMs":5000}}}}}}"#
            ),
        )
        .expect("S1 arm existing writer stimulus");
    }
    async fn block(&self) -> u64 {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(files) = std::fs::read_dir(self.control.join("witness")) {
                    for file in files.flatten() {
                        if file.path().extension().is_none_or(|ext| ext != "json") {
                            continue;
                        }
                        let bytes = std::fs::read_to_string(file.path()).expect("S1 witness bytes");
                        let value =
                            pilotdeck_protocol::parse_json(&bytes).expect("S1 witness JSON");
                        if is(field(&value, "phase"), "block") {
                            return field(&value, "seq").as_number().expect("S1 block sequence")
                                as u64;
                        }
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("S1 accepted_input block reached")
    }
    fn release(&self, seq: u64) {
        std::fs::write(
            self.control.join("release.json"),
            format!(r#"{{"seq":{seq}}}"#),
        )
        .expect("S1 release held write");
    }
    fn stages(&self, key: &JsString) -> Vec<PathBuf> {
        let parent = self.target(key).parent().unwrap().to_path_buf();
        std::fs::read_dir(parent)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".ledger-")
            })
            .collect()
    }
    async fn restore(&self, key: JsString) -> Result<JsValue, RequestError> {
        execute(self.home.clone(), self.project.clone(), key, self.store()).await
    }
    fn start(&self, key: JsString) -> tokio::task::JoinHandle<Result<JsValue, RequestError>> {
        tokio::spawn(execute(
            self.home.clone(),
            self.project.clone(),
            key,
            self.store(),
        ))
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn archive() -> JsValue {
    pilotdeck_protocol::parse_json(r#"{"schemaVersion":1,"format":"portable_text_messages","title":"Restored safely","messages":[{"role":"user","text":"question"},{"role":"assistant","text":"answer"}]}"#).unwrap()
}
fn expected_export() -> JsValue {
    // Fixed TS formal export, before and after restart, puts messages before
    // title even when the imported archive supplies title before messages.
    // Keep a separate ordered expectation instead of reusing the input.
    pilotdeck_protocol::parse_json(r#"{"schemaVersion":1,"format":"portable_text_messages","messages":[{"role":"user","text":"question"},{"role":"assistant","text":"answer"}],"title":"Restored safely"}"#).unwrap()
}
async fn execute(
    home: PathBuf,
    project: PathBuf,
    key: JsString,
    store: Arc<dyn TranscriptStore>,
) -> Result<JsValue, RequestError> {
    let restoring = claim_restore(&key)?;
    let value = archive();
    validate(&value)?;
    restore_validated(home, project, key, value, restoring, store).await
}
fn contains_units(value: &JsString, text: &str) -> bool {
    let needle: Vec<u16> = text.encode_utf16().collect();
    needle.is_empty()
        || value
            .units()
            .windows(needle.len())
            .any(|part| part == needle)
}
fn assert_complete(path: &Path) {
    let read = crate::storage::read_transcript(path).expect("S1 reopened transcript");
    assert!(
        read.diagnostics.is_empty(),
        "S1 valid chain: {:?}",
        read.diagnostics
    );
    assert_eq!(read.entries.len(), 2);
    assert!(is(field(&read.entries[0], "type"), "session_metadata"));
    assert!(is(field(&read.entries[1], "type"), "accepted_input"));
    assert_eq!(portable_archive(&read.entries).unwrap(), expected_export());
}

#[tokio::test]
async fn s1_middle_append_failure_cleans_stage_and_same_key_retry_succeeds() {
    let h = Harness::new();
    let key = h.key("fault");
    h.arm("fault", "middle-eio");
    let error = h
        .restore(key.clone())
        .await
        .expect_err("S1 EIO must reject restore");
    assert_eq!(error.code, "EIO");
    assert_eq!(
        error.message,
        JsString::from(std::io::Error::from_raw_os_error(5).to_string())
    );
    assert!(!h.target(&key).exists());
    assert!(h.stages(&key).is_empty());
    h.restore(key.clone())
        .await
        .expect("S1 consumed fault permits retry");
    assert_complete(&h.target(&key));
    assert!(h.stages(&key).is_empty());
}

#[tokio::test]
async fn s1_cancelled_request_keeps_reservation_until_owned_writer_cleans_up() {
    let h = Harness::new();
    let key = h.key("cancel");
    h.arm("barrier", "middle-cancel");
    let task = h.start(key.clone());
    let seq = h.block().await;
    assert!(!h.target(&key).exists());
    assert_eq!(h.stages(&key).len(), 1);
    task.abort();
    assert!(task.await.expect_err("S1 request cancelled").is_cancelled());
    assert!(matches!(claim_restore(&key), Err(error) if error.code == "SESSION_RESTORE_CONFLICT"));
    assert!(!h.target(&key).exists());
    assert!(
        crate::runtime::session::factory::try_writer_lease(&h.target(&key))
            .unwrap()
            .is_none(),
        "S1 cancelled caller cannot release the blocking writer lease"
    );
    h.release(seq);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match h.restore(key.clone()).await {
                Ok(_) => return,
                Err(error)
                    if error.code == "SESSION_RESTORE_CONFLICT"
                        && contains_units(&error.message, "in progress") =>
                {
                    tokio::task::yield_now().await
                }
                Err(error) => panic!("S1 cancel retry failed: {error:?}"),
            }
        }
    })
    .await
    .expect("S1 cancellation releases and retry completes");
    assert_complete(&h.target(&key));
    assert!(h.stages(&key).is_empty());
}

#[tokio::test]
async fn s1_sanitized_key_aliases_publish_once_under_one_file_fence() {
    let h = Harness::new();
    let first = h.key("alias/name");
    let second = h.key("alias-name");
    assert_eq!(h.target(&first), h.target(&second));
    h.arm("barrier", "alias-first");
    let one = h.start(first.clone());
    let seq = h.block().await;
    let two = h.start(second.clone());
    tokio::task::yield_now().await;
    assert!(!two.is_finished());
    h.release(seq);
    one.await.unwrap().expect("S1 first alias publishes");
    let error = two.await.unwrap().expect_err("S1 other alias conflicts");
    assert_eq!(error.code, "SESSION_RESTORE_CONFLICT");
    assert_complete(&h.target(&first));
    assert!(h.stages(&first).is_empty());
}

#[tokio::test]
async fn s1_existing_writer_lease_excludes_restore_until_released() {
    let h = Harness::new();
    let key = h.key("writer");
    let lease = crate::runtime::session::factory::try_writer_lease(&h.target(&key))
        .unwrap()
        .unwrap();
    let error = h
        .restore(key.clone())
        .await
        .expect_err("S1 existing writer excludes restore");
    assert_eq!(error.code, "SESSION_WRITER_BUSY");
    assert!(!h.target(&key).exists());
    drop(lease);
    h.restore(key.clone())
        .await
        .expect("S1 released writer permits retry");
    assert_complete(&h.target(&key));
}

#[tokio::test]
async fn s1_target_created_during_stage_is_never_overwritten() {
    let h = Harness::new();
    let key = h.key("race");
    h.arm("barrier", "external-creator");
    let task = h.start(key.clone());
    let seq = h.block().await;
    std::fs::write(h.target(&key), b"external-owner-bytes\n").expect("S1 competing creator");
    h.release(seq);
    let error = task
        .await
        .unwrap()
        .expect_err("S1 commit cannot replace competitor");
    assert_eq!(error.code, "SESSION_RESTORE_CONFLICT");
    assert_eq!(
        std::fs::read(h.target(&key)).unwrap(),
        b"external-owner-bytes\n"
    );
    assert!(h.stages(&key).is_empty());
}

#[tokio::test]
async fn s1_occupied_target_kinds_conflict_without_mutating_them() {
    let h = Harness::new();
    let key = h.key("directory");
    std::fs::create_dir_all(h.target(&key)).unwrap();
    assert_eq!(
        h.restore(key.clone()).await.unwrap_err().code,
        "SESSION_RESTORE_CONFLICT"
    );
    assert!(h.target(&key).is_dir());
    let key = h.key("file");
    std::fs::write(h.target(&key), b"original").unwrap();
    assert_eq!(
        h.restore(key.clone()).await.unwrap_err().code,
        "SESSION_RESTORE_CONFLICT"
    );
    assert_eq!(std::fs::read(h.target(&key)).unwrap(), b"original");
    #[cfg(unix)]
    for (suffix, destination) in [
        ("live-link", h.target(&key)),
        ("dangling-link", h.root.join("missing-target")),
    ] {
        let alias = h.key(suffix);
        std::os::unix::fs::symlink(&destination, h.target(&alias)).unwrap();
        assert_eq!(
            h.restore(alias.clone()).await.unwrap_err().code,
            "SESSION_RESTORE_CONFLICT"
        );
        assert_eq!(std::fs::read_link(h.target(&alias)).unwrap(), destination);
    }
}

#[tokio::test]
async fn s1_real_parent_path_error_preserves_enotdir_and_allows_retry() {
    let h = Harness::new();
    let key = h.key("not-directory");
    let target = h.target(&key);
    let parent = target.parent().unwrap();
    std::fs::create_dir_all(parent.parent().unwrap()).unwrap();
    std::fs::write(parent, b"not a directory").unwrap();
    let error = h.restore(key.clone()).await.expect_err("S1 actual ENOTDIR");
    assert_eq!(error.code, "ENOTDIR");
    assert!(contains_units(&error.message, "lstat"));
    assert!(contains_units(
        &error.message,
        target.to_str().expect("S1 own fixture path is UTF-8")
    ));
    assert_eq!(std::fs::read(parent).unwrap(), b"not a directory");
    std::fs::remove_file(parent).unwrap();
    h.restore(key.clone())
        .await
        .expect("S1 repair parent then retry");
    assert_complete(&target);
}

#[cfg(unix)]
#[tokio::test]
async fn s1_real_directory_permission_error_preserves_eacces() {
    use std::os::unix::fs::PermissionsExt;
    let h = Harness::new();
    let key = h.key("permission");
    let target = h.target(&key);
    let parent = target.parent().unwrap();
    std::fs::create_dir_all(parent).unwrap();
    struct RestoreMode(PathBuf, std::fs::Permissions);
    impl Drop for RestoreMode {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, self.1.clone());
        }
    }
    let mode = RestoreMode(
        parent.to_path_buf(),
        std::fs::metadata(parent).unwrap().permissions(),
    );
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o000)).unwrap();
    let probe = std::fs::write(parent.join("permission-probe"), b"probe");
    if !matches!(&probe,Err(error) if error.kind()==std::io::ErrorKind::PermissionDenied) {
        assert!(
            !crate::test_permission_faults::require_permission_faults(),
            "S1 required real permission fault unavailable on this privileged host"
        );
        eprintln!(
            "SKIP: S1 directory permission fault unavailable; set PDRS_REQUIRE_PERMISSION_FAULTS=1 to require it"
        );
        return;
    }
    let error = h.restore(key.clone()).await.expect_err("S1 actual EACCES");
    assert_eq!(error.code, "EACCES");
    assert!(contains_units(&error.message, "lstat"));
    assert!(contains_units(
        &error.message,
        target.to_str().expect("S1 own fixture path is UTF-8")
    ));
    drop(mode);
    assert!(!target.exists());
    h.restore(key.clone())
        .await
        .expect("S1 restored permission permits retry");
    assert_complete(&target);
}

/// A real publication failure: the temporary inode remains open, but its name
/// vanishes after the last append, so sync succeeds and link returns ENOENT.
struct UnlinkStageAfterInput(JsonlTranscriptStore);
impl TranscriptStore for UnlinkStageAfterInput {
    fn append(
        &self,
        key: &crate::storage::TranscriptKey,
        entry: &JsValue,
    ) -> crate::storage::StoreResult<()> {
        self.0.append(key, entry)?;
        if is(field(entry, "type"), "accepted_input") {
            std::fs::remove_file(&key.transcript_path)?;
        }
        Ok(())
    }
    fn read(
        &self,
        key: &crate::storage::TranscriptKey,
        max: Option<u64>,
    ) -> crate::storage::StoreResult<crate::storage::TranscriptRead> {
        self.0.read(key, max)
    }
}
#[tokio::test]
async fn s1_real_publication_failure_keeps_enoent_link_stage_and_no_final() {
    let h = Harness::new();
    let key = h.key("link-missing");
    let error = execute(
        h.home.clone(),
        h.project.clone(),
        key.clone(),
        Arc::new(UnlinkStageAfterInput(JsonlTranscriptStore::with_stimulus(
            None,
        ))),
    )
    .await
    .expect_err("S1 actual link source missing");
    assert_eq!(error.code, "ENOENT");
    assert!(contains_units(&error.message, ", link '"));
    assert!(contains_units(&error.message, " -> '"));
    assert!(!h.target(&key).exists());
    assert!(h.stages(&key).is_empty());
    h.restore(key.clone())
        .await
        .expect("S1 publication failure allows retry");
    assert_complete(&h.target(&key));
}

#[test]
fn s1_non_io_storage_error_is_not_mislabelled_with_an_errno() {
    let error = restore_ledger_error(
        crate::storage::LedgerError::Invalid("not an OS fault".into()),
        Some(("write", None)),
    );
    assert_eq!(error.code, "gateway_request_failed");
    assert!(error.message.equals_str("not an OS fault"));
}
