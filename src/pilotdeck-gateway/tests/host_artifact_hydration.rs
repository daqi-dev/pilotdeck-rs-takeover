//! Host-owned payload recovery: the bytes live in the store, caches are disposable.
use pilotdeck_gateway::{
    runtime::{
        context::tool_budget::{ToolResultBudget, ToolResultBudgetOptions},
        project::{ProjectRuntimeRegistry, ResourceBundle},
        session::{
            SessionContext, SessionFactory,
            factory::{
                FactoryOptions, FactoryStorage, PreparedResources, PreparedSession,
                ProjectSessionFactory, SessionStorage as FactorySessionStorage,
            },
        },
        turn::{
            host_factory::{Deployment, TurnHostFactory},
            runner::tail::TailConfig,
        },
    },
    storage::{
        AsyncTranscriptAdapter, LedgerError, SessionBlobStorage, SessionStorageInput, StoreResult,
        TranscriptBlobStore, TranscriptKey, TranscriptRead, TranscriptStore, recorder::Recorder,
    },
};
use pilotdeck_protocol::{JsValue, parse_json};
use serde_json::json;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

fn js(value: serde_json::Value) -> JsValue {
    parse_json(&value.to_string()).unwrap()
}
fn blocks(value: &JsValue) -> &[JsValue] {
    match value {
        JsValue::Array(blocks) => blocks,
        _ => panic!("expected content array"),
    }
}
fn text(value: &JsValue) -> String {
    value.as_string().unwrap().to_utf8().unwrap()
}
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("pdrs-host-artifacts-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn results(&self) -> PathBuf {
        self.0.join(".pilotdeck/tool-results/session")
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[derive(Default)]
struct Blobs {
    bytes: Mutex<HashMap<String, Vec<u8>>>,
    calls: Mutex<Vec<String>>,
    fail_write: AtomicBool,
    fail_read: AtomicBool,
    delete_failure: Mutex<Option<std::io::ErrorKind>>,
}
impl TranscriptBlobStore for Blobs {
    fn write(&self, key: &TranscriptKey, name: &str, bytes: &[u8]) -> StoreResult<()> {
        self.calls.lock().unwrap().push(format!(
            "write:{}:{name}",
            key.session_id.to_utf8().unwrap()
        ));
        if self.fail_write.load(Ordering::SeqCst) {
            return Err(LedgerError::Invalid("HOST_WRITE_FAILED".into()));
        }
        self.bytes
            .lock()
            .unwrap()
            .insert(name.into(), bytes.to_vec());
        Ok(())
    }
    fn read(&self, key: &TranscriptKey, name: &str) -> StoreResult<Option<Vec<u8>>> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("read:{}:{name}", key.session_id.to_utf8().unwrap()));
        if self.fail_read.load(Ordering::SeqCst) {
            return Err(LedgerError::Invalid("HOST_READ_FAILED".into()));
        }
        Ok(self.bytes.lock().unwrap().get(name).cloned())
    }
    fn delete(&self, _: &TranscriptKey, name: &str) -> StoreResult<()> {
        self.calls.lock().unwrap().push(format!("delete:{name}"));
        if let Some(kind) = *self.delete_failure.lock().unwrap() {
            return Err(LedgerError::Io(std::io::Error::new(
                kind,
                "HOST_DELETE_FAILED",
            )));
        }
        self.bytes.lock().unwrap().remove(name);
        Ok(())
    }
    fn delete_all(&self, _: &TranscriptKey) -> StoreResult<()> {
        self.bytes.lock().unwrap().clear();
        Ok(())
    }
}
fn scoped(root: &Scratch, blobs: Arc<Blobs>) -> SessionBlobStorage {
    SessionBlobStorage {
        store: blobs,
        key: TranscriptKey {
            project_root: root.0.clone(),
            pilot_home: root.0.join("home"),
            session_id: "owner".into(),
            transcript_path: root.0.join("semantic/transcript.jsonl"),
        },
    }
}
fn budget(root: &Scratch, blobs: Option<Arc<Blobs>>) -> ToolResultBudget {
    let mut options = ToolResultBudgetOptions::new(root.results());
    options.max_result_size_chars = Some(48);
    options.max_result_size_tokens = Some(20);
    ToolResultBudget::new(options)
        .unwrap()
        .with_artifact_storage(blobs.map(|store| scoped(root, store)))
}
fn tool_message(payload: &str) -> JsValue {
    js(
        json!({"role":"user","content":[{"type":"tool_result","toolCallId":"read-output","content":[{"type":"text","text":payload}]}]}),
    )
}
fn reference(path: &Path, alias: Option<&str>, kind: &str) -> serde_json::Value {
    let mut value = json!({"type":kind,"path":path,"toolCallId":"read-output","mimeType":"text/plain","originalBytes":10,"preview":"preview","hasMore":true,"mediaType":"image"});
    if let Some(alias) = alias {
        value["readFilePath"] = json!(alias);
    }
    value
}

