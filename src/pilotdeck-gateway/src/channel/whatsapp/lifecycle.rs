//! Independent cleanup ownership, following the shared IM channel lifecycle.
use super::*;

#[derive(Clone)]
struct CleanupOutcome {
    result: Result<(), ChannelError>,
    retryable: bool,
}
struct CleanupCompletion {
    sender: watch::Sender<Option<CleanupOutcome>>,
    finished: bool,
}
impl CleanupCompletion {
    fn finish(mut self, result: Result<(), ChannelError>, retryable: bool) {
        self.sender
            .send_replace(Some(CleanupOutcome { result, retryable }));
        self.finished = true;
    }
}
impl Drop for CleanupCompletion {
    fn drop(&mut self) {
        if !self.finished {
            self.sender.send_replace(Some(CleanupOutcome {
                result: Err(ChannelError::error("whatsapp: cleanup worker terminated before draining; cleanup ownership is unresolved")),
                retryable: false,
            }));
        }
    }
}
pub(super) struct CleanupAttempt {
    result: watch::Receiver<Option<CleanupOutcome>>,
    // The worker owns cleanup independently of every stop waiter. Keeping its
    // handle here also makes a worker failure observable through the receipt.
    _worker: JoinHandle<()>,
}
impl Shared {
    async fn cleanup_owned(self: Arc<Self>, retry: bool) -> Result<(), ChannelError> {
        let retry_streams = if retry {
            std::mem::take(&mut *self.failed_streams.lock().expect("whatsapp failed streams"))
        } else {
            Vec::new()
        };
        let poller = self.poller.lock().expect("whatsapp poller").take();
        if let Some(poller) = poller {
            self.record_join(poller.await, "poller");
        }
        let polls = std::mem::take(&mut *self.polls.lock().expect("whatsapp polls"));
        for poll in polls {
            self.record_join(poll.await, "poll");
        }
        let bridge = self.bridge.lock().expect("whatsapp bridge").take();
        if let Some(bridge) = bridge {
            if let Err(error) = bridge.join().await {
                self.fatal_cleanup.store(true, Ordering::Release);
                self.cleanup_errors
                    .lock()
                    .expect("whatsapp cleanup errors")
                    .push(error);
            }
        }
        let handlers = std::mem::take(&mut *self.handlers.lock().expect("whatsapp handlers"));
        for handler in handlers {
            if let Err(error) = handler.await {
                self.fatal_cleanup.store(true, Ordering::Release);
                self.log(
                    "error",
                    &format!("whatsapp: handler cleanup failed: {error}"),
                );
                self.cleanup_errors
                    .lock()
                    .expect("whatsapp cleanup errors")
                    .push(ChannelError::error(format!(
                        "whatsapp: handler cleanup failed: {error}"
                    )));
            }
        }
        let outbound = std::mem::take(&mut self.outbound.lock().expect("whatsapp outbound").tasks);
        for task in outbound {
            if let Err(error) = task.await {
                self.fatal_cleanup.store(true, Ordering::Release);
                self.cleanup_errors
                    .lock()
                    .expect("whatsapp cleanup errors")
                    .push(ChannelError::error(format!(
                        "whatsapp: outbound cleanup failed: {error}"
                    )));
            }
        }
        for mut stream in retry_streams {
            if let Err(error) = stream
                .cancel_and_drain(JsString::from("whatsapp channel stopped"))
                .await
            {
                self.cleanup_errors
                    .lock()
                    .expect("whatsapp cleanup errors")
                    .push(error);
                self.failed_streams
                    .lock()
                    .expect("whatsapp failed streams")
                    .push(stream);
            }
        }
        let errors =
            std::mem::take(&mut *self.cleanup_errors.lock().expect("whatsapp cleanup errors"));
        if !errors.is_empty() {
            return Err(ChannelError::Aggregate {
                message: "whatsapp: owned turn cleanup failed".into(),
                errors,
            });
        }
        let chats: Vec<_> = self
            .state
            .lock()
            .expect("whatsapp state")
            .known_chats
            .iter()
            .cloned()
            .collect();
        for chat in chats {
            self.interaction.clear_chat_for_shutdown(&chat);
        }
        Ok(())
    }

