use super::*;
use crate::runtime::network::Cancellation;
use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::{Arc, Mutex, Weak},
};
type Disposal = tokio::sync::watch::Receiver<Option<Result<(), LspError>>>;
fn wait_cleanup(mut receiver: Disposal) -> LspFuture<()> {
    Box::pin(async move {
        loop {
            if let Some(result) = receiver.borrow().clone() {
                return result;
            }
            receiver
                .changed()
                .await
                .map_err(|_| LspError::new("LSP_DISPOSED", "LSP cleanup receipt was lost"))?;
        }
    })
}
fn owned_cleanup(
    slot: &mut Option<Disposal>,
    executor: Option<tokio::runtime::Handle>,
    operation: impl std::future::Future<Output = Result<(), LspError>> + Send + 'static,
) -> Disposal {
    if let Some(receiver) = slot {
        return receiver.clone();
    }
    let (done, receiver) = tokio::sync::watch::channel(None);
    match executor.or_else(|| tokio::runtime::Handle::try_current().ok()) {
        Some(runtime) => {
            *slot = Some(receiver.clone());
            crate::runtime::cleanup_task::spawn_with_error(
                &runtime,
                done,
                LspError::new("LSP_DISPOSED", "LSP cleanup task was dropped or panicked"),
                operation,
            );
        }
        None => {
            done.send_replace(Some(Err(LspError::new(
                "LSP_DISPOSED",
                "LSP cleanup requires a Tokio runtime",
            ))));
        }
    }
    receiver
}
fn dispose_entry(entry: Arc<Entry>) -> LspFuture<()> {
    let provider = entry.provider.clone();
    let executor = entry
        .executor
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let receiver = owned_cleanup(
        &mut entry.cleanup.lock().unwrap_or_else(|e| e.into_inner()),
        executor,
        async move { provider.dispose().await },
    );
    wait_cleanup(receiver)
}
struct Entry {
    provider: Arc<dyn LspProvider>,
    extensions: BTreeMap<String, String>,
    cleanup: Mutex<Option<Disposal>>,
    executor: Mutex<Option<tokio::runtime::Handle>>,
}
#[derive(Default)]
struct State {
    disposed: bool,
    entries: HashMap<String, Arc<Entry>>,
    order: Vec<String>,
    retiring: Vec<Arc<Entry>>,
    cleanup: Option<Disposal>,
    executor: Option<tokio::runtime::Handle>,
}
#[derive(Default, Clone)]
pub struct LspService {
    state: Arc<Mutex<State>>,
}
pub struct LspProviderRegistration {
    state: Weak<Mutex<State>>,
    entry: Arc<Entry>,
}
impl LspProviderRegistration {
    pub fn release(&self) -> LspFuture<()> {
        self.release_with_barrier(|| {})
    }
    fn release_with_barrier(&self, before_registry_lock: impl FnOnce()) -> LspFuture<()> {
        let entry = self.entry.clone();
        before_registry_lock();
        // Every caller crosses the same registry lock before starting or
        // joining cleanup. No early atomic flag can let repeat release
        // dispose a provider which is still selectable by a third query.
        if let Some(state) = self.state.upgrade() {
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            let id = entry.provider.id();
            if state
                .entries
                .get(id)
                .is_some_and(|current| Arc::ptr_eq(current, &entry))
            {
                state.entries.remove(id);
                state.retiring.push(entry.clone());
                state.order.retain(|value| value != id);
            }
        }
        // Capture/start the independent cleanup owner now. Its receipt survives
        // an unpolled/dropped caller, and repeated release joins the same task.
        // Service disposal uses this exact entry receipt as well.
        dispose_entry(entry)
    }
}
impl LspService {
    pub fn new() -> Self {
        let service = Self::default();
        if let Ok(executor) = tokio::runtime::Handle::try_current() {
            service.attach_runtime(executor);
        }
        service
    }
    /// Attach the existing native host executor at project stage. The token
    /// retains a handle even if the shared registry is subsequently dropped.
    pub(crate) fn attach_runtime(&self, executor: tokio::runtime::Handle) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.executor.is_none() {
            state.executor = Some(executor.clone());
        }
        let executor = state
            .executor
            .as_ref()
            .expect("attached LSP runtime")
            .clone();
        for entry in state.entries.values().chain(state.retiring.iter()) {
            let mut owner = entry.executor.lock().unwrap_or_else(|e| e.into_inner());
            if owner.is_none() {
                *owner = Some(executor.clone());
            }
        }
    }
    pub fn register_provider(
        &self,
        provider: Arc<dyn LspProvider>,
    ) -> Result<LspProviderRegistration, LspError> {
        if self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .disposed
        {
            return Err(disposed());
        }
        let id = provider.id().to_owned();
        if crate::metadata::js_trim(&id).is_empty() {
            return Err(LspError::new(
                "LSP_INVALID_PROVIDER",
                "LSP provider id must be a non-empty string",
            ));
        }
        let mapping = provider.extension_to_language();
        if mapping.is_empty() {
            return Err(LspError::new(
                "LSP_INVALID_PROVIDER",
                format!("LSP provider {id} has no extensions"),
            ));
        }
        let mut extensions = BTreeMap::new();
        for (ext, language) in mapping {
            let key = crate::metadata::js_trim(&ext).to_lowercase();
            if !key.starts_with('.') || key != ext || crate::metadata::js_trim(&language).is_empty()
            {
                return Err(LspError::new(
                    "LSP_INVALID_PROVIDER",
                    format!("LSP provider {id} has an invalid extension mapping"),
                ));
            }
            extensions.insert(key, language);
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.disposed {
            return Err(disposed());
        }
        if state.entries.contains_key(&id)
            || state.entries.values().any(|entry| {
                entry
                    .extensions
                    .keys()
                    .any(|ext| extensions.contains_key(ext))
            })
        {
            return Err(LspError::new(
                "LSP_CONFLICT",
                format!("LSP provider {id} conflicts with an existing provider"),
            ));
        }
        let executor = state
            .executor
            .clone()
            .or_else(|| tokio::runtime::Handle::try_current().ok());
        if state.executor.is_none() {
            state.executor = executor.clone();
        }
        let entry = Arc::new(Entry {
            provider,
            extensions,
            cleanup: Mutex::new(None),
            executor: Mutex::new(executor),
        });
        state.entries.insert(id.clone(), entry.clone());
        state.order.push(id);
        Ok(LspProviderRegistration {
            state: Arc::downgrade(&self.state),
            entry,
        })
    }
    pub fn query(
        &self,
        request: LspQueryRequest,
        cancellation: Cancellation,
    ) -> LspFuture<LspQueryResult> {
        let selected = (|| {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.disposed {
                return Err(disposed());
            }
            validate_request(&request)?;
            let ext = extension(&request.file_path);
            let entry = state
                .order
                .iter()
                .filter_map(|id| state.entries.get(id))
                .find(|e| e.extensions.contains_key(&ext))
                .cloned()
                .ok_or_else(|| {
                    LspError::new(
                        "LSP_UNAVAILABLE",
                        format!(
                            "No LSP provider is registered for {}",
                            if ext.is_empty() {
                                "the requested file"
                            } else {
                                &ext
                            }
                        ),
                    )
                })?;
            let language = entry.extensions[&ext].clone();
            Ok((entry, language))
        })();
        Box::pin(async move {
            let (entry, language_id) = selected?;
            entry
                .provider
                .query(
                    LspProviderQuery {
                        request,
                        language_id,
                    },
                    cancellation,
                )
                .await
        })
    }
    pub fn bind_session(&self, execution: LspSessionExecution) -> LspFuture<Arc<Self>> {
        let providers = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.disposed {
                return Box::pin(async { Err(disposed()) });
            }
            state
                .order
                .iter()
                .filter_map(|id| state.entries.get(id).map(|e| e.provider.clone()))
                .collect::<Vec<_>>()
        };
        Box::pin(async move {
            let bound = Arc::new(Self::new());
            for provider in providers {
                let selected = match provider.bind_session(execution.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        bound.dispose().await?;
                        return Err(e);
                    }
                };
                if let Err(e) = bound.register_provider(selected.clone()) {
                    selected.dispose().await?;
                    bound.dispose().await?;
                    return Err(e);
                }
            }
            Ok(bound)
        })
    }
    pub fn dispose(&self) -> LspFuture<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(receiver) = &state.cleanup {
            return wait_cleanup(receiver.clone());
        }
        let executor = state
            .executor
            .clone()
            .or_else(|| tokio::runtime::Handle::try_current().ok());
        if executor.is_none() {
            return Box::pin(async {
                Err(LspError::new(
                    "LSP_DISPOSED",
                    "LSP cleanup requires a Tokio runtime",
                ))
            });
        }
        state.disposed = true;
        let order = std::mem::take(&mut state.order);
        let mut entries = order
            .into_iter()
            .filter_map(|id| state.entries.remove(&id))
            .collect::<Vec<_>>();
        entries.append(&mut state.retiring);
        let operations = entries.into_iter().map(dispose_entry).collect::<Vec<_>>();
        let receiver = owned_cleanup(&mut state.cleanup, executor, async move {
            let errors = futures_util::future::join_all(operations)
                .await
                .into_iter()
                .filter_map(Result::err)
                .collect::<Vec<_>>();
            if errors.len() > 1 {
                Err(LspError::new(
                    "AggregateError",
                    "Failed to dispose LSP providers.",
                ))
            } else {
                errors.into_iter().next().map_or(Ok(()), Err)
            }
        });
        wait_cleanup(receiver)
    }
}
fn disposed() -> LspError {
    LspError::new("LSP_DISPOSED", "LSP service is disposed")
}
fn extension(file: &str) -> String {
    let name = file.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    name.rfind('.')
        .filter(|i| *i > 0 && name != "..")
        .map(|i| name[i..].to_lowercase())
        .unwrap_or_default()
}
fn validate_request(request: &LspQueryRequest) -> Result<(), LspError> {
    if crate::metadata::js_trim(&request.workspace_root.to_string_lossy()).is_empty() {
        return Err(LspError::new(
            "LSP_WORKSPACE_REQUIRED",
            "LSP query requires a workspace root",
        ));
    }
    if crate::metadata::js_trim(&request.file_path).is_empty() {
        return Err(LspError::new(
            "LSP_MALFORMED_RESPONSE",
            "LSP query requires a file path",
        ));
    }
    if [request.position.line, request.position.character]
        .iter()
        .any(|n| !n.is_finite() || n.fract() != 0.0 || *n < 0.0)
    {
        return Err(LspError::new(
            "LSP_MALFORMED_RESPONSE",
            "LSP position must contain non-negative integer coordinates",
        ));
    }
    let cwd = std::env::current_dir()
        .map_err(|e| LspError::new("LSP_MALFORMED_RESPONSE", e.to_string()))?;
    let root = crate::metadata::resolve_path(&cwd, &request.workspace_root);
    let file = crate::metadata::resolve(&root, &request.file_path);
    if !file.starts_with(&root)
        || file
            .strip_prefix(&root)
            .ok()
            .and_then(Path::to_str)
            .is_some_and(|p| p.starts_with(".."))
    {
        return Err(LspError::new(
            "LSP_MALFORMED_RESPONSE",
            format!(
                "LSP source {} resolves outside the workspace",
                request.file_path
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