#[tokio::test]
async fn host_text_payload_and_alias_survive_deleted_cache_without_mutating_reference() {
    let root = Scratch::new();
    let blobs = Arc::new(Blobs::default());
    let writer = budget(&root, Some(blobs.clone()));
    let payload = "durable body ".repeat(80);
    let message = writer
        .apply_to_message(&tool_message(&payload), None)
        .await
        .unwrap();
    let blocks = blocks(message.get("content").unwrap());
    let block = blocks
        .iter()
        .find(|block| block.get("type") == Some(&JsValue::from("tool_result_reference")))
        .unwrap();
    let path = PathBuf::from(text(block.get("path").unwrap()));
    let alias = root.0.join(text(block.get("readFilePath").unwrap()));
    assert_eq!(fs::read_to_string(&path).unwrap(), payload);
    assert_eq!(fs::read_to_string(&alias).unwrap(), payload);
    fs::remove_dir_all(root.0.join(".pilotdeck/tool-results")).unwrap();
    let before = message.clone();
    budget(&root, Some(blobs.clone()))
        .hydrate_references(std::slice::from_ref(&message))
        .await
        .unwrap();
    assert_eq!(message, before);
    assert_eq!(fs::read_to_string(path).unwrap(), payload);
    assert_eq!(fs::read_to_string(alias).unwrap(), payload);
    assert_eq!(
        blobs
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.starts_with("write:owner:"))
            .count(),
        1
    );
}

#[tokio::test]
async fn host_write_failure_publishes_neither_payload_alias_nor_replacement_cache() {
    let root = Scratch::new();
    let blobs = Arc::new(Blobs::default());
    blobs.fail_write.store(true, Ordering::SeqCst);
    let writer = budget(&root, Some(blobs.clone()));
    let message = tool_message(&"payload ".repeat(80));
    assert_eq!(
        writer
            .apply_to_message(&message, None)
            .await
            .unwrap_err()
            .message,
        "HOST_WRITE_FAILED"
    );
    assert!(!root.results().exists());
    assert!(!root.0.join(".pilotdeck/tool-results/refs").exists());
    blobs.fail_write.store(false, Ordering::SeqCst);
    let recovered = writer.apply_to_message(&message, None).await.unwrap();
    assert!(
        blocks(recovered.get("content").unwrap())
            .iter()
            .any(|block| block.get("type") == Some(&JsValue::from("tool_result_reference")))
    );
    assert_eq!(blobs.bytes.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn host_media_payload_hydrates_as_encoded_body_and_has_no_read_file_alias() {
    let root = Scratch::new();
    let blobs = Arc::new(Blobs::default());
    let encoded = "YWJj".repeat(100);
    let message = js(
        json!({"role":"user","content":[{"type":"tool_result","toolCallId":"media","content":[{"type":"image","mimeType":"image/png","data":encoded}]}]}),
    );
    let persisted = budget(&root, Some(blobs.clone()))
        .apply_to_message(&message, None)
        .await
        .unwrap();
    let blocks = blocks(persisted.get("content").unwrap());
    let media = blocks
        .iter()
        .find(|block| block.get("type") == Some(&JsValue::from("media_reference")))
        .unwrap();
    let path = PathBuf::from(text(media.get("path").unwrap()));
    assert_eq!(fs::read_to_string(&path).unwrap(), encoded);
    fs::remove_file(&path).unwrap();
    budget(&root, Some(blobs))
        .hydrate_references(&[persisted])
        .await
        .unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), encoded);
}