    fn cleanup_receipt(self: &Arc<Self>) -> watch::Receiver<Option<CleanupOutcome>> {
        let mut attempt = self.cleanup.lock().expect("whatsapp cleanup attempt");
        let retry = attempt.is_some();
        if let Some(current) = attempt.as_ref() {
            // Pending work is always shared. A completed failure retains its
            // owned streams in Shared and a subsequent stop retries them.
            if !current
                .result
                .borrow()
                .as_ref()
                .is_some_and(|outcome| outcome.retryable)
            {
                return current.result.clone();
            }
        }
        let (completed, result) = watch::channel(None);
        let shared = self.clone();
        // Construct the guard before spawning: abort before the worker's first
        // poll must also publish a terminal error instead of an empty receipt.
        let completion = CleanupCompletion {
            sender: completed,
            finished: false,
        };
        let worker = tokio::spawn(async move {
            let outcome = shared.clone().cleanup_owned(retry).await;
            let can_retry = outcome.is_err()
                && !shared.fatal_cleanup.load(Ordering::Acquire)
                && !shared
                    .failed_streams
                    .lock()
                    .expect("whatsapp failed streams")
                    .is_empty();
            completion.finish(outcome, can_retry);
        });
        *attempt = Some(CleanupAttempt {
            result: result.clone(),
            _worker: worker,
        });
        result
    }
}
impl ChannelHandle for Handle {
    fn stop(&self, reason: &str) -> BoxFuture<Result<(), ChannelError>> {
        self.0.stop_receipt(Some(reason))
    }
}
impl Shared {
    pub(super) fn stop_receipt(
        self: &Arc<Self>,
        reason: Option<&str>,
    ) -> BoxFuture<Result<(), ChannelError>> {
        // This prefix is deliberately synchronous: merely obtaining and
        // dropping the returned future must already stop accepting messages.
        self.outbound.lock().expect("whatsapp outbound").accepting = false;
        self.running.store(false, Ordering::Release);
        self.stopping.send_replace(true);
        if let Some(bridge) = self.bridge.lock().expect("whatsapp bridge").as_ref() {
            bridge.stop();
        }
        if let Some(reason) = reason {
            self.log("info", &format!("whatsapp: stopping ({reason})"));
        }
        let mut receipt = self.cleanup_receipt();
        Box::pin(async move {
            loop {
                if let Some(outcome) = receipt.borrow().clone() {
                    return outcome.result;
                }
                receipt.changed().await.map_err(|_| {
                    ChannelError::error("whatsapp: cleanup worker exited without a result")
                })?;
            }
        })
    }
}

impl Shared {
    fn record_join(&self, result: Result<(), tokio::task::JoinError>, role: &str) {
        if let Err(error) = result {
            self.fatal_cleanup.store(true, Ordering::Release);
            self.cleanup_errors
                .lock()
                .expect("whatsapp cleanup errors")
                .push(ChannelError::error(format!(
                    "whatsapp: {role} cleanup failed: {error}"
                )));
        }
    }

    pub(super) fn prepare_start(&self) -> Result<(), ChannelError> {
        let mut cleanup = self.cleanup.lock().expect("whatsapp cleanup attempt");
        if let Some(attempt) = cleanup.as_ref() {
            if !attempt
                .result
                .borrow()
                .as_ref()
                .is_some_and(|outcome| outcome.result.is_ok())
            {
                return Err(ChannelError::error("whatsapp: prior stop has not drained"));
            }
            *cleanup = None;
        }
        if self.running.load(Ordering::Acquire)
            || self.bridge.lock().expect("whatsapp bridge").is_some()
        {
            return Err(ChannelError::error("whatsapp: channel is already started"));
        }
        Ok(())
    }
}
