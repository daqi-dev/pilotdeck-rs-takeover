//! Detached lifecycle work belongs to the session even after its turn ends.
use super::*;
use crate::runtime::{hooks::Cancellation, project::RuntimeResult};
use std::collections::HashMap;

#[derive(Default)]
struct Active {
    closed: bool,
    next: u64,
    signals: HashMap<u64, Cancellation>,
}

#[derive(Clone)]
pub(super) struct SessionLifecycle {
    runtime: crate::runtime::hooks::LifecycleRuntime,
    active: Arc<Mutex<Active>>,
}
impl SessionLifecycle {
    pub(super) fn new(runtime: crate::runtime::hooks::HookRuntime) -> Self {
        Self {
            runtime: crate::runtime::hooks::LifecycleRuntime::new(runtime),
            active: Arc::default(),
        }
    }
    pub(super) fn close(&self) {
        let signals = {
            let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
            active.closed = true;
            active.signals.values().cloned().collect::<Vec<_>>()
        };
        for signal in signals {
            signal.abort();
        }
    }
}
impl LifecyclePort for SessionLifecycle {
    fn dispatch(&self, mut input: LifecycleDispatchInput) -> HostFuture<LifecycleDispatchResult> {
        let signal = input.signal.get_or_insert_with(Cancellation::new).clone();
        let guard = {
            let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
            if active.closed {
                return Box::pin(async {
                    Err(crate::host::ModuleError::new("Session hook runtime is closed.").into())
                });
            }
            let id = active.next;
            active.next += 1;
            active.signals.insert(id, signal);
            DispatchGuard {
                active: self.active.clone(),
                id,
            }
        };
        let runtime = self.runtime.clone();
        Box::pin(async move {
            let _guard = guard;
            runtime.dispatch(input).await
        })
    }
    fn dispose(&self) -> HostFuture<()> {
        self.close();
        let runtime = self.runtime.clone();
        Box::pin(async move { runtime.dispose_checked().await })
    }
}
struct DispatchGuard {
    active: Arc<Mutex<Active>>,
    id: u64,
}
impl Drop for DispatchGuard {
    fn drop(&mut self) {
        self.active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .signals
            .remove(&self.id);
    }
}

/// Registered before host construction, populated as soon as tools acquire
/// their session hook runtime. Failed construction and normal session close
/// consume the same prepared-resource release.
#[derive(Clone, Default)]
pub(in crate::native_process) struct SessionHookCleanup(Arc<Mutex<Option<Arc<ToolHooks>>>>);
impl SessionHookCleanup {
    pub(in crate::native_process) fn register(&self, hooks: Arc<ToolHooks>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(hooks);
    }
    pub(in crate::native_process) async fn release(self) -> RuntimeResult {
        let hooks = self.0.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(hooks) = hooks {
            hooks
                .close_session()
                .await
                .map_err(|error| crate::runtime::project::RuntimeError::new(error.to_string()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::hooks::exec::{ShellFuture, ShellPort, ShellRequest, ShellResult};
    use tokio::sync::mpsc;

    struct WaitingShell(mpsc::UnboundedSender<Cancellation>);
    impl ShellPort for WaitingShell {
        fn execute(&self, request: ShellRequest) -> ShellFuture {
            let signal = request.signal.expect("session hook cancellation");
            self.0.send(signal.clone()).unwrap();
            Box::pin(async move {
                signal.cancelled().await;
                Ok(ShellResult {
                    exit_code: Some(0),
                    stdout: vec![],
                    stderr: vec![],
                    timed_out: false,
                })
            })
        }
    }
    fn input(signal: Option<Cancellation>) -> LifecycleDispatchInput {
        LifecycleDispatchInput {
            event: "PreModelRequest".into(),
            base_input: JsObject::new(),
            payload: None,
            match_query: None,
            cwd: "/isolated".into(),
            env: None,
            signal,
        }
    }
    fn configured() -> (Arc<ToolHooks>, mpsc::UnboundedReceiver<Cancellation>) {
        let hooks = Arc::new(ToolHooks::default());
        let (started, receive) = mpsc::unbounded_channel();
        let config = pilotdeck_protocol::parse_json(
            r#"{"PreModelRequest":[{"hooks":[{"type":"command","command":"held"}]}]}"#,
        )
        .unwrap();
        hooks.configure_session(
            &crate::hooks::config::parse_hooks_config(&config).settings,
            &crate::runtime::hooks::runtime::HookObservers {
                shell: Some(Arc::new(WaitingShell(started))),
                ..Default::default()
            },
        );
        (hooks, receive)
    }
    #[tokio::test]
    async fn session_close_cancels_advisory_work_from_all_completed_turns_and_drains() {
        let (hooks, mut started) = configured();
        let cleanup = SessionHookCleanup::default();
        cleanup.register(hooks.clone());
        let port = hooks.session_port().unwrap();
        let old_turn = Cancellation::new();
        let new_turn = Cancellation::new();
        let first = tokio::spawn(port.dispatch(input(Some(old_turn.clone()))));
        let second = tokio::spawn(port.dispatch(input(Some(new_turn.clone()))));
        let detached = tokio::spawn(port.dispatch(input(None)));
        let mut admitted = vec![];
        for _ in 0..3 {
            admitted.push(started.recv().await.unwrap());
        }
        assert!(!old_turn.is_aborted());
        assert!(!new_turn.is_aborted());
        tokio::time::timeout(std::time::Duration::from_secs(2), cleanup.release())
            .await
            .unwrap()
            .unwrap();
        assert!(admitted.iter().all(Cancellation::is_aborted));
        assert!(old_turn.is_aborted());
        assert!(new_turn.is_aborted());
        for task in [first, second, detached] {
            task.await.unwrap().unwrap();
        }
        assert!(port.dispatch(input(None)).await.is_err());
        assert!(hooks.session_port().is_none());
    }

    #[tokio::test]
    async fn user_abort_keeps_session_authority_for_the_next_turn() {
        let (hooks, mut started) = configured();
        let port = hooks.session_port().unwrap();
        let user = Cancellation::new();
        let first = tokio::spawn(port.dispatch(input(Some(user.clone()))));
        assert!(started.recv().await.unwrap().same_channel(&user));
        user.abort();
        first.await.unwrap().unwrap();
        let next = tokio::spawn(port.dispatch(input(None)));
        let next_signal = started.recv().await.unwrap();
        assert!(!next_signal.is_aborted());
        hooks.close_session().await.unwrap();
        next.await.unwrap().unwrap();
    }
}
