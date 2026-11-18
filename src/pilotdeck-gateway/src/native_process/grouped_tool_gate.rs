//! Per-batch producer ownership around the future Host final-execution gate.
//! This module does not mint or validate a Kernel permit by itself.
use super::detached_tasks::{ProducerGroup, ProducerGuard};
use crate::{
    host::{HostError, HostFuture},
    runtime::tools::scheduler::{FinalToolAdmission, FinalToolContinuation, FinalToolRequest},
};
use pilotdeck_protocol::JsObject;
use std::sync::Arc;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::runtime::Handle;

/// Only the eventual trusted Host adapter may implement this. It must consume
/// the matching Host preparation and move the one-shot continuation into the
/// same real ToolJob after final-args/permission/permit validation. It must
/// retain all actual scoped resources until their independent receipts.
pub(crate) trait HostFinalExecutor: Send + Sync + 'static {
    fn execute(
        &self,
        request: FinalToolRequest,
        continuation: FinalToolContinuation,
    ) -> HostFuture<JsObject>;
}

struct GroupedGate<T: Send + Sync + 'static> {
    producer: ProducerGuard<T>,
    host: Arc<dyn HostFinalExecutor>,
    cancel: crate::host::Cancellation,
}
struct OwnedContinuation<T: Send + Sync + 'static> {
    inner: Option<FinalToolContinuation>,
    producer: Option<ProducerGuard<T>>,
}
struct OwnedActionFuture<T: Send + Sync + 'static> {
    inner: Option<HostFuture<JsObject>>,
    producer: Option<ProducerGuard<T>>,
}
fn retire_before_producer<T: Send + Sync + 'static, U>(
    inner: Option<U>,
    producer: &mut Option<ProducerGuard<T>>,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(inner)));
    if result.is_err()
        && let Some(producer) = producer.as_ref()
    {
        producer.mark_unknown();
    }
    drop(producer.take());
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
impl<T: Send + Sync + 'static> Drop for OwnedContinuation<T> {
    fn drop(&mut self) {
        retire_before_producer(self.inner.take(), &mut self.producer);
    }
}
impl<T: Send + Sync + 'static> OwnedContinuation<T> {
    fn invoke(
        mut self,
        permit: Arc<dyn crate::runtime::tools::scheduler::FinalToolPermit>,
    ) -> HostFuture<JsObject> {
        let continuation = self.inner.take().expect("single continuation");
        let producer = self.producer.take().expect("single producer");
        let future =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| continuation(permit)));
        match future {
            Ok(future) => Box::pin(OwnedActionFuture {
                inner: Some(future),
                producer: Some(producer),
            }),
            Err(panic) => {
                producer.mark_unknown();
                drop(producer);
                std::panic::resume_unwind(panic)
            }
        }
    }
}
impl<T: Send + Sync + 'static> Drop for OwnedActionFuture<T> {
    fn drop(&mut self) {
        retire_before_producer(self.inner.take(), &mut self.producer);
    }
}
impl<T: Send + Sync + 'static> Future for OwnedActionFuture<T> {
    type Output = Result<JsObject, HostError>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(inner) = self.inner.as_mut() else {
            panic!("action future polled after completion")
        };
        match inner.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(value) => {
                let inner = self.inner.take();
                retire_before_producer(inner, &mut self.producer);
                Poll::Ready(value)
            }
        }
    }
}
fn bind_action_producer<T: Send + Sync + 'static>(
    action_producer: ProducerGuard<T>,
    continuation: FinalToolContinuation,
) -> FinalToolContinuation {
    let owned = OwnedContinuation {
        inner: Some(continuation),
        producer: Some(action_producer),
    };
    Box::new(move |permit| owned.invoke(permit))
}
impl<T: Send + Sync + 'static> FinalToolAdmission for GroupedGate<T> {
    #[cfg(feature = "kernel-managed-model")]
    fn local_cancellation(&self) -> Option<crate::host::Cancellation> {
        Some(self.cancel.clone())
    }
    fn child_producer(&self) -> Result<Arc<dyn FinalToolAdmission>, HostError> {
        let producer = self.producer.child()?;
        Ok(Arc::new(Self {
            producer,
            host: self.host.clone(),
            cancel: self.cancel.clone(),
        }))
    }
    fn execute(
        &self,
        request: FinalToolRequest,
        continuation: FinalToolContinuation,
    ) -> HostFuture<JsObject> {
        let producer = match self.producer.child() {
            Ok(producer) => producer,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        // Host may retain the not-yet-invoked continuation even after the
        // submitting future is dropped. Give that continuation its own
        // producer until Host actually drops it or its action future exits.
        let action_producer = match self.producer.child() {
            Ok(producer) => producer,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let continuation = bind_action_producer(action_producer, continuation);
        let future = self.host.execute(request, continuation);
        Box::pin(async move {
            let _producer = producer;
            future.await
        })
    }
    fn spawn_detached(&self, future: HostFuture<JsObject>) -> Result<(), HostError> {
        self.producer.handoff(future)
    }
}

/// Host control retains the group, but must not retain the returned root gate
/// after the primary Handler exits: its last Arc owns the root producer.
/// The exact same Host preparation must also own `owner`, final executor,
/// project fork and reservation. No default executor or authority exists.
pub(crate) fn grouped_gate<T: Send + Sync + 'static>(
    owner: T,
    executor: Handle,
    host: Arc<dyn HostFinalExecutor>,
    cancel: crate::host::Cancellation,
) -> (ProducerGroup<T>, Arc<dyn FinalToolAdmission>) {
    let (group, producer) = ProducerGroup::new(owner, executor);
    let gate = Arc::new(GroupedGate {
        producer,
        host,
        cancel,
    });
    (group, gate)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Refuse;
    impl HostFinalExecutor for Refuse {
        fn execute(&self, _: FinalToolRequest, _: FinalToolContinuation) -> HostFuture<JsObject> {
            Box::pin(async {
                Err(HostError::new(
                    crate::host::HostErrorKind::Protocol,
                    "not installed",
                ))
            })
        }
    }
    #[tokio::test]
    async fn child_gate_keeps_the_group_open_after_primary_closes() {
        let (group, root) = grouped_gate(
            Arc::new(()),
            Handle::current(),
            Arc::new(Refuse),
            crate::host::Cancellation::default(),
        );
        let child = root.child_producer().unwrap();
        group.close_root();
        assert!(root.child_producer().is_err());
        drop(root);
        assert!(!group.tasks_drained());
        drop(child);
        assert!(group.tasks_drained());
        assert!(group.tasks_drained());
    }

    #[tokio::test]
    async fn host_retained_uninvoked_continuation_keeps_owner() {
        let (group, root) = ProducerGroup::new(Arc::new(()), Handle::current());
        let child = root.child().unwrap();
        let continuation =
            bind_action_producer(child, Box::new(|_| Box::pin(async { Ok(JsObject::new()) })));
        group.close_root();
        drop(root);
        assert!(!group.tasks_drained());
        drop(continuation);
        assert!(group.tasks_drained());
    }

    struct Permit;
    impl crate::runtime::tools::scheduler::FinalToolPermit for Permit {
        fn revalidate(&self) -> Result<(), HostError> {
            Ok(())
        }
    }
    struct BlockDrop {
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }
    impl Drop for BlockDrop {
        fn drop(&mut self) {
            let _ = self.entered.send(());
            let _ = self.release.recv();
        }
    }
    fn original_destructor_barrier(invoked: bool) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (group, root) = ProducerGroup::new(17_u32, runtime.handle().clone());
        let producer = root.child().unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let marker = BlockDrop {
            entered: entered_tx,
            release: release_rx,
        };
        let continuation: FinalToolContinuation = Box::new(move |_| {
            Box::pin(async move {
                let _marker = marker;
                std::future::pending::<Result<JsObject, HostError>>().await
            })
        });
        let wrapped = bind_action_producer(producer, continuation);
        group.close_root();
        drop(root);
        let worker = if invoked {
            let future = wrapped(Arc::new(Permit));
            std::thread::spawn(move || drop(future))
        } else {
            std::thread::spawn(move || drop(wrapped))
        };
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert!(group.take_drained_owner().is_none());
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(group.take_drained_owner(), Some(17));
    }
    #[test]
    fn uninvoked_continuation_destructs_before_owner_release() {
        original_destructor_barrier(false);
    }
    #[test]
    fn unpolled_action_future_destructs_before_owner_release() {
        original_destructor_barrier(true);
    }
}
