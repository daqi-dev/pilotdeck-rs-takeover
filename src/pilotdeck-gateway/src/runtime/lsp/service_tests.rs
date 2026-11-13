use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
struct Provider {
    id: String,
    extensions: BTreeMap<String, String>,
    disposed: Arc<AtomicUsize>,
    bound: Arc<AtomicUsize>,
    pending: bool,
}
impl LspProvider for Provider {
    fn id(&self) -> &str {
        &self.id
    }
    fn extension_to_language(&self) -> BTreeMap<String, String> {
        self.extensions.clone()
    }
    fn query(&self, _request: LspProviderQuery, cancel: Cancellation) -> LspFuture<LspQueryResult> {
        let pending = self.pending;
        Box::pin(async move {
            if pending {
                cancel.cancelled().await;
                return Err(LspError::new("ABORT_ERR", "LSP query aborted"));
            }
            Ok(LspQueryResult::Hover {
                hover: Some(serde_json::json!({"contents":"bound"})),
            })
        })
    }
    fn bind_session(
        &self,
        _execution: LspSessionExecution,
    ) -> Result<Arc<dyn LspProvider>, LspError> {
        self.bound.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(Self {
            id: self.id.clone(),
            extensions: self.extensions.clone(),
            disposed: self.disposed.clone(),
            bound: self.bound.clone(),
            pending: self.pending,
        }))
    }
    fn dispose(&self) -> LspFuture<()> {
        self.disposed.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}
