//! An admitted child owns its generation independently of its parent. The
//! factory receives only weak authority, revoked at explicit child disposal.
use crate::runtime::{
    project::{DisposeFuture, RuntimeError, RuntimeLease, RuntimeResult},
    session::{CompactFuture, LiveSession, LiveTurn, factory::ReloadSnapshot},
    turn::host_factory::GenerationLeaseSupplier,
};
use pilotdeck_protocol::{JsString, JsValue};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub(super) struct Generation(Arc<Mutex<Option<RuntimeLease>>>);
impl Generation {
    pub(super) fn new(lease: RuntimeLease) -> Self {
        Self(Arc::new(Mutex::new(Some(lease))))
    }
    pub(super) fn supplier(&self) -> GenerationLeaseSupplier {
        let weak = Arc::downgrade(&self.0);
        Arc::new(move || {
            let owner = weak
                .upgrade()
                .ok_or_else(|| RuntimeError::new("Subagent generation is closed."))?;
            let lease = lock(&owner);
            lease
                .as_ref()
                .ok_or_else(|| RuntimeError::new("Subagent generation is closed."))?
                .fork()
        })
    }
    pub(super) fn mcp_provider(
        &self,
    ) -> Result<Arc<crate::runtime::mcp::registry::ProjectMcpRuntimeProvider>, JsString> {
        let owner = lock(&self.0);
        let lease = owner
            .as_ref()
            .ok_or_else(|| JsString::from("Subagent generation is closed."))?;
        let resources = lease
            .runtime()
            .resources()
            .get::<crate::runtime::project::ProjectResources>()
            .ok_or_else(|| JsString::from("Native project resources are missing."))?;
        let provider = resources
            .context()
            .owner(crate::runtime::project::Family::McpProvider)
            .ok_or_else(|| JsString::from("Native MCP provider is missing."))?;
        provider
            .as_any()
            .downcast_ref::<crate::runtime::project::tools_mcp::McpProviderResource>()
            .map(|resource| resource.provider().clone())
            .ok_or_else(|| JsString::from("Native MCP provider has the wrong type."))
    }
    pub(super) fn close(&self) -> DisposeFuture<'static> {
        let lease = lock(&self.0).take();
        Box::pin(async move {
            match lease {
                Some(mut lease) => lease.release().await,
                None => Ok(()),
            }
        })
    }
}

pub(super) struct ChildSession {
    inner: Arc<dyn LiveSession>,
    generation: Generation,
    extensions: Arc<Mutex<Option<Arc<crate::extensions::SessionExtensions>>>>,
    hooks: super::super::hooks::SessionHookCleanup,
    cleanup: Mutex<Option<tokio::sync::watch::Receiver<Option<RuntimeResult>>>>,
    mcp: Arc<Mutex<Option<super::scope::ForkMcp>>>,
    executor: tokio::runtime::Handle,
}
impl ChildSession {
    pub(super) fn new(
        inner: Arc<dyn LiveSession>,
        generation: Generation,
        extensions: Option<Arc<crate::extensions::SessionExtensions>>,
        hooks: super::super::hooks::SessionHookCleanup,
    ) -> Self {
        Self {
            inner,
            generation,
            extensions: Arc::new(Mutex::new(extensions)),
            hooks,
            cleanup: Mutex::default(),
            mcp: Arc::default(),
            executor: tokio::runtime::Handle::current(),
        }
    }
    pub(super) fn with_mcp(mut self, mcp: Option<super::scope::ForkMcp>) -> Self {
        self.mcp = Arc::new(Mutex::new(mcp));
        self
    }
    fn start_dispose(&self) -> tokio::sync::watch::Receiver<Option<RuntimeResult>> {
        let mut cleanup = lock(&self.cleanup);
        if let Some(completion) = cleanup.as_ref() {
            return completion.clone();
        }
        let (completed, completion) = tokio::sync::watch::channel(None);
        *cleanup = Some(completion.clone());
        // Revoke new child work synchronously, while the returned future keeps
        // the lease alive through writer and Hook cleanup even if unawaited.
        let release_generation = self.generation.close();
        let inner = self.inner.clone();
        let hooks = self.hooks.clone();
        let extensions = self.extensions.clone();
        let mcp = lock(&self.mcp).take();
        self.executor.spawn(async move {
            let session_result = inner.dispose().await;
            let hook_result = hooks.release().await;
            let mcp_result = match mcp {
                Some(mcp) => mcp
                    .stop()
                    .await
                    .map_err(|error| RuntimeError::new(String::from_utf16_lossy(error.units()))),
                None => Ok(()),
            };
            lock(&extensions).take();
            let generation_result = release_generation.await;
            completed.send_replace(Some(
                session_result
                    .and(hook_result)
                    .and(mcp_result)
                    .and(generation_result),
            ));
        });
        completion
    }
}
impl Drop for ChildSession {
    fn drop(&mut self) {
        self.start_dispose();
    }
}
impl LiveSession for ChildSession {
    fn reload_snapshot(&self) -> Result<ReloadSnapshot, RuntimeError> {
        self.inner.reload_snapshot()
    }
    fn draining_reload_snapshot(&self) -> Result<ReloadSnapshot, RuntimeError> {
        self.inner.draining_reload_snapshot()
    }
    fn submit(&self, input: JsValue, options: JsValue) -> Result<Box<dyn LiveTurn>, RuntimeError> {
        let admission = lock(&self.cleanup);
        if admission.is_some() {
            return Err(RuntimeError::new("Subagent session is disposed."));
        }
        self.inner.submit(input, options)
    }
    fn seed_read_state(&self, path: &JsString, mtime: f64) -> Result<bool, RuntimeError> {
        self.inner.seed_read_state(path, mtime)
    }
    fn compact(&self, turn: JsString, cancel: crate::host::Cancellation) -> CompactFuture {
        let admission = lock(&self.cleanup);
        if admission.is_some() {
            return Box::pin(async { Err(RuntimeError::new("Subagent session is disposed.")) });
        }
        self.inner.compact(turn, cancel)
    }
    fn snapshot(&self) -> JsValue {
        self.inner.snapshot()
    }
    fn abort(&self, reason: &JsString) -> RuntimeResult {
        self.inner.abort(reason)
    }
    fn disposal_guard(&self) -> Option<Box<dyn Send>> {
        self.inner.disposal_guard()
    }
    fn close_transcript(&self) {
        self.inner.close_transcript();
    }
    fn dispose(&self) -> DisposeFuture<'_> {
        let mut completion = self.start_dispose();
        Box::pin(async move {
            loop {
                if let Some(result) = completion.borrow_and_update().clone() {
                    return result;
                }
                completion.changed().await.map_err(|_| {
                    RuntimeError::new("Subagent cleanup stopped before completion.")
                })?;
            }
        })
    }
}

fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::project::{ProjectRuntimeRegistry, ResourceBundle};
    use std::path::Path;
    struct SlowSession {
        started: tokio::sync::Semaphore,
        finish: tokio::sync::Semaphore,
        disposals: std::sync::atomic::AtomicUsize,
    }
    impl LiveSession for SlowSession {
        fn snapshot(&self) -> JsValue {
            JsValue::Undefined
        }
        fn abort(&self, _: &JsString) -> RuntimeResult {
            Ok(())
        }
        fn dispose(&self) -> DisposeFuture<'_> {
            self.disposals
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.started.add_permits(1);
            Box::pin(async {
                self.finish.acquire().await.unwrap().forget();
                Ok(())
            })
        }
    }
    #[tokio::test]
    async fn cancelled_child_dispose_waiter_does_not_cancel_owned_cleanup() {
        let root =
            std::env::temp_dir().join(format!("pdrs-subagent-drain-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let registry =
            ProjectRuntimeRegistry::new(&root, Arc::new(|_: &Path, _: &mut ResourceBundle| Ok(())));
        let generation = Generation::new(registry.acquire(None).unwrap());
        let supplier = generation.supplier();
        let runtime = supplier().unwrap().runtime().clone();
        registry.invalidate(None);
        let inner = Arc::new(SlowSession {
            started: tokio::sync::Semaphore::new(0),
            finish: tokio::sync::Semaphore::new(0),
            disposals: Default::default(),
        });
        let session = ChildSession::new(inner.clone(), generation, None, Default::default());
        let waiter = session.dispose();
        assert!(
            supplier().is_err(),
            "disposal synchronously revokes child admission"
        );
        drop(waiter);
        inner.started.acquire().await.unwrap().forget();
        assert_eq!(
            runtime.lease_count(),
            1,
            "the cleanup task retains generation while draining"
        );
        assert!(
            session
                .submit(JsValue::Undefined, JsValue::Undefined)
                .is_err()
        );
        inner.finish.add_permits(1);
        tokio::time::timeout(std::time::Duration::from_secs(3), session.dispose())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(inner.disposals.load(std::sync::atomic::Ordering::SeqCst), 1);
        runtime.wait_disposed().await.unwrap();
        assert_eq!(runtime.lease_count(), 0);
        registry.dispose().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    struct Session;
    impl LiveSession for Session {
        fn snapshot(&self) -> JsValue {
            JsValue::Undefined
        }
        fn abort(&self, _: &JsString) -> RuntimeResult {
            Ok(())
        }
        fn dispose(&self) -> DisposeFuture<'_> {
            Box::pin(async { Ok(()) })
        }
    }
    #[tokio::test]
    async fn child_generation_survives_parent_close_and_releases_on_dispose_or_drop() {
        let root =
            std::env::temp_dir().join(format!("pdrs-subagent-generation-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let registry =
            ProjectRuntimeRegistry::new(&root, Arc::new(|_: &Path, _: &mut ResourceBundle| Ok(())));
        let parent = Generation::new(registry.acquire(None).unwrap());
        let parent_supplier = parent.supplier();
        let child = Generation::new(parent_supplier().unwrap());
        let child_supplier = child.supplier();
        let old = parent_supplier().unwrap().runtime().clone();
        registry.invalidate(None);
        parent.close().await.unwrap();
        assert!(parent_supplier().is_err());
        let turn = child_supplier().unwrap();
        assert!(Arc::ptr_eq(turn.runtime(), &old));
        drop(turn);
        let nested = Generation::new(child_supplier().unwrap());
        let session = ChildSession::new(Arc::new(Session), child, None, Default::default());
        session.dispose().await.unwrap();
        assert!(child_supplier().is_err());
        assert_eq!(
            old.lease_count(),
            1,
            "nested/background child owns independent admission"
        );
        drop(nested);
        old.wait_disposed().await.unwrap();
        assert_eq!(old.lease_count(), 0);
        registry.dispose().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
