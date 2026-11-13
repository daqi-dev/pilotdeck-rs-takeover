use super::*;
use crate::{
    host::{
        HostFuture,
        modules::context::{ContextRuntime, Operation},
    },
    runtime::project::{ProjectRuntimeRegistry, ResourceBundle},
    storage::{JsonlTranscriptStore, TranscriptKey},
};
use std::sync::atomic::{AtomicUsize, Ordering};

struct RequiredOnly(Arc<AtomicUsize>);
impl ContextRuntime for RequiredOnly {
    fn supports(&self, operation: Operation) -> bool {
        operation == Operation::PrepareForModel
    }
    fn prepare_for_model(&self, _: ContextInput) -> HostFuture<JsValue> {
        Box::pin(async { unreachable!("manual compaction must not prepare") })
    }
    fn try_auto_compact(&self, _: ContextInput) -> HostFuture<JsValue> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { unreachable!("an undeclared method must not run") })
    }
}

#[tokio::test]
async fn unsupported_external_compaction_records_skipped_without_calling_runtime() {
    let root = std::env::temp_dir().join(format!("pdrs-external-compact-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let registry = ProjectRuntimeRegistry::new(
        &root,
        Arc::new(|_: &std::path::Path, _: &mut ResourceBundle| Ok(())),
    );
    registry.resolve(None).unwrap();
    let lease = registry.acquire(None).unwrap();
    let path = root.join("session.jsonl");
    let recorder = Arc::new(Recorder::new(
        Arc::new(JsonlTranscriptStore::new()),
        TranscriptKey {
            project_root: root.clone(),
            pilot_home: root.clone(),
            session_id: "session".into(),
            transcript_path: path.clone(),
        },
    ));
    let calls = Arc::new(AtomicUsize::new(0));
    let context = MaintenanceContext::for_test(Arc::new(RequiredOnly(calls.clone())), lease);
    let cancel = Cancellation::default();
    cancel.cancel(None);
    let result = compact(
        recorder,
        "session".into(),
        "turn".into(),
        vec!["message".into()],
        context,
        cancel,
    )
    .await
    .unwrap();
    assert_eq!(result.get("type"), Some(&"skipped".into()));
    assert_eq!(result.get("reason"), Some(&"unavailable".into()));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let transcript = std::fs::read_to_string(&path).unwrap();
    assert!(transcript.contains("compaction_started"));
    assert!(transcript.contains("compaction_completed"));
    assert!(transcript.contains("\"status\":\"skipped\""));
    assert!(transcript.contains("turn_result"));
    std::fs::remove_dir_all(root).unwrap();
}