fn provider() -> Arc<Provider> {
    Arc::new(Provider {
        id: "fixture".into(),
        extensions: BTreeMap::from([(".ts".into(), "typescript".into())]),
        disposed: Arc::default(),
        bound: Arc::default(),
        pending: false,
    })
}
fn request() -> LspQueryRequest {
    LspQueryRequest {
        operation: LspOperation::Hover,
        file_path: "source.ts".into(),
        position: LspPosition {
            line: 1.0,
            character: 2.0,
        },
        workspace_root: Path::new("/workspace").to_owned(),
    }
}
#[tokio::test]
async fn exact_provider_release_is_idempotent_and_cannot_remove_replacement() {
    let service = LspService::new();
    let old = provider();
    let release = service.register_provider(old.clone()).unwrap();
    release.release().await.unwrap();
    release.release().await.unwrap();
    assert_eq!(old.disposed.load(Ordering::SeqCst), 1);
    let replacement = provider();
    service.register_provider(replacement.clone()).unwrap();
    release.release().await.unwrap();
    assert!(service.query(request(), Cancellation::new()).await.is_ok());
    service.dispose().await.unwrap();
    service.dispose().await.unwrap();
    assert_eq!(replacement.disposed.load(Ordering::SeqCst), 1);
    assert_eq!(
        service
            .query(request(), Cancellation::new())
            .await
            .unwrap_err()
            .code,
        "LSP_DISPOSED"
    );
}
#[tokio::test]
async fn bound_session_dispose_does_not_stop_generation_or_another_session() {
    let service = LspService::new();
    let p = provider();
    service.register_provider(p.clone()).unwrap();
    let root = std::env::temp_dir().join(format!("pdrs-lsp-bound-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let execution = LspSessionExecution {
        workspace_root: root.clone(),
        io: Arc::new(NativeLspExecutionPort::new(Default::default(), root.clone()).unwrap()),
    };
    let a = service.bind_session(execution.clone()).await.unwrap();
    let b = service.bind_session(execution).await.unwrap();
    a.dispose().await.unwrap();
    assert!(b.query(request(), Cancellation::new()).await.is_ok());
    assert!(service.query(request(), Cancellation::new()).await.is_ok());
    assert_eq!(p.bound.load(Ordering::SeqCst), 2);
    b.dispose().await.unwrap();
    service.dispose().await.unwrap();
    assert_eq!(p.disposed.load(Ordering::SeqCst), 3);
    std::fs::remove_dir_all(root).unwrap();
}
#[tokio::test]
async fn query_cancellation_is_local_to_the_admitted_query() {
    let service = LspService::new();
    let base = provider();
    let p = Arc::new(Provider {
        pending: true,
        id: base.id.clone(),
        extensions: base.extensions.clone(),
        disposed: base.disposed.clone(),
        bound: base.bound.clone(),
    });
    service.register_provider(p.clone()).unwrap();
    let cancel = Cancellation::new();
    let query = service.query(request(), cancel.clone());
    cancel.cancel();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), query)
            .await
            .unwrap()
            .unwrap_err()
            .code,
        "ABORT_ERR"
    );
    assert_eq!(p.disposed.load(Ordering::SeqCst), 0);
    service.dispose().await.unwrap();
    assert_eq!(p.disposed.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn default_refusal_and_provider_conflicts_remain_explicit() {
    let service = LspService::new();
    assert_eq!(
        service
            .query(request(), Cancellation::new())
            .await
            .unwrap_err()
            .message,
        "No LSP provider is registered for .ts"
    );
    service.register_provider(provider()).unwrap();
    assert_eq!(
        service.register_provider(provider()).err().unwrap().code,
        "LSP_CONFLICT"
    );
    let mut outside = request();
    outside.file_path = "../foreign.ts".into();
    assert_eq!(
        service
            .query(outside, Cancellation::new())
            .await
            .unwrap_err()
            .code,
        "LSP_MALFORMED_RESPONSE"
    );
    service.dispose().await.unwrap();
}

struct DisposeGate {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    calls: Arc<AtomicUsize>,
}
impl LspProvider for DisposeGate {
    fn id(&self) -> &str {
        "gate"
    }
    fn extension_to_language(&self) -> BTreeMap<String, String> {
        BTreeMap::from([(".ts".into(), "typescript".into())])
    }
    fn query(
        &self,
        _request: LspProviderQuery,
        _cancel: Cancellation,
    ) -> LspFuture<LspQueryResult> {
        Box::pin(async { Ok(LspQueryResult::Hover { hover: None }) })
    }
    fn bind_session(
        &self,
        _execution: LspSessionExecution,
    ) -> Result<Arc<dyn LspProvider>, LspError> {
        Ok(Arc::new(Self {
            entered: self.entered.clone(),
            release: self.release.clone(),
            calls: self.calls.clone(),
        }))
    }
    fn dispose(&self) -> LspFuture<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let entered = self.entered.clone();
        let release = self.release.clone();
        Box::pin(async move {
            entered.notify_one();
            release.notified().await;
            Ok(())
        })
    }
}
#[tokio::test]
async fn dropped_disposal_receipt_retains_owner_and_next_dispose_joins() {
    let service = LspService::new();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    service
        .register_provider(Arc::new(DisposeGate {
            entered: entered.clone(),
            release: release.clone(),
            calls: calls.clone(),
        }))
        .unwrap();
    drop(service.dispose());
    tokio::time::timeout(std::time::Duration::from_secs(1), entered.notified())
        .await
        .unwrap();
    let mut done = service.dispose();
    tokio::select! {result=&mut done=>panic!("cleanup still owned and blocked {result:?}"),_=tokio::task::yield_now()=>{}}
    release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(1), done)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn unpolled_registration_release_starts_cleanup_without_service_dispose() {
    let service = LspService::new();
    let provider = provider();
    let registration = service.register_provider(provider.clone()).unwrap();
    drop(registration.release());
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while provider.disposed.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        service
            .query(request(), Cancellation::new())
            .await
            .unwrap_err()
            .code,
        "LSP_UNAVAILABLE"
    );
    service.dispose().await.unwrap();
    assert_eq!(provider.disposed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn repeated_release_joins_the_same_held_cleanup_after_first_receipt_drop() {
    let service = LspService::new();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let registration = service
        .register_provider(Arc::new(DisposeGate {
            entered: entered.clone(),
            release: release.clone(),
            calls: calls.clone(),
        }))
        .unwrap();
    drop(registration.release());
    tokio::time::timeout(std::time::Duration::from_secs(1), entered.notified())
        .await
        .unwrap();
    let mut repeated = registration.release();
    tokio::select! {biased; result=&mut repeated=>panic!("release escaped held provider cleanup: {result:?}"),_=tokio::task::yield_now()=>{}}
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(1), repeated)
        .await
        .unwrap()
        .unwrap();
    registration.release().await.unwrap();
    service.dispose().await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn callable_default_service_keeps_existing_fixed_oracle_refusals() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/native-process/oracle.json"
    ))
    .unwrap();
    let service = LspService::new();
    for (name, workspace, file, line) in [
        ("empty", "/isolated/project", "file.ts", 0.0),
        ("bound", "/isolated/project", "file.ts", 0.0),
        ("workspace", "", "file.ts", 0.0),
        ("position", "/isolated/project", "file.ts", -1.0),
        ("escape", "/isolated/project", "../file.ts", 0.0),
        ("dot", "/isolated/project", "dir.ts/.", 0.0),
        ("parent", "/isolated/project", "dir.ts/..", 0.0),
    ] {
        let actual = service
            .query(
                LspQueryRequest {
                    operation: LspOperation::Hover,
                    file_path: file.into(),
                    workspace_root: workspace.into(),
                    position: LspPosition {
                        line,
                        character: 0.0,
                    },
                },
                Cancellation::new(),
            )
            .await
            .unwrap_err();
        let expected = fixture["observations"]["lsp"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == name)
            .unwrap();
        assert_eq!(
            actual.code,
            expected["error"]["code"].as_str().unwrap(),
            "{name}"
        );
        assert_eq!(
            actual.message,
            expected["error"]["message"].as_str().unwrap(),
            "{name}"
        );
    }
    service.dispose().await.unwrap();
}

