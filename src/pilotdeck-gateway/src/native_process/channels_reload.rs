//! The adapter branch of pilotdeck.ts's config subscription. A single queue
//! keeps reloads in event order. Shutdown unsubscribes before draining that
//! queue, so no config callback can restart a channel after close finishes.

use super::NativeChannels;
use crate::{
    channel::protocol::ChannelError,
    config::store::{ConfigStore, Subscription},
};
use pilotdeck_protocol::{JsString, JsValue};
use std::sync::{Arc, Mutex, atomic::Ordering};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};

pub(super) struct ReloadDriver {
    subscription: Mutex<Option<Subscription>>,
    result: watch::Receiver<Option<Result<(), ChannelError>>>,
    _worker: JoinHandle<()>,
}

struct Completion(Option<watch::Sender<Option<Result<(), ChannelError>>>>);

impl Completion {
    fn finish(mut self) {
        if let Some(sender) = self.0.take() {
            sender.send_replace(Some(Ok(())));
        }
    }
}

impl Drop for Completion {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            sender.send_replace(Some(Err(ChannelError::error(
                "Channel reload task stopped before draining",
            ))));
        }
    }
}

impl ReloadDriver {
    pub(super) fn new(
        channels: &Arc<NativeChannels>,
        store: &ConfigStore,
    ) -> Result<Self, ChannelError> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|error| ChannelError::error(error.to_string()))?;
        let (send, mut receive) = mpsc::unbounded_channel::<JsValue>();
        let (complete, result) = watch::channel(None);
        // This guard is created before spawn: even cancellation before the
        // first poll settles the waiters instead of leaving an empty result.
        let completion = Completion(Some(complete));
        let weak = Arc::downgrade(channels);
        let worker = runtime.spawn(async move {
            while let Some(config) = receive.recv().await {
                let Some(channels) = weak.upgrade() else {
                    break;
                };
                match channels.reload(&config).await {
                    Ok(result) => {
                        let parts = result
                            .started
                            .iter()
                            .map(|key| format!("{key}=started"))
                            .chain(result.stopped.iter().map(|key| format!("{key}=stopped")))
                            .collect::<Vec<_>>();
                        if !parts.is_empty() {
                            channels.reload_log(
                                "info",
                                &format!(
                                    "[pilotdeck] Adapter hot-reload complete: {}",
                                    parts.join(", ")
                                ),
                            );
                        }
                    }
                    Err(error) => channels.reload_log(
                        "warn",
                        &format!("[pilotdeck] adapter hot-reload failed: {}", error.message()),
                    ),
                }
            }
            completion.finish();
        });
        let weak = Arc::downgrade(channels);
        let adapter_prefix = JsString::from("adapters.");
        let proxy_prefix = JsString::from("proxy.");
        let proxy_key = JsString::from("proxy");
        // Serialize the initial snapshot with any concurrently published
        // callbacks. Subscribe first so no subsequent event is lost.
        let snapshot_gate = Arc::new(Mutex::new(()));
        let guard = snapshot_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let callback_gate = snapshot_gate.clone();
        let subscription = store.subscribe(Arc::new(move |event| {
            let _guard = callback_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let adapter_changed = event
                .changed_paths
                .iter()
                .any(|path| path.units().starts_with(adapter_prefix.units()));
            let proxy_changed = event
                .changed_paths
                .iter()
                .any(|path| path == &proxy_key || path.units().starts_with(proxy_prefix.units()));
            if !adapter_changed && !proxy_changed {
                return;
            }
            let Some(channels) = weak.upgrade() else {
                return;
            };
            if channels.closing.load(Ordering::SeqCst) {
                return;
            }
            // Snapshot is owned by this event, not reread after another
            // reload might have published a newer configuration.
            let config = event
                .next_snapshot
                .get("config")
                .cloned()
                .unwrap_or(JsValue::Undefined);
            // TS updates the global dispatcher immediately, independently of
            // the serialized adapter queue. Proxy-only events must reach
            // adapters that keep running without reconstruction.
            if proxy_changed {
                channels.network.update_proxy(config.get("proxy"));
            }
            if !adapter_changed || !channels.reload_active.load(Ordering::SeqCst) {
                return;
            }
            if send.send(config).is_err() {
                channels.reload_log(
                    "warn",
                    "[pilotdeck] adapter hot-reload failed: Channel reload task stopped",
                );
            }
        }));
        let snapshot = store.snapshot();
        let current = snapshot
            .get("config")
            .and_then(|config| config.get("proxy"));
        if current != channels.config.get("proxy") {
            channels.network.update_proxy(current);
        }
        drop(guard);
        Ok(Self {
            subscription: Mutex::new(Some(subscription)),
            result,
            _worker: worker,
        })
    }

    pub(super) async fn drain(&self) -> Result<(), ChannelError> {
        self.unsubscribe();
        let mut result = self.result.clone();
        loop {
            if let Some(outcome) = result.borrow_and_update().clone() {
                return outcome;
            }
            result
                .changed()
                .await
                .map_err(|_| ChannelError::error("Channel reload task stopped without a result"))?;
        }
    }

    fn unsubscribe(&self) {
        if let Some(subscription) = self
            .subscription
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            subscription.unsubscribe();
            // Dropping the last listener drops its sender. Already queued
            // reloads still run before the worker observes EOF.
            drop(subscription);
        }
    }
}

impl Drop for ReloadDriver {
    fn drop(&mut self) {
        self.unsubscribe();
    }
}