#[tokio::test]
async fn hydration_decodes_utf8_and_empty_bytes_preserves_existing_cache_and_deduplicates_before_read()
 {
    let root = Scratch::new();
    let blobs = Arc::new(Blobs::default());
    let results = root.results();
    fs::create_dir_all(&results).unwrap();
    fs::write(results.join("existing.txt"), "local-original").unwrap();
    {
        let mut bytes = blobs.bytes.lock().unwrap();
        bytes.insert("utf8.txt".into(), vec![0xef, 0xbb, 0xbf, b'a', 0xff]);
        bytes.insert("empty.txt".into(), vec![]);
        bytes.insert("existing.txt".into(), b"host-replacement".to_vec());
        bytes.insert("media.txt".into(), b"media".to_vec());
    }
    let mut blocks = vec![
        reference(
            &results.join("utf8.txt"),
            Some(".pilotdeck/tool-results/refs/u.txt"),
            "tool_result_reference",
        ),
        reference(&results.join("empty.txt"), None, "media_reference"),
        reference(&results.join("existing.txt"), None, "tool_result_reference"),
        reference(&results.join("missing.txt"), None, "media_reference"),
        reference(
            &results.join("missing.txt"),
            Some(".pilotdeck/tool-results/refs/missing.txt"),
            "tool_result_reference",
        ),
        reference(&results.join("media.txt"), None, "media_reference"),
        reference(
            &results.join("media.txt"),
            Some(".pilotdeck/tool-results/refs/later.txt"),
            "tool_result_reference",
        ),
    ];
    // These paths are outside the direct-child managed set, including literal '..foo'.
    blocks.extend([
        reference(&results, None, "media_reference"),
        reference(&results.join("nested/x.txt"), None, "media_reference"),
        reference(&results.join("..foo"), None, "media_reference"),
        reference(&root.0.join("outside.txt"), None, "media_reference"),
    ]);
    budget(&root, Some(blobs.clone()))
        .hydrate_references(&[js(json!({"role":"user","content":blocks}))])
        .await
        .unwrap();
    assert_eq!(fs::read_to_string(results.join("utf8.txt")).unwrap(), "a�");
    assert_eq!(
        fs::read_to_string(root.0.join(".pilotdeck/tool-results/refs/u.txt")).unwrap(),
        "a�"
    );
    assert_eq!(fs::metadata(results.join("empty.txt")).unwrap().len(), 0);
    assert_eq!(
        fs::read_to_string(results.join("existing.txt")).unwrap(),
        "local-original"
    );
    assert!(
        !root
            .0
            .join(".pilotdeck/tool-results/refs/later.txt")
            .exists()
    );
    assert!(!results.join("missing.txt").exists());
    let calls = blobs.calls.lock().unwrap();
    assert_eq!(calls.len(), 5);
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.ends_with(":missing.txt"))
            .count(),
        1
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.ends_with(":media.txt"))
            .count(),
        1
    );
}

