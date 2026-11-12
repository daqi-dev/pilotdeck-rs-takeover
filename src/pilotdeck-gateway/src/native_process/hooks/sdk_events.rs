//! Separate SDK ConfigChange/FileChanged runtimes and their session ownership.
use super::*;
use std::{collections::HashMap, sync::Weak};
use tokio::task::JoinHandle;

#[derive(Default)]
pub(in crate::native_process) struct SdkEventBus(Mutex<BusState>);
#[derive(Default)]
struct BusState {
    next: u64,
    listeners: HashMap<u64, Weak<State>>,
}
impl SdkEventBus {
    pub(in crate::native_process) fn publish(&self, payload: JsObject) {
        let listeners = {
            let mut bus = self.0.lock().unwrap_or_else(|e| e.into_inner());
            bus.listeners.retain(|_, entry| entry.strong_count() > 0);
            bus.listeners
                .values()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>()
        };
        for listener in listeners {
            listener.config_changed(payload.clone());
        }
    }
    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.lock().unwrap().listeners.len()
    }
}
#[derive(Default)]
struct Pending {
    closed: bool,
    tasks: Vec<JoinHandle<()>>,
}
struct State {
    config: Option<session::SessionLifecycle>,
    file: Option<session::SessionLifecycle>,
    session: JsString,
    cwd: JsString,
    pending: Mutex<Pending>,
}
impl State {
    fn input(&self, event: &str, payload: JsObject, query: JsString) -> LifecycleDispatchInput {
        let mut base = JsObject::new();
        base.insert("sessionId".into(), JsValue::String(self.session.clone()));
        base.insert("transcriptPath".into(), "".into());
        base.insert("cwd".into(), JsValue::String(self.cwd.clone()));
        LifecycleDispatchInput {
            event: event.into(),
            base_input: base,
            payload: Some(payload),
            match_query: Some(query),
            cwd: self.cwd.clone(),
            env: None,
            signal: None,
        }
    }
    fn config_changed(&self, payload: JsObject) {
        let Some(runtime) = &self.config else {
            return;
        };
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if pending.closed {
            return;
        }
        pending.tasks.retain(|task| !task.is_finished());
        let dispatch = runtime.dispatch(self.input("ConfigChange", payload, "ConfigChange".into()));
        pending.tasks.push(tokio::spawn(async move {
            let _ = dispatch.await;
        }));
    }
}
pub(in crate::native_process) struct SdkSessionEvents {
    state: Arc<State>,
    bus: Arc<SdkEventBus>,
    registration: Option<u64>,
}
impl SdkSessionEvents {
    pub(super) fn new(
        config: Option<&JsValue>,
        bus: Arc<SdkEventBus>,
        session: JsString,
        cwd: JsString,
        observers: &crate::runtime::hooks::runtime::HookObservers,
    ) -> Option<Self> {
        let settings = merge_sdk_hooks(Vec::new(), config, true);
        let runtime = |event: &str, async_hooks: bool| {
            let selected: HooksSettings = settings
                .iter()
                .filter(|(name, matchers)| name.equals_str(event) && !matchers.is_empty())
                .cloned()
                .collect();
            (!selected.is_empty()).then(|| {
                session::SessionLifecycle::new(crate::runtime::hooks::HookRuntime::with_options(
                    selected,
                    crate::runtime::hooks::HookRuntimeOptions {
                        shell: observers.shell.clone(),
                        observer: observers.execution.clone(),
                        on_async_registered: if async_hooks {
                            observers.async_registered.clone()
                        } else {
                            None
                        },
                        ..Default::default()
                    },
                ))
            })
        };
        let config = runtime("ConfigChange", false);
        let file = runtime("FileChanged", true);
        if config.is_none() && file.is_none() {
            return None;
        }
        let state = Arc::new(State {
            config,
            file,
            session,
            cwd,
            pending: Mutex::default(),
        });
        let registration = if state.config.is_some() {
            let mut listeners = bus.0.lock().unwrap_or_else(|e| e.into_inner());
            let id = listeners.next;
            listeners.next += 1;
            listeners.listeners.insert(id, Arc::downgrade(&state));
            Some(id)
        } else {
            None
        };
        Some(Self {
            state,
            bus,
            registration,
        })
    }
    pub(super) fn file_changed(&self, update: &JsObject) -> HostFuture<()> {
        let state = &self.state;
        let Some(runtime) = &state.file else {
            return Box::pin(async { Ok(()) });
        };
        let pending = state.pending.lock().unwrap_or_else(|e| e.into_inner());
        if pending.closed {
            return Box::pin(async { Ok(()) });
        }
        let relative = update
            .get("relativePath")
            .and_then(JsValue::as_string)
            .cloned()
            .unwrap_or_default();
        let mut payload = JsObject::new();
        payload.insert("filePath".into(), JsValue::String(relative.clone()));
        payload.insert(
            "absolutePath".into(),
            update
                .get("absolutePath")
                .cloned()
                .unwrap_or(JsValue::Undefined),
        );
        payload.insert("root".into(), JsValue::String(state.cwd.clone()));
        payload.insert(
            "changeType".into(),
            if update
                .get("changeKind")
                .and_then(JsValue::as_string)
                .is_some_and(|kind| kind.equals_str("create"))
            {
                "created".into()
            } else {
                "updated".into()
            },
        );
        let dispatch = runtime.dispatch(state.input("FileChanged", payload, relative));
        drop(pending);
        Box::pin(async move {
            let _ = dispatch.await;
            Ok(())
        })
    }
    pub(super) fn close(&self) -> HostFuture<()> {
        if let Some(id) = self.registration {
            self.bus
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .listeners
                .remove(&id);
        }
        let tasks = {
            let mut pending = self.state.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.closed = true;
            std::mem::take(&mut pending.tasks)
        };
        let runtimes = [self.state.config.clone(), self.state.file.clone()];
        for runtime in runtimes.iter().flatten() {
            runtime.close();
        }
        Box::pin(async move {
            let mut error = None;
            for task in tasks {
                if let Err(cause) = task.await {
                    error = Some(crate::host::ModuleError::new(cause.to_string()).into());
                }
            }
            for runtime in runtimes.into_iter().flatten() {
                if let Err(cause) = runtime.dispose().await {
                    error = Some(cause);
                }
            }
            match error {
                Some(error) => Err(error),
                None => Ok(()),
            }
        })
    }
}

impl crate::runtime::refresh::SdkConfigChange for SdkEventBus {
    fn dispatch_sdk_config_change(&self, change: &crate::runtime::refresh::ConfigChangeNotice) {
        if let JsValue::Object(payload) = &change.to_js() {
            self.publish(payload.clone());
        }
    }
}

#[cfg(test)]
#[path = "sdk_events_tests.rs"]
mod tests;
