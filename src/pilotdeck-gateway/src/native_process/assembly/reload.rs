//! pilotdeck.ts:136-193: one owned tail for adapter and automation reloads.
//! Telemetry and proxy effects remain in the synchronous admission prefix.
use super::automation::{
    ProjectAutomationBundle, ProjectAutomationConfig, ProjectAutomationReloadInput,
    ProjectAutomationStatus,
};
use crate::{
    config::store::{ConfigStore, Subscription},
    native_process::channels::NativeChannels,
    telemetry::{BoxFuture, TelemetryClient},
};
use pilotdeck_protocol::{JsString, JsValue};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{mpsc, watch};

struct Work {
    config: JsValue,
    adapters: bool,
    always_on: bool,
    cron: bool,
}
type Outcome = Option<Result<(), String>>;
type AdapterReload = Arc<dyn Fn(JsValue) -> BoxFuture<Result<Vec<String>, String>> + Send + Sync>;
type SubsystemReload = Arc<
    dyn Fn(JsValue, bool, bool) -> BoxFuture<Result<ProjectAutomationStatus, String>> + Send + Sync,
>;
type Log = Arc<dyn Fn(&str, &str) + Send + Sync>;
struct Actions {
    adapters: AdapterReload,
    subsystems: SubsystemReload,
    log: Log,
}
pub(super) struct ServerReloadDriver {
    subscription: Mutex<Option<Subscription>>,
    active: Arc<AtomicBool>,
    result: watch::Receiver<Outcome>,
}
struct Completion(Option<watch::Sender<Outcome>>);
impl Drop for Completion {
    fn drop(&mut self) {
        if let Some(done) = self.0.take() {
            done.send_replace(Some(Err(
                "Server reload task stopped before draining".into()
            )));
        }
    }
}
fn changed(paths: &[JsString], root: &str, include_root: bool) -> bool {
    let prefix: Vec<u16> = format!("{root}.").encode_utf16().collect();
    paths
        .iter()
        .any(|path| (include_root && path.equals_str(root)) || path.units().starts_with(&prefix))
}
fn spawn_queue(actions: Actions) -> (mpsc::UnboundedSender<Work>, watch::Receiver<Outcome>) {
    let (send, mut receive) = mpsc::unbounded_channel::<Work>();
    let (done, result) = watch::channel(None);
    let completion = Completion(Some(done));
    tokio::spawn(async move {
        let mut completion = completion;
        while let Some(work) = receive.recv().await {
            if work.adapters {
                match (actions.adapters)(work.config.clone()).await {
                    Ok(parts) if !parts.is_empty() => (actions.log)(
                        "info",
                        &format!(
                            "[pilotdeck] Adapter hot-reload complete: {}",
                            parts.join(", ")
                        ),
                    ),
                    Ok(_) => {}
                    Err(error) => (actions.log)(
                        "warn",
                        &format!("[pilotdeck] adapter hot-reload failed: {error}"),
                    ),
                }
            }
            if work.always_on || work.cron {
                match (actions.subsystems)(work.config, work.always_on, work.cron).await {
                    Ok(status) => {
                        let mut parts = Vec::new();
                        if work.always_on {
                            parts.push(format!(
                                "always-on={}",
                                if status.always_on {
                                    "started"
                                } else {
                                    "stopped"
                                }
                            ));
                        }
                        if work.cron {
                            parts.push(format!(
                                "cron={}",
                                if status.cron { "started" } else { "stopped" }
                            ));
                        }
                        (actions.log)(
                            "info",
                            &format!(
                                "[pilotdeck] Subsystem hot-reload complete: {}",
                                parts.join(", ")
                            ),
                        );
                    }
                    Err(error) => (actions.log)(
                        "warn",
                        &format!("[pilotdeck] subsystem reload failed: {error}"),
                    ),
                }
            }
        }
        if let Some(done) = completion.0.take() {
            done.send_replace(Some(Ok(())));
        }
    });
    (send, result)
}
impl ServerReloadDriver {
    pub fn new(
        store: &ConfigStore,
        channels: Arc<NativeChannels>,
        automation: Arc<ProjectAutomationBundle>,
        telemetry: Arc<dyn TelemetryClient>,
    ) -> Self {
        let adapter_channels = channels.clone();
        let log_channels = channels.clone();
        let (send, result) = spawn_queue(Actions {
            adapters: Arc::new(move |config| {
                let channels = adapter_channels.clone();
                Box::pin(async move {
                    let result = channels
                        .reload(&config)
                        .await
                        .map_err(|error| error.message().to_owned())?;
                    Ok(result
                        .started
                        .iter()
                        .map(|key| format!("{key}=started"))
                        .chain(result.stopped.iter().map(|key| format!("{key}=stopped")))
                        .collect())
                })
            }),
            subsystems: Arc::new(move |config, always_on_changed, cron_changed| {
                let automation = automation.clone();
                Box::pin(async move {
                    automation
                        .reload(ProjectAutomationReloadInput {
                            config: ProjectAutomationConfig {
                                always_on: config
                                    .get("alwaysOn")
                                    .map(crate::runtime::always_on::AlwaysOnConfig::from_section),
                                cron: config.get("cron").cloned(),
                            },
                            always_on_changed,
                            cron_changed,
                        })
                        .await
                        .map_err(|error| error.to_string())
                })
            }),
            log: Arc::new(move |level, message| log_channels.reload_log(level, message)),
        });
        let active = Arc::new(AtomicBool::new(false));
        let callback_active = active.clone();
        let subscription = store.subscribe(Arc::new(move |event| {
            let config = event
                .next_snapshot
                .get("config")
                .cloned()
                .unwrap_or(JsValue::Undefined);
            if changed(&event.changed_paths, "telemetry", true) {
                telemetry.set_enabled(
                    config
                        .get("telemetry")
                        .and_then(|t| t.get("enabled"))
                        .and_then(JsValue::as_bool)
                        == Some(true),
                );
            }
            if changed(&event.changed_paths, "proxy", true) {
                channels.apply_proxy_config(&config);
            }
            let adapters = callback_active.load(Ordering::SeqCst)
                && changed(&event.changed_paths, "adapters", false);
            let always_on = changed(&event.changed_paths, "alwaysOn", true);
            let cron = changed(&event.changed_paths, "cron", true);
            if (adapters || always_on || cron)
                && send
                    .send(Work {
                        config,
                        adapters,
                        always_on,
                        cron,
                    })
                    .is_err()
            {
                channels.reload_log(
                    "warn",
                    "[pilotdeck] subsystem reload failed: Server reload task stopped",
                );
            }
        }));
        Self {
            subscription: Mutex::new(Some(subscription)),
            active,
            result,
        }
    }
    pub fn activate(&self) {
        self.active.store(true, Ordering::SeqCst);
    }
    /// Close admission synchronously and retain the exact queued-work receipt.
    pub fn stop(&self) -> BoxFuture<Result<(), String>> {
        if let Some(subscription) = self
            .subscription
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            subscription.unsubscribe();
        }
        wait(self.result.clone())
    }
}
fn wait(mut result: watch::Receiver<Outcome>) -> BoxFuture<Result<(), String>> {
    Box::pin(async move {
        loop {
            if let Some(outcome) = result.borrow_and_update().clone() {
                return outcome;
            }
            if result.changed().await.is_err() {
                return Err("Server reload completion closed".into());
            }
        }
    })
}
impl Drop for ServerReloadDriver {
    fn drop(&mut self) {
        if let Some(subscription) = self
            .subscription
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            subscription.unsubscribe();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;
    use tokio::sync::oneshot;
    #[tokio::test]
    async fn one_tail_keeps_snapshot_order_across_held_adapter_failure_and_dropped_receipts() {
        let trace: Arc<Mutex<Vec<String>>> = Arc::default();
        let (release, gate) = oneshot::channel();
        let gate = Arc::new(Mutex::new(Some(gate)));
        let (started, start) = oneshot::channel();
        let started = Arc::new(Mutex::new(Some(started)));
        let adapter_trace = trace.clone();
        let subsystem_trace = trace.clone();
        let log_trace = trace.clone();
        let (send, receipt) = spawn_queue(Actions {
            adapters: Arc::new(move |config| {
                let id = config
                    .get("id")
                    .and_then(JsValue::as_string)
                    .unwrap()
                    .to_utf8()
                    .unwrap();
                let trace = adapter_trace.clone();
                let gate = gate.lock().unwrap().take();
                let started = started.lock().unwrap().take();
                Box::pin(async move {
                    trace.lock().unwrap().push(format!("adapter-start:{id}"));
                    if let Some(started) = started {
                        started.send(()).unwrap();
                    }
                    if let Some(gate) = gate {
                        gate.await.unwrap();
                    }
                    trace.lock().unwrap().push(format!("adapter-end:{id}"));
                    if id == "first" {
                        Err("fixture adapter failed".into())
                    } else {
                        Ok(Vec::new())
                    }
                })
            }),
            subsystems: Arc::new(move |config, ao, cron| {
                let id = config
                    .get("id")
                    .and_then(JsValue::as_string)
                    .unwrap()
                    .to_utf8()
                    .unwrap();
                let trace = subsystem_trace.clone();
                Box::pin(async move {
                    trace
                        .lock()
                        .unwrap()
                        .push(format!("subsystems:{id}:{ao}:{cron}"));
                    if id == "first" {
                        Err("fixture subsystem failed".into())
                    } else {
                        Ok(ProjectAutomationStatus {
                            always_on: ao,
                            cron,
                        })
                    }
                })
            }),
            log: Arc::new(move |level, _| {
                if level == "warn" {
                    log_trace.lock().unwrap().push("warn".into());
                }
            }),
        });
        let work = |id: &str, ao, cron| Work {
            config: crate::metadata::object([("id", id.into())]),
            adapters: true,
            always_on: ao,
            cron,
        };
        send.send(work("first", true, true))
            .unwrap_or_else(|_| panic!("worker stopped"));
        start.await.unwrap();
        send.send(work("second", false, true))
            .unwrap_or_else(|_| panic!("worker stopped"));
        let mut pending = wait(receipt.clone());
        assert!(pending.as_mut().now_or_never().is_none());
        drop(pending);
        assert_eq!(*trace.lock().unwrap(), ["adapter-start:first"]);
        release.send(()).unwrap();
        drop(send);
        wait(receipt).await.unwrap();
        assert_eq!(
            *trace.lock().unwrap(),
            [
                "adapter-start:first",
                "adapter-end:first",
                "warn",
                "subsystems:first:true:true",
                "warn",
                "adapter-start:second",
                "adapter-end:second",
                "subsystems:second:false:true"
            ]
        );
    }
}