#[tokio::test]
async fn hydration_rejects_unmanaged_aliases_and_propagates_host_read_failure() {
    let root = Scratch::new();
    let blobs = Arc::new(Blobs::default());
    blobs
        .bytes
        .lock()
        .unwrap()
        .insert("body.txt".into(), b"body".to_vec());
    let path = root.results().join("body.txt");
    let messages = [js(
        json!({"role":"user","content":[reference(&path,Some("outside.txt"),"tool_result_reference")]}),
    )];
    budget(&root, Some(blobs.clone()))
        .hydrate_references(&messages)
        .await
        .unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "body");
    assert!(!root.0.join("outside.txt").exists());
    fs::remove_file(&path).unwrap();
    blobs.fail_read.store(true, Ordering::SeqCst);
    assert_eq!(
        budget(&root, Some(blobs.clone()))
            .hydrate_references(&messages)
            .await
            .unwrap_err()
            .message,
        "HOST_READ_FAILED"
    );
    assert!(!path.exists());
    let reads = blobs.calls.lock().unwrap().len();
    budget(&root, None)
        .hydrate_references(&messages)
        .await
        .unwrap();
    assert_eq!(blobs.calls.lock().unwrap().len(), reads);
}

struct Events {
    entries: Mutex<Vec<JsValue>>,
    blobs: Arc<Blobs>,
    order: Arc<Mutex<Vec<String>>>,
}
impl TranscriptStore for Events {
    fn append(&self, _: &TranscriptKey, entry: &JsValue) -> StoreResult<()> {
        self.entries.lock().unwrap().push(entry.clone());
        Ok(())
    }
    fn read(&self, _: &TranscriptKey, _: Option<u64>) -> StoreResult<TranscriptRead> {
        self.order.lock().unwrap().push("read".into());
        Ok(TranscriptRead {
            entries: self.entries.lock().unwrap().clone(),
            diagnostics: vec![],
        })
    }
    fn tool_result_artifacts(&self) -> Option<Arc<dyn TranscriptBlobStore>> {
        Some(self.blobs.clone())
    }
    fn recover_replacements(&self, _: &TranscriptKey) -> Option<StoreResult<()>> {
        self.order.lock().unwrap().push("recover".into());
        Some(Ok(()))
    }
}

