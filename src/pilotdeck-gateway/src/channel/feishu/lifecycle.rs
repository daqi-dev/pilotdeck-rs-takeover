use super::super::protocol::TurnEventStream;
use super::*;
use futures_util::FutureExt;
use tokio::{sync::watch, task::JoinHandle};

pub(super) type CleanupReceipt = watch::Receiver<Option<Result<(), ChannelError>>>;
pub(super) type OwnedStream = Arc<tokio::sync::Mutex<Option<Box<dyn TurnEventStream>>>>;
pub(super) struct Lifecycle {
    pub open: bool,
    tasks: Vec<JoinHandle<()>>,
    pub streams: Vec<OwnedStream>,
    errors: Vec<ChannelError>,
    fatal: bool,
}
impl Default for Lifecycle {
    fn default() -> Self {
        Self {
            open: true,
            tasks: vec![],
            streams: vec![],
            errors: vec![],
            fatal: false,
        }
    }
}
impl Runtime {
    pub(super) fn spawn_task(
        self: &Arc<Self>,
        future: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> bool {
        if !self.owned.load(std::sync::atomic::Ordering::Acquire) {
            tokio::spawn(future);
            return true;
        }
        let mut lifecycle = self.lifecycle.lock().expect("feishu lifecycle");
        if !lifecycle.open {
            return false;
        }
        let mut i = 0;
        while i < lifecycle.tasks.len() {
            if !lifecycle.tasks[i].is_finished() {
                i += 1;
                continue;
            }
            let task = lifecycle.tasks.swap_remove(i);
            if let Some(Err(error)) = task.now_or_never() {
                lifecycle.fatal = true;
                lifecycle.errors.push(ChannelError::error(format!(
                    "feishu: owned task failed: {error}"
                )));
            }
        }
        let mut stop = self.stopped.subscribe();
        lifecycle.tasks.push(tokio::spawn(async move {
            tokio::select! {biased;_=stop.changed()=>{},_=future=>{}}
        }));
        true
    }
    pub(super) fn record_drain_panic(&self, detail: &str) {
        if self.owned.load(std::sync::atomic::Ordering::Acquire) {
            let mut lifecycle = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
            lifecycle.fatal = true;
            lifecycle.errors.push(ChannelError::error(format!(
                "feishu: owned drain task panicked: {detail}"
            )));
        }
    }
    pub(super) fn stop_owned(self: &Arc<Self>) -> BoxFuture<Result<(), ChannelError>> {
        {
            let mut lifecycle = self.lifecycle.lock().expect("feishu lifecycle");
            lifecycle.open = false;
            self.stopped.send_replace(true);
        }
        let mut cleanup = self.cleanup.lock().expect("feishu cleanup");
        let retry = cleanup
            .as_ref()
            .is_some_and(|r| matches!(r.borrow().as_ref(), Some(Err(_))));
        if cleanup.is_none() || retry {
            let (sender, receipt) = watch::channel(None);
            *cleanup = Some(receipt);
            let runtime = self.clone();
            struct Completion(
                watch::Sender<Option<Result<(), ChannelError>>>,
                Arc<Runtime>,
            );
            impl Drop for Completion {
                fn drop(&mut self) {
                    if self.0.borrow().is_none() {
                        self.1.lifecycle.lock().expect("feishu lifecycle").fatal = true;
                        self.0.send_replace(Some(Err(ChannelError::error(
                            "feishu: cleanup worker terminated before drain",
                        ))));
                    }
                }
            }
            let completion = Completion(sender, runtime.clone());
            tokio::spawn(async move {
                let stream_cleanup = runtime
                    .stream
                    .lock()
                    .expect("feishu stream owner")
                    .as_ref()
                    .map(|s| s.stop());
                if let Some(cleanup) = stream_cleanup
                    && let Err(error) = cleanup.await
                {
                    let mut l = runtime.lifecycle.lock().expect("feishu lifecycle");
                    l.errors.push(error);
                    l.fatal = true;
                }
                let tasks =
                    std::mem::take(&mut runtime.lifecycle.lock().expect("feishu lifecycle").tasks);
                for task in tasks {
                    if let Err(error) = task.await {
                        let mut l = runtime.lifecycle.lock().expect("feishu lifecycle");
                        l.fatal = true;
                        l.errors.push(ChannelError::error(format!(
                            "feishu: owned task failed: {error}"
                        )));
                    }
                }
                let streams = std::mem::take(
                    &mut runtime.lifecycle.lock().expect("feishu lifecycle").streams,
                );
                let mut failed = vec![];
                for stream in streams {
                    let mut guard = stream.lock().await;
                    if let Some(events) = guard.as_mut() {
                        if let Err(error) = events
                            .cancel_and_drain("feishu channel stopped".into())
                            .await
                        {
                            runtime
                                .lifecycle
                                .lock()
                                .expect("feishu lifecycle")
                                .errors
                                .push(error);
                            failed.push(stream.clone());
                        } else {
                            *guard = None;
                        }
                    }
                }
                let result = {
                    let mut l = runtime.lifecycle.lock().expect("feishu lifecycle");
                    l.streams = failed;
                    if l.errors.is_empty() && !l.fatal {
                        Ok(())
                    } else {
                        let errors = if l.fatal {
                            l.errors.clone()
                        } else {
                            std::mem::take(&mut l.errors)
                        };
                        Err(ChannelError::Aggregate {
                            message: "feishu: cleanup failed".into(),
                            errors,
                        })
                    }
                };
                completion.0.send_replace(Some(result));
            });
        }
        let mut receipt = cleanup.as_ref().expect("feishu cleanup receipt").clone();
        Box::pin(async move {
            loop {
                if let Some(result) = receipt.borrow().clone() {
                    return result;
                }
                receipt
                    .changed()
                    .await
                    .map_err(|_| ChannelError::error("feishu cleanup receipt lost"))?;
            }
        })
    }
}