#[test]
fn concurrent_repeat_release_cannot_dispose_before_registry_removal() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let service = LspService::new();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let registration = Arc::new(
        service
            .register_provider(Arc::new(DisposeGate {
                entered: entered.clone(),
                release: release.clone(),
                calls: calls.clone(),
            }))
            .unwrap(),
    );
    let (arrived_tx, arrived_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let first = registration.clone();
        let handle = runtime.handle().clone();
        let owner = scope.spawn(move || {
            let _runtime = handle.enter();
            let receipt = first.release_with_barrier(|| {
                arrived_tx.send(()).unwrap();
                resume_rx
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap();
            });
            handle.block_on(async {
                tokio::time::timeout(std::time::Duration::from_secs(2), receipt)
                    .await
                    .unwrap()
                    .unwrap();
            });
        });
        arrived_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        runtime.block_on(async{
   let mut repeated=registration.release();
   tokio::time::timeout(std::time::Duration::from_secs(1),entered.notified()).await.unwrap();
   // A remains at the exact pre-registry-lock barrier. B has begun actual
   // provider disposal, and C must see the supplier as already unregistered.
   assert_eq!(service.query(request(),Cancellation::new()).await.unwrap_err().code,"LSP_UNAVAILABLE");
   resume_tx.send(()).unwrap();
   tokio::select!{biased; result=&mut repeated=>panic!("held provider cleanup returned early {result:?}"),_=tokio::task::yield_now()=>{}}
   release.notify_one();tokio::time::timeout(std::time::Duration::from_secs(1),repeated).await.unwrap().unwrap();
  });
        owner.join().unwrap();
    });
    runtime.block_on(async { service.dispose().await }).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn captured_owner_outlives_foreign_runtime_that_drops_cleanup_receipt() {
    let owner = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let service = owner.block_on(async {
        let service = LspService::new();
        service
            .register_provider(Arc::new(DisposeGate {
                entered: entered.clone(),
                release: release.clone(),
                calls: calls.clone(),
            }))
            .unwrap();
        service
    });
    let foreign = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    foreign.block_on(async {
        drop(service.dispose());
        tokio::time::timeout(std::time::Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
    });
    drop(foreign);
    release.notify_one();
    owner.block_on(async {
        tokio::time::timeout(std::time::Duration::from_secs(1), service.dispose())
            .await
            .unwrap()
            .unwrap();
    });
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn unstaged_service_has_explicit_error_then_attached_owner_supports_sync_release() {
    let service = LspService::new();
    let provider = provider();
    let registration = service.register_provider(provider.clone()).unwrap();
    let unavailable = registration.release();
    let owner = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let error = owner.block_on(unavailable).unwrap_err();
    assert_eq!(error.message, "LSP cleanup requires a Tokio runtime");
    assert_eq!(provider.disposed.load(Ordering::SeqCst), 0);
    service.attach_runtime(owner.handle().clone());
    drop(registration.release());
    owner.block_on(async {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while provider.disposed.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        service.dispose().await.unwrap();
    });
    assert_eq!(provider.disposed.load(Ordering::SeqCst), 1);
}

#[test]
fn registration_retains_its_executor_after_service_registry_is_dropped() {
    let owner = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let provider = provider();
    let registration = owner.block_on(async {
        let service = LspService::new();
        service.register_provider(provider.clone()).unwrap()
    });
    drop(registration.release());
    owner.block_on(async {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while provider.disposed.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    });
    assert_eq!(provider.disposed.load(Ordering::SeqCst), 1);
}