#[tokio::test]
async fn factory_hydrates_effective_compaction_snapshot_before_host_publication_without_transcript_files()
 {
    let root = Scratch::new();
    let blobs = Arc::new(Blobs::default());
    let order = Arc::new(Mutex::new(vec![]));
    let events = Arc::new(Events {
        entries: Mutex::new(vec![]),
        blobs: blobs.clone(),
        order: order.clone(),
    });
    let layout = root.0.join("host-identifiers");
    let adapter = AsyncTranscriptAdapter::new(events.clone(), Arc::new(move |_| layout.clone()));
    let storage = adapter
        .create_session_storage(SessionStorageInput {
            project_root: root.0.clone(),
            pilot_home: root.0.join("home"),
            session_id: "session".into(),
            now: None,
        })
        .unwrap();
    let obsolete = root.results().join("obsolete.txt");
    let effective = root.results().join("effective.txt");
    blobs.bytes.lock().unwrap().extend([
        ("obsolete.txt".into(), b"obsolete".to_vec()),
        ("effective.txt".into(), b"effective".to_vec()),
    ]);
    storage
        .transcript
        .record_accepted_input(
            &"session".into(),
            &"seed".into(),
            vec![js(
                json!({"role":"user","content":[reference(&obsolete,None,"media_reference")]}),
            )],
            None,
        )
        .unwrap();
    let recorder = Recorder::from_writer(Arc::new(storage.transcript));
    recorder.record_compaction_replacement(&"session".into(),&"seed".into(),&js(json!({"kind":"compact","subtype":"compact_boundary","compactMetadata":{"compactionId":"restore-check","trigger":"auto","preTokens":10,"postTokens":2,"messagesSummarized":1,"summaryGenerated":true}})),&[js(json!({"role":"user","content":[reference(&effective,Some(".pilotdeck/tool-results/refs/effective.txt"),"tool_result_reference")]}))]).unwrap();
    let storage = adapter
        .create_session_storage(SessionStorageInput {
            project_root: root.0.clone(),
            pilot_home: root.0.join("home"),
            session_id: "session".into(),
            now: None,
        })
        .unwrap();
    let transcript_path = storage.transcript_path.clone();
    let checkpoint = storage.projection_checkpoint_path.clone();
    let factory_storage = Arc::new(Mutex::new(Some(FactoryStorage::from_session_storage(
        &storage,
    ))));
    let registry = Arc::new(ProjectRuntimeRegistry::new(
        &root.0,
        Arc::new(|_: &Path, _: &mut ResourceBundle| Ok(())),
    ));
    let prepare_registry = registry.clone();
    let published = Arc::new(AtomicBool::new(false));
    let prepare_published = published.clone();
    let alias = root.0.join(".pilotdeck/tool-results/refs/effective.txt");
    let host_order = order.clone();
    let result_dir = root.results();
    let factory = ProjectSessionFactory::new(FactoryOptions {
        writer_busy_wait_timeout: Duration::from_secs(1),
        prepare: Arc::new(move |_, _| {
            let registry = prepare_registry.clone();
            let published = prepare_published.clone();
            let effective = effective.clone();
            let obsolete = obsolete.clone();
            let alias = alias.clone();
            let order = host_order.clone();
            Box::pin(async move {
                Ok(PreparedSession {
                    resources: PreparedResources::default(),
                    tail: TailConfig::default(),
                    host: Box::new(move |binding| {
                        assert_eq!(fs::read_to_string(&effective).unwrap(), "effective");
                        assert_eq!(fs::read_to_string(&alias).unwrap(), "effective");
                        assert!(!obsolete.exists());
                        assert_eq!(binding.projection.snapshot()?.messages.len(), 1);
                        order.lock().unwrap().push("publish".into());
                        published.store(true, Ordering::SeqCst);
                        Ok(TurnHostFactory::new(
                            registry,
                            Deployment::Native,
                            pilotdeck_gateway::host::Limits::default(),
                            None,
                            |_, _| panic!("no model turn is required to restore a session"),
                        ))
                    }),
                })
            })
        }),
        create_storage: Arc::new(move |_| {
            let storage = factory_storage.lock().unwrap().take().unwrap();
            Box::pin(async move { Ok(FactorySessionStorage::Unlocked(storage)) })
        }),
        admission: None,
        diagnostic: Arc::new(|_| {}),
        tool_results_dir: Some(Arc::new(move |_| result_dir.clone())),
    });
    let handle = factory
        .create(
            SessionContext {
                session_key: "session".into(),
                project_key: None,
                channel_key: "test".into(),
                allowed_tools: None,
                disallowed_tools: None,
            },
            None,
        )
        .await
        .unwrap();
    assert!(published.load(Ordering::SeqCst));
    assert_eq!(*order.lock().unwrap(), vec!["read", "publish"]);
    assert_eq!(
        *blobs.calls.lock().unwrap(),
        vec!["read:session:effective.txt"]
    );
    assert!(!transcript_path.exists());
    assert!(!checkpoint.exists());
    handle.dispose("test-complete".into()).await.unwrap();
    factory.dispose().await.unwrap();
    registry.dispose().await.unwrap();
    assert!(!checkpoint.exists());
}

#[test]
fn host_file_history_rewinds_from_binary_blob_without_local_backup_and_preserves_mode() {
    use pilotdeck_gateway::storage::file_history::{FileHistoryOptions, FileHistoryStore};
    let root = Scratch::new();
    let blobs = Arc::new(Blobs::default());
    let file = root.0.join("edited.bin");
    let original = vec![0, 255, b'a', b'\n'];
    fs::write(&file, &original).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
    }
    let backup_dir = root.0.join("not-a-local-backup");
    let history = FileHistoryStore::new(FileHistoryOptions::new(&backup_dir))
        .with_backup_storage(Some(scoped(&root, blobs.clone())));
    history
        .track_edit(file.to_str().unwrap(), "before-edit")
        .unwrap();
    assert!(!backup_dir.exists());
    assert_eq!(
        blobs.bytes.lock().unwrap().values().next().unwrap(),
        &original
    );
    fs::write(&file, b"changed").unwrap();
    history
        .mark_edit_committed(file.to_str().unwrap(), "before-edit")
        .unwrap();
    let rewind = history.rewind("before-edit").unwrap();
    assert_eq!(rewind.files_changed, vec![file.to_str().unwrap()]);
    assert!(rewind.missing.is_empty());
    assert_eq!(fs::read(&file).unwrap(), original);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
    assert!(!backup_dir.exists());
}

