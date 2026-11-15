//! Scope-owned Hyper task tracking. This draft is not yet wired into the client.
use std::{
    future::Future,
    sync::{Arc, Mutex},
};
use tokio::task::JoinHandle;

#[derive(Default)]
struct State {
    stopped: bool,
    tasks: Vec<JoinHandle<()>>,
}

/// Owns every task Hyper asks its client executor to spawn. `stop` requests
/// abort; `is_released` waits for Tokio to have actually dropped each task.
/// The socket dial bridge has its own tracking and must also converge.
#[derive(Clone, Default)]
pub struct ScopeExecutor {
    state: Arc<Mutex<State>>,
    untracked: bool,
}
impl ScopeExecutor {
    /// Original Gateway ambient client uses Tokio's untracked executor.
    pub fn ambient() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            untracked: true,
        }
    }
    pub fn stop(&self) {
        if self.untracked {
            return;
        }
        let aborts = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state.stopped = true;
            state.tasks.retain(|task| !task.is_finished());
            state
                .tasks
                .iter()
                .map(JoinHandle::abort_handle)
                .collect::<Vec<_>>()
        };
        for abort in aborts {
            abort.abort();
        }
    }
    pub fn is_released(&self) -> bool {
        if self.untracked {
            return false;
        }
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.tasks.retain(|task| !task.is_finished());
        state.stopped && state.tasks.is_empty()
    }
}
impl<F> hyper::rt::Executor<F> for ScopeExecutor
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    fn execute(&self, future: F) {
        if self.untracked {
            tokio::spawn(async move {
                let _ = future.await;
            });
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.tasks.retain(|task| !task.is_finished());
        if state.stopped {
            return;
        }
        state.tasks.push(tokio::spawn(async move {
            let _ = future.await;
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::rt::Executor;

    #[tokio::test]
    async fn shutdown_waits_for_the_aborted_task_to_finish() {
        let executor = ScopeExecutor::default();
        executor.execute(std::future::pending::<()>());
        assert!(!executor.is_released());
        executor.stop();
        for _ in 0..100 {
            if executor.is_released() {
                return;
            }
            tokio::task::yield_now().await;
        }
        assert!(executor.is_released());
    }
}
