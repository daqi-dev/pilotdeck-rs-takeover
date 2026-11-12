use super::*;
use futures_util::FutureExt;
use std::sync::atomic::Ordering;
impl Runtime {
    pub(super) fn spawn_task(
        self: &Arc<Self>,
        future: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> bool {
        let mut tasks = self.tasks.lock().expect("bluebubbles tasks");
        if !self.running.load(Ordering::Acquire) {
            return false;
        }
        let mut index = 0;
        while index < tasks.len() {
            if !tasks[index].is_finished() {
                index += 1;
                continue;
            }
            let task = tasks.swap_remove(index);
            if let Some(Err(error)) = task.now_or_never() {
                self.fatal_cleanup.store(true, Ordering::Release);
                self.cleanup_errors
                    .lock()
                    .expect("bluebubbles cleanup errors")
                    .push(ChannelError::error(format!(
                        "bluebubbles: owned task failed: {error}"
                    )));
            }
        }
        let handle = tokio::spawn(future);
        if self.owned {
            tasks.push(handle);
        }
        true
    }
    pub(super) fn prepare_start(&self) -> Result<(), ChannelError> {
        if self.owned {
            let cleanup = self.cleanup.lock().expect("bluebubbles cleanup");
            if let Some(receipt) = cleanup.as_ref() {
                match receipt.borrow().as_ref() {
                    Some(Ok(())) => {}
                    _ => {
                        return Err(ChannelError::error(
                            "bluebubbles: previous instance cleanup is unresolved",
                        ));
                    }
                }
            }
        }
        *self.cleanup.lock().expect("bluebubbles cleanup") = None;
        Ok(())
    }
    pub(super) fn stop_owned(
        self: &Arc<Self>,
        reason: &str,
    ) -> BoxFuture<Result<(), ChannelError>> {
        {
            let _admission = self.tasks.lock().expect("bluebubbles tasks");
            self.running.store(false, Ordering::Release);
            self.poll_abort
                .lock()
                .expect("bluebubbles stop signal")
                .send_replace(true);
        }
        let mut cleanup = self.cleanup.lock().expect("bluebubbles cleanup");
        let retry = cleanup
            .as_ref()
            .is_some_and(|receipt| matches!(receipt.borrow().as_ref(), Some(Err(_))));
        self.log(
            "info",
            &format!(
                "bluebubbles: stopping ({})",
                if reason.is_empty() {
                    "no reason"
                } else {
                    reason
                }
            ),
        );
        if cleanup.is_none() || retry {
            let (shared, sender) = (self.clone(), watch::channel(None));
            let (completed, receipt) = sender;
            struct Completion(
                watch::Sender<Option<Result<(), ChannelError>>>,
                Arc<Runtime>,
            );
            impl Drop for Completion {
                fn drop(&mut self) {
                    if self.0.borrow().is_none() {
                        self.1.fatal_cleanup.store(true, Ordering::Release);
                        self.0.send_replace(Some(Err(ChannelError::error(
                            "bluebubbles: cleanup worker terminated before drain",
                        ))));
                    }
                }
            }
            let completion = Completion(completed, shared.clone());
            tokio::spawn(async move {
                let retry_streams = if retry {
                    std::mem::take(
                        &mut *shared
                            .failed_streams
                            .lock()
                            .expect("bluebubbles failed streams"),
                    )
                } else {
                    Vec::new()
                };
                let tasks = std::mem::take(&mut *shared.tasks.lock().expect("bluebubbles tasks"));
                for task in tasks {
                    if let Err(error) = task.await {
                        shared.fatal_cleanup.store(true, Ordering::Release);
                        shared
                            .cleanup_errors
                            .lock()
                            .expect("bluebubbles cleanup errors")
                            .push(ChannelError::error(format!(
                                "bluebubbles: owned task failed: {error}"
                            )));
                    }
                }
                for mut stream in retry_streams {
                    if let Err(error) = stream
                        .cancel_and_drain("bluebubbles channel stopped".into())
                        .await
                    {
                        shared
                            .cleanup_errors
                            .lock()
                            .expect("bluebubbles cleanup errors")
                            .push(error);
                        shared
                            .failed_streams
                            .lock()
                            .expect("bluebubbles failed streams")
                            .push(stream);
                    }
                }
                let errors = {
                    let mut errors = shared
                        .cleanup_errors
                        .lock()
                        .expect("bluebubbles cleanup errors");
                    if shared.fatal_cleanup.load(Ordering::Acquire) {
                        errors.clone()
                    } else {
                        std::mem::take(&mut *errors)
                    }
                };
                let result = if errors.is_empty() && !shared.fatal_cleanup.load(Ordering::Acquire) {
                    let chats: Vec<_> = shared
                        .known_chats
                        .lock()
                        .expect("bluebubbles known chats")
                        .iter()
                        .cloned()
                        .collect();
                    for chat in chats {
                        let chat = String::from_utf16_lossy(chat.units());
                        shared.interaction.elicitation_clear(&chat);
                        shared.interaction.permission_clear_after_turn(&chat);
                    }
                    Ok(())
                } else {
                    Err(ChannelError::Aggregate {
                        message: "bluebubbles: owned cleanup failed".into(),
                        errors,
                    })
                };
                completion.0.send_replace(Some(result));
            });
            *cleanup = Some(receipt);
        }
        let mut receipt = cleanup.as_ref().expect("cleanup receipt").clone();
        Box::pin(async move {
            loop {
                if let Some(result) = receipt.borrow().clone() {
                    return result;
                }
                receipt
                    .changed()
                    .await
                    .map_err(|_| ChannelError::error("bluebubbles: cleanup receipt lost"))?;
            }
        })
    }
}