#[test]
fn host_file_history_missing_payload_is_reported_and_store_failure_propagates_without_fs_fallback()
{
    use pilotdeck_gateway::storage::file_history::{FileHistoryOptions, FileHistoryStore};
    let root = Scratch::new();
    let blobs = Arc::new(Blobs::default());
    let file = root.0.join("edited.txt");
    fs::write(&file, "before").unwrap();
    let backup_dir = root.0.join("backups");
    let history = FileHistoryStore::new(FileHistoryOptions::new(&backup_dir))
        .with_backup_storage(Some(scoped(&root, blobs.clone())));
    history.track_edit(file.to_str().unwrap(), "edit").unwrap();
    fs::write(&file, "after").unwrap();
    history
        .mark_edit_committed(file.to_str().unwrap(), "edit")
        .unwrap();
    blobs.bytes.lock().unwrap().clear();
    let rewind = history.rewind("edit").unwrap();
    assert_eq!(rewind.missing, vec![file.to_str().unwrap()]);
    assert_eq!(fs::read_to_string(&file).unwrap(), "after");
    blobs.fail_read.store(true, Ordering::SeqCst);
    assert_eq!(
        history.rewind("edit").unwrap_err().to_string(),
        "HOST_READ_FAILED"
    );
    assert!(!backup_dir.exists());
    blobs.fail_write.store(true, Ordering::SeqCst);
    assert_eq!(
        history
            .track_edit(file.to_str().unwrap(), "retry")
            .unwrap_err()
            .to_string(),
        "HOST_WRITE_FAILED"
    );
    assert!(!backup_dir.exists());
    // The baseline's eviction catch ignores ENOENT from either storage leg,
    // while a permission failure remains observable through the warning port.
    for (kind, expected_warnings) in [
        (std::io::ErrorKind::NotFound, 0),
        (std::io::ErrorKind::PermissionDenied, 1),
    ] {
        let eviction_root = Scratch::new();
        let eviction_blobs = Arc::new(Blobs::default());
        let warnings = Arc::new(Mutex::new(Vec::<String>::new()));
        let warning_sink = warnings.clone();
        let mut options = FileHistoryOptions::new(eviction_root.0.join("absent-backups"));
        options.max_snapshots = Some(1);
        options.warn = Some(Arc::new(move |message| {
            warning_sink.lock().unwrap().push(message.to_owned())
        }));
        let history = FileHistoryStore::new(options)
            .with_backup_storage(Some(scoped(&eviction_root, eviction_blobs.clone())));
        let tracked = eviction_root.0.join("tracked.txt");
        fs::write(&tracked, "retained host backup").unwrap();
        history
            .track_edit(tracked.to_str().unwrap(), "one")
            .unwrap();
        history.make_snapshot("one").unwrap();
        fs::remove_file(&tracked).unwrap();
        *eviction_blobs.delete_failure.lock().unwrap() = Some(kind);
        history.make_snapshot("two").unwrap();
        assert_eq!(
            eviction_blobs
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.starts_with("delete:"))
                .count(),
            1
        );
        assert_eq!(warnings.lock().unwrap().len(), expected_warnings);
        if expected_warnings > 0 {
            assert!(warnings.lock().unwrap()[0].contains("HOST_DELETE_FAILED"));
        }
    }
}

