//! Cancellation belongs to one submission, not its reusable session/run keys.
use super::{SubmitError, TurnCompletionHandle};
use pilotdeck_protocol::JsString;
use std::{
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::watch;

pub(super) struct OwnedControl {
    reason: watch::Sender<Option<JsString>>,
    completion: Mutex<Option<TurnCompletionHandle>>,
    error: Mutex<Option<SubmitError>>,
    finished: AtomicBool,
}
impl OwnedControl {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            reason: watch::channel(None).0,
            completion: Mutex::new(None),
            error: Mutex::new(None),
            finished: AtomicBool::new(false),
        })
    }
    pub(super) fn cancel(&self, reason: JsString) {
        if self.finished.load(Ordering::SeqCst) {
            return;
        }
        self.reason.send_if_modified(|current| {
            if current.is_some() {
                false
            } else {
                *current = Some(reason);
                true
            }
        });
        if let Some(completion) = self.completion.lock().unwrap().as_ref()
            && let Some(reason) = self.reason()
        {
            completion.cancel_owned(&reason);
        }
    }
    pub(super) fn admitted(&self, completion: TurnCompletionHandle) {
        let mut slot = self.completion.lock().unwrap();
        if let Some(reason) = self.reason() {
            completion.cancel_owned(&reason);
        }
        *slot = Some(completion);
    }
    pub(super) fn reason(&self) -> Option<JsString> {
        self.reason.borrow().clone()
    }
    pub(super) async fn cancelled(&self) -> JsString {
        let mut reason = self.reason.subscribe();
        loop {
            if let Some(value) = reason.borrow_and_update().clone() {
                return value;
            }
            reason
                .changed()
                .await
                .expect("owned cancellation retains its sender");
        }
    }
    pub(super) fn record_error(&self, error: SubmitError) {
        self.error.lock().unwrap().get_or_insert(error);
    }
    pub(super) fn error(&self) -> Option<SubmitError> {
        self.error.lock().unwrap().clone()
    }
    pub(super) fn finish(&self, result: Result<(), SubmitError>) -> Result<(), SubmitError> {
        self.finished.store(true, Ordering::SeqCst);
        if let Err(error) = &result {
            self.record_error(error.clone());
        }
        result.and_then(|()| self.error().map_or(Ok(()), Err))
    }
}

/// Only cancellation-safe preparation futures use this helper. Durable
/// acceptance and terminal writes are always awaited to a definite outcome.
pub(super) async fn prepare<T>(
    control: Option<&Arc<OwnedControl>>,
    future: impl Future<Output = T>,
) -> Option<T> {
    match control {
        None => Some(future.await),
        Some(control) => tokio::select! {
            biased;
            _=control.cancelled()=>None,
            value=future=>Some(value),
        },
    }
}

#[cfg(test)]
#[path = "submit_owned_control_tests.rs"]
mod tests;
