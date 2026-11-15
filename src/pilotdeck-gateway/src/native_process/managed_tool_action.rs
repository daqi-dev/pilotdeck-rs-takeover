//! Control for the restricted native read_file action.  This is not a batch,
//! lease, background-process or remote-operation receipt.
use agentcpm_host::TrustedToolLifecycle;
use agentcpm_iface::{BoxFuture, types::Error};
use agentcpm_scheduler_protocol::tool_batch::BusinessResult;
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};

pub(crate) struct ActionControl {
    stopped: AtomicBool,
    released: AtomicBool,
}
impl ActionControl {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            stopped: AtomicBool::new(false),
            released: AtomicBool::new(false),
        })
    }
    pub(crate) fn wrap(
        self: &Arc<Self>,
        future: BoxFuture<'static, Result<BusinessResult, Error>>,
    ) -> BoxFuture<'static, Result<BusinessResult, Error>> {
        Box::pin(ActionFuture {
            inner: Some(future),
            control: self.clone(),
        })
    }
}
impl TrustedToolLifecycle for ActionControl {
    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }
    fn is_released(&self) -> bool {
        self.released.load(Ordering::Acquire)
    }
}
struct ActionFuture {
    inner: Option<BoxFuture<'static, Result<BusinessResult, Error>>>,
    control: Arc<ActionControl>,
}
impl ActionFuture {
    fn retire(&mut self) {
        // Drop the original future and its leases before making the receipt
        // visible. A panic in a destructor cannot publish a successful receipt.
        drop(self.inner.take());
        self.control.released.store(true, Ordering::Release);
    }
}
impl Future for ActionFuture {
    type Output = Result<BusinessResult, Error>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.control.stopped.load(Ordering::Acquire) {
            self.retire();
            return Poll::Ready(Err(Error::new(
                agentcpm_iface::types::ErrorCategory::Policy,
                -32000,
                "Managed tool action stopped.",
            )));
        }
        let Some(inner) = self.inner.as_mut() else {
            panic!("action polled after completion")
        };
        match inner.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(value) => {
                self.retire();
                Poll::Ready(value)
            }
        }
    }
}
impl Drop for ActionFuture {
    fn drop(&mut self) {
        if self.inner.is_some() {
            self.retire();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct DropMarker(Arc<AtomicBool>);
    impl Drop for DropMarker {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    #[tokio::test]
    async fn consumer_drop_releases_only_after_inner_destructor() {
        let control = ActionControl::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let marker = DropMarker(dropped.clone());
        let future = control.wrap(Box::pin(async move {
            let _marker = marker;
            std::future::pending::<Result<BusinessResult, Error>>().await
        }));
        assert!(!control.is_released());
        control.stop();
        assert!(!control.is_released());
        drop(future);
        assert!(dropped.load(Ordering::Acquire));
        assert!(control.is_released());
    }

    #[tokio::test]
    async fn stop_before_poll_does_not_poll_the_action() {
        let control = ActionControl::new();
        let polled = Arc::new(AtomicBool::new(false));
        let seen = polled.clone();
        let action = control.wrap(Box::pin(std::future::poll_fn(move |_| {
            seen.store(true, Ordering::Release);
            Poll::<Result<BusinessResult, Error>>::Pending
        })));
        control.stop();
        assert!(action.await.is_err());
        assert!(!polled.load(Ordering::Acquire));
        assert!(control.is_released());
    }
}