struct GatedWrite {
    blobs: Arc<Blobs>,
    entered: tokio::sync::Semaphore,
    release: (Mutex<bool>, std::sync::Condvar),
    writes: std::sync::atomic::AtomicUsize,
}
struct ReleaseWriteOnDrop(Arc<GatedWrite>);
impl Drop for ReleaseWriteOnDrop {
    fn drop(&mut self) {
        let (lock, ready) = &self.0.release;
        *lock.lock().unwrap_or_else(|error| error.into_inner()) = true;
        ready.notify_all();
    }
}
impl TranscriptBlobStore for GatedWrite {
    fn write(&self, key: &TranscriptKey, name: &str, bytes: &[u8]) -> StoreResult<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.entered.add_permits(1);
        let (lock, ready) = &self.release;
        let mut released = lock.lock().unwrap();
        while !*released {
            released = ready.wait(released).unwrap();
        }
        drop(released);
        self.blobs.write(key, name, bytes)
    }
    fn read(&self, key: &TranscriptKey, name: &str) -> StoreResult<Option<Vec<u8>>> {
        self.blobs.read(key, name)
    }
    fn delete(&self, key: &TranscriptKey, name: &str) -> StoreResult<()> {
        self.blobs.delete(key, name)
    }
    fn delete_all(&self, key: &TranscriptKey) -> StoreResult<()> {
        self.blobs.delete_all(key)
    }
}

