//! Scope-owned forwarding for original product model options.
use crate::host::modules::model_port::{RouterModelPortOptions, stream_options_for_invocation};
use crate::runtime::model::http::{StreamOptions, YieldGate};
use crate::runtime::model::router::AbortFlag;
use agentcpm_iface::{
    BoxFuture,
    env::http_pull::{HttpFailure, HttpPullContext, HttpResources},
};
use pilotdeck_protocol::JsObject;
use pilotdeck_trusted_http_host::{ScopedLifecycle, TrustedHttpRuntime};
use std::sync::{Arc, Mutex};
use tokio::{runtime::Handle, sync::Notify, task::JoinHandle};

#[derive(Default)]
struct State {
    stopped: bool,
    started: bool,
    joined: bool,
    failed: bool,
    abort: Option<tokio::task::AbortHandle>,
}
struct Forwarder {
    state: Mutex<State>,
    notify: Notify,
    task: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    #[cfg(test)]
    join_attempts: std::sync::atomic::AtomicUsize,
}
impl Forwarder {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
            notify: Notify::new(),
            task: tokio::sync::Mutex::new(None),
            #[cfg(test)]
            join_attempts: std::sync::atomic::AtomicUsize::new(0),
        })
    }
    fn start(&self, turn: AbortFlag, child: AbortFlag, executor: &Handle) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.stopped {
            state.started = true;
            state.joined = true;
            self.notify.notify_waiters();
            return;
        }
        if turn.is_aborted() {
            state.started = true;
            child.abort();
            state.joined = true;
            self.notify.notify_waiters();
            return;
        }
        let task = executor.spawn(async move {
            turn.cancelled().await;
            child.abort();
        });
        state.abort = Some(task.abort_handle());
        *self
            .task
            .try_lock()
            .expect("unstarted task cannot have a joiner") = Some(task);
        state.started = true;
        self.notify.notify_waiters();
    }
}
impl ScopedLifecycle for Forwarder {
    fn stop(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.stopped = true;
        if let Some(abort) = &state.abort {
            abort.abort();
        }
        if !state.started {
            state.joined = true;
        }
        self.notify.notify_waiters();
    }
    fn resources(&self) -> HttpResources {
        // Resident-wide shutdown may only poll is_released; consume a finished
        // task's actual JoinHandle there as well as in the async receipt path.
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if !state.joined
            && !state.failed
            && let Ok(mut task_slot) = self.task.try_lock()
            && let Some(task) = task_slot.as_mut()
            && task.is_finished()
        {
            use std::{
                future::Future,
                pin::Pin,
                task::{Context, Poll, Waker},
            };
            let mut cx = Context::from_waker(Waker::noop());
            if let Poll::Ready(result) = Pin::new(task).poll(&mut cx) {
                task_slot.take();
                state.abort = None;
                state.failed = result.as_ref().is_err_and(|error| !error.is_cancelled());
                state.joined = !state.failed;
                self.notify.notify_waiters();
            }
        }
        if state.failed {
            HttpResources::Unknown
        } else if state.joined {
            HttpResources::Released
        } else {
            HttpResources::Held
        }
    }
    fn released(&self) -> BoxFuture<'_, HttpResources> {
        Box::pin(async move {
            loop {
                let notified = self.notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let started = {
                    let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                    if state.failed {
                        return HttpResources::Unknown;
                    }
                    if state.joined {
                        return HttpResources::Released;
                    }
                    state.started
                };
                // Before start publishes its handle there can be no joiner.
                if !started {
                    notified.await;
                    continue;
                }
                #[cfg(test)]
                self.join_attempts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut task_slot = self.task.lock().await;
                if let Some(task) = task_slot.as_mut() {
                    // Await by reference: cancellation of this receipt reader
                    // leaves the handle in the scope for the next reader.
                    let result = task.await;
                    task_slot.take();
                    let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                    state.abort = None;
                    state.failed = result.as_ref().is_err_and(|error| !error.is_cancelled());
                    state.joined = !state.failed;
                    self.notify.notify_waiters();
                    continue;
                }
                drop(task_slot);
                notified.await;
            }
        })
    }
}
/// Called only with original call context from the current sealed input and the
/// same turn's already-verified facts. No returned data grants permissions.
pub(crate) fn create_job_options(
    context: &JsObject,
    turn_options: &RouterModelPortOptions,
    turn_abort: AbortFlag,
    scope_context: HttpPullContext,
    runtime: &TrustedHttpRuntime,
    executor: &Handle,
) -> Result<StreamOptions, HttpFailure> {
    if !turn_options.fallback_models.is_empty() || turn_options.managed_model_policy.is_some() {
        return Err(HttpFailure::new(
            agentcpm_iface::env::http_pull::HttpFailurePhase::Scope,
            agentcpm_iface::env::http_pull::HttpFailureKind::Unsupported,
            "managed model router options require the original router",
        ));
    }
    let child = AbortFlag::new();
    let options = stream_options_for_invocation(
        context,
        turn_options.invocation.as_ref(),
        turn_options.is_main_agent,
        child.clone(),
        YieldGate::new(),
    );
    let forwarder = Forwarder::new();
    // Register the unstarted Held owner before any task is spawned. If close
    // wins, registration/start cannot create an untracked late task.
    runtime.register_scope_lifecycle(scope_context, forwarder.clone())?;
    forwarder.start(turn_abort, child, executor);
    Ok(options)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_audit_fields_come_from_call_and_turn_options() {
        use crate::host::modules::model_port::InvocationPortAudit;
        use crate::storage::invocation_log::{InvocationStorageConfig, JsonlInvocationLogSink};
        use pilotdeck_protocol::JsValue;
        let mut context = JsObject::new();
        context.insert("sessionId".into(), "source-session".into());
        context.insert("turnId".into(), "source-turn".into());
        context.insert("runId".into(), "call-run".into());
        let mut options = RouterModelPortOptions::main_agent();
        options.is_main_agent = false;
        options.invocation = Some(InvocationPortAudit {
            sink: Arc::new(JsonlInvocationLogSink::new(InvocationStorageConfig {
                root: std::path::PathBuf::new(),
                storage_config_version: "v-original".into(),
            })),
            workspace_id: "workspace".into(),
        });
        let options = stream_options_for_invocation(
            &context,
            options.invocation.as_ref(),
            options.is_main_agent,
            AbortFlag::new(),
            YieldGate::new(),
        );
        let audit = &options.invocation.unwrap().context;
        for (key, value) in [
            ("sessionId", "source-session"),
            ("turnId", "source-turn"),
            ("runId", "call-run"),
            ("workspaceId", "workspace"),
            ("storageConfigVersion", "v-original"),
            ("caller", "subagent"),
        ] {
            assert_eq!(audit.get(key), Some(&JsValue::from(value)));
        }
        assert_eq!(audit.get("logicalCallId"), Some(&JsValue::Undefined));
    }
    #[tokio::test]
    async fn close_joins_task_and_never_cancels_another_job() {
        let turn = AbortFlag::new();
        let a = Forwarder::new();
        let b = Forwarder::new();
        let ca = AbortFlag::new();
        let cb = AbortFlag::new();
        a.start(turn.clone(), ca.clone(), &Handle::current());
        b.start(turn.clone(), cb.clone(), &Handle::current());
        a.stop();
        assert_eq!(a.released().await, HttpResources::Released);
        assert!(!turn.is_aborted() && !cb.is_aborted());
        turn.abort();
        tokio::time::timeout(std::time::Duration::from_secs(1), cb.cancelled())
            .await
            .unwrap();
        b.stop();
        assert_eq!(b.released().await, HttpResources::Released);
    }
    #[tokio::test]
    async fn global_shutdown_poll_observes_join_without_receipt_reader() {
        let forwarder = Forwarder::new();
        forwarder.start(AbortFlag::new(), AbortFlag::new(), &Handle::current());
        forwarder.stop();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while forwarder.resources() != HttpResources::Released {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn cancelled_receipt_reader_does_not_detach_the_scope_owned_join() {
        let forwarder = Forwarder::new();
        forwarder.start(AbortFlag::new(), AbortFlag::new(), &Handle::current());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), forwarder.released())
                .await
                .is_err()
        );
        assert_eq!(forwarder.resources(), HttpResources::Held);
        forwarder.stop();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), forwarder.released())
                .await
                .unwrap(),
            HttpResources::Released
        );
    }
    #[tokio::test]
    async fn receipt_waiting_before_start_never_takes_the_empty_join_slot() {
        use std::{future::Future, task::Poll};
        let forwarder = Forwarder::new();
        let mut receipt = forwarder.released();
        std::future::poll_fn(|cx| {
            assert!(receipt.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(
            forwarder
                .join_attempts
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "pre-start receipt must not attempt the join slot"
        );
        forwarder.start(AbortFlag::new(), AbortFlag::new(), &Handle::current());
        std::future::poll_fn(|cx| {
            assert!(receipt.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(
            forwarder
                .join_attempts
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "published task must be joined"
        );
        forwarder.stop();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), receipt)
                .await
                .unwrap(),
            HttpResources::Released
        );
    }
    #[tokio::test]
    async fn close_before_start_cannot_spawn_untracked_task() {
        let forwarder = Forwarder::new();
        forwarder.stop();
        assert_eq!(forwarder.released().await, HttpResources::Released);
        let child = AbortFlag::new();
        forwarder.start(AbortFlag::new(), child.clone(), &Handle::current());
        assert_eq!(forwarder.resources(), HttpResources::Released);
        assert!(!child.is_aborted());
    }
}