#[tokio::test]
async fn abandoned_host_write_keeps_owned_spill_ticket_and_retry_publishes_once_after_release() {
    let root = Scratch::new();
    let blobs = Arc::new(Blobs::default());
    let gate = Arc::new(GatedWrite {
        blobs: blobs.clone(),
        entered: tokio::sync::Semaphore::new(0),
        release: (Mutex::new(false), std::sync::Condvar::new()),
        writes: std::sync::atomic::AtomicUsize::new(0),
    });
    let _release_on_drop = ReleaseWriteOnDrop(gate.clone());
    let mut port = scoped(&root, blobs);
    port.store = gate.clone();
    let mut options = ToolResultBudgetOptions::new(root.results());
    options.max_result_size_chars = Some(48);
    let budget = Arc::new(
        ToolResultBudget::new(options)
            .unwrap()
            .with_artifact_storage(Some(port)),
    );
    let message = tool_message(&"owned-host-body ".repeat(90));
    let first_budget = budget.clone();
    let first_message = message.clone();
    let first =
        tokio::spawn(async move { first_budget.apply_to_message(&first_message, None).await });
    tokio::time::timeout(Duration::from_secs(3), gate.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    assert!(!root.results().exists());
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    let retry_budget = budget.clone();
    let retry = tokio::spawn(async move { retry_budget.apply_to_message(&message, None).await });
    tokio::task::yield_now().await;
    assert!(!retry.is_finished());
    assert_eq!(gate.writes.load(Ordering::SeqCst), 1);
    assert!(!root.results().exists());
    {
        let (lock, ready) = &gate.release;
        *lock.lock().unwrap() = true;
        ready.notify_all();
    }
    let result = tokio::time::timeout(Duration::from_secs(3), retry)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let reference = blocks(result.get("content").unwrap())
        .iter()
        .find(|block| block.get("type") == Some(&JsValue::from("tool_result_reference")))
        .unwrap();
    let file = PathBuf::from(text(reference.get("path").unwrap()));
    let alias = root.0.join(text(reference.get("readFilePath").unwrap()));
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "owned-host-body ".repeat(90)
    );
    assert_eq!(fs::read(&file).unwrap(), fs::read(alias).unwrap());
    assert_eq!(gate.writes.load(Ordering::SeqCst), 1);
    assert_eq!(gate.blobs.bytes.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn factory_hydration_read_failure_releases_prepared_resources_without_publication_and_can_retry()
 {
    let root = Scratch::new();
    let blobs = Arc::new(Blobs::default());
    let events = Arc::new(Events {
        entries: Mutex::new(vec![]),
        blobs: blobs.clone(),
        order: Arc::default(),
    });
    let layout = root.0.join("external-only");
    let adapter = Arc::new(AsyncTranscriptAdapter::new(
        events,
        Arc::new(move |_| layout.clone()),
    ));
    let input = SessionStorageInput {
        project_root: root.0.clone(),
        pilot_home: root.0.join("home"),
        session_id: "session".into(),
        now: None,
    };
    let storage = adapter.create_session_storage(input).unwrap();
    let path = root.results().join("restore.txt");
    blobs
        .bytes
        .lock()
        .unwrap()
        .insert("restore.txt".into(), b"recovered".to_vec());
    storage
        .transcript
        .record_accepted_input(
            &"session".into(),
            &"seed".into(),
            vec![js(
                json!({"role":"user","content":[reference(&path,None,"tool_result_reference")]}),
            )],
            None,
        )
        .unwrap();
    let registry = Arc::new(ProjectRuntimeRegistry::new(
        &root.0,
        Arc::new(|_: &Path, _: &mut ResourceBundle| Ok(())),
    ));
    let published = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let released = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let prepare_registry = registry.clone();
    let prepare_published = published.clone();
    let prepare_released = released.clone();
    let storage_root = root.0.clone();
    let result_dir = root.results();
    let factory = ProjectSessionFactory::new(FactoryOptions {
        writer_busy_wait_timeout: Duration::from_secs(1),
        prepare: Arc::new(move |_, _| {
            let registry = prepare_registry.clone();
            let published = prepare_published.clone();
            let released = prepare_released.clone();
            let path = path.clone();
            Box::pin(async move {
                let mut resources = PreparedResources::default();
                resources.push(move || {
                    Box::pin(async move {
                        released.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    })
                });
                Ok(PreparedSession {
                    resources,
                    tail: TailConfig::default(),
                    host: Box::new(move |_| {
                        assert_eq!(fs::read_to_string(&path).unwrap(), "recovered");
                        published.fetch_add(1, Ordering::SeqCst);
                        Ok(TurnHostFactory::new(
                            registry,
                            Deployment::Native,
                            pilotdeck_gateway::host::Limits::default(),
                            None,
                            |_, _| panic!("restoration must not call the provider"),
                        ))
                    }),
                })
            })
        }),
        create_storage: Arc::new(move |_| {
            let storage = adapter
                .create_session_storage(SessionStorageInput {
                    project_root: storage_root.clone(),
                    pilot_home: storage_root.join("home"),
                    session_id: "session".into(),
                    now: None,
                })
                .unwrap();
            Box::pin(async move {
                Ok(FactorySessionStorage::Unlocked(
                    FactoryStorage::from_session_storage(&storage),
                ))
            })
        }),
        admission: None,
        diagnostic: Arc::new(|_| {}),
        tool_results_dir: Some(Arc::new(move |_| result_dir.clone())),
    });
    let context = SessionContext {
        session_key: "session".into(),
        project_key: None,
        channel_key: "test".into(),
        allowed_tools: None,
        disallowed_tools: None,
    };
    blobs.fail_read.store(true, Ordering::SeqCst);
    let error = factory
        .create(context.clone(), None)
        .await
        .err()
        .expect("host hydration must fail creation");
    assert_eq!(error.message, "HOST_READ_FAILED");
    assert_eq!(published.load(Ordering::SeqCst), 0);
    assert_eq!(released.load(Ordering::SeqCst), 1);
    assert!(!root.results().exists());
    blobs.fail_read.store(false, Ordering::SeqCst);
    let handle = factory.create(context, None).await.unwrap();
    assert_eq!(published.load(Ordering::SeqCst), 1);
    handle.dispose("test-complete".into()).await.unwrap();
    assert_eq!(released.load(Ordering::SeqCst), 2);
    factory.dispose().await.unwrap();
    registry.dispose().await.unwrap();
}
