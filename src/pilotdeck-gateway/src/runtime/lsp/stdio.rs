//! Short-lived, session-owned JSON-RPC language servers over stdio.
//! Every accepted query owns its process through real pipe/process teardown,
//! including when the caller drops its waiter. No server pool is installed.
use super::{
    LspCommand, LspError, LspFuture, LspLocation, LspOperation, LspPosition, LspProvider,
    LspProviderQuery, LspQueryResult, LspRange, LspSessionExecution,
};
use crate::{process_group::GroupGuard, runtime::network::Cancellation};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{oneshot, watch},
};

fn error(code: &str, message: impl Into<String>) -> LspError {
    LspError {
        code: code.into(),
        message: message.into(),
    }
}
fn source_io_error(syscall: &str, path: &Path, cause: std::io::Error) -> LspError {
    let (code, message) =
        crate::node_io::diagnostic_or_unknown(&cause, syscall, Some(&path.to_string_lossy()));
    error(code, message)
}

fn malformed(message: impl Into<String>) -> LspError {
    error("LSP_MALFORMED_RESPONSE", message)
}
fn aborted() -> LspError {
    error("ABORT_ERR", "LSP query aborted")
}
fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, Debug)]
pub struct StdioLspConfig {
    pub id: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub extension_to_language: BTreeMap<String, String>,
    pub initialization_options: Option<Value>,
    pub configuration: Option<Value>,
    pub request_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub max_document_bytes: u64,
}
impl StdioLspConfig {
    pub fn new(
        id: impl Into<String>,
        command: impl Into<String>,
        extension_to_language: BTreeMap<String, String>,
    ) -> Self {
        Self {
            id: id.into(),
            command: command.into(),
            args: vec![],
            env: BTreeMap::new(),
            extension_to_language,
            initialization_options: None,
            configuration: None,
            request_timeout: Duration::from_secs(30),
            shutdown_timeout: Duration::from_secs(5),
            max_document_bytes: 4_000_000,
        }
    }
}
#[derive(Default)]
struct Active {
    disposed: bool,
    next: u64,
    queries: BTreeMap<u64, QueryControl>,
}
struct QueryControl {
    stop: Cancellation,
    done: watch::Receiver<Option<Result<(), LspError>>>,
}
pub struct StdioLspProvider {
    config: StdioLspConfig,
    execution: Option<LspSessionExecution>,
    active: Arc<Mutex<Active>>,
}
impl StdioLspProvider {
    pub fn new(mut config: StdioLspConfig) -> Result<Arc<Self>, LspError> {
        if config.max_document_bytes == 0 {
            return Err(malformed("LSP maxDocumentBytes must be a positive integer"));
        }
        if config.shutdown_timeout.is_zero() {
            return Err(malformed(
                "LSP shutdownTimeoutMs must be a positive integer",
            ));
        }
        if config.request_timeout.is_zero() {
            return Err(malformed("LSP requestTimeoutMs must be a positive integer"));
        }
        let mut normalized = BTreeMap::new();
        for (raw, language) in &config.extension_to_language {
            let extension = raw.trim().to_ascii_lowercase();
            let suffix = extension.strip_prefix('.').unwrap_or("");
            if !suffix
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
                || !suffix
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
                || language.trim().is_empty()
            {
                return Err(malformed(format!("Invalid LSP extension mapping: {raw}")));
            }
            normalized.insert(extension, language.clone());
        }
        config.extension_to_language = normalized;
        Ok(Arc::new(Self {
            config,
            execution: None,
            active: Arc::default(),
        }))
    }
}
struct QueryRegistration {
    active: Arc<Mutex<Active>>,
    id: u64,
    done: Option<watch::Sender<Option<Result<(), LspError>>>>,
}
impl QueryRegistration {
    fn finish(mut self, result: Result<(), LspError>) {
        let success = result.is_ok();
        if let Some(done) = self.done.take() {
            done.send_replace(Some(result));
        }
        if success {
            lock(&self.active).queries.remove(&self.id);
        }
    }
}
impl Drop for QueryRegistration {
    fn drop(&mut self) {
        if let Some(done) = self.done.take() {
            done.send_replace(Some(Err(error(
                "LSP_DISPOSED",
                "LSP query owner was lost before process cleanup settled",
            ))));
            // Keep this failed receipt discoverable by dispose. Never claim
            // the server was settled merely because a task was dropped.
        }
    }
}
struct CancelWaiter {
    stop: Cancellation,
    armed: bool,
}
impl Drop for CancelWaiter {
    fn drop(&mut self) {
        if self.armed {
            self.stop.cancel();
        }
    }
}
impl LspProvider for StdioLspProvider {
    fn id(&self) -> &str {
        &self.config.id
    }
    fn extension_to_language(&self) -> BTreeMap<String, String> {
        self.config.extension_to_language.clone()
    }
    fn bind_session(
        &self,
        execution: LspSessionExecution,
    ) -> Result<Arc<dyn LspProvider>, LspError> {
        Ok(Arc::new(Self {
            config: self.config.clone(),
            execution: Some(execution),
            active: Arc::default(),
        }))
    }
    fn query(
        &self,
        request: LspProviderQuery,
        cancellation: Cancellation,
    ) -> LspFuture<LspQueryResult> {
        let runtime = match tokio::runtime::Handle::try_current() {
            Ok(runtime) => runtime,
            Err(e) => return Box::pin(async move { Err(error("LSP_UNAVAILABLE", e.to_string())) }),
        };
        let stop = Cancellation::new();
        let (done, receipt) = watch::channel(None);
        let registration = {
            let mut active = lock(&self.active);
            if active.disposed {
                return Box::pin(async { Err(error("LSP_DISPOSED", "LSP provider is disposed")) });
            }
            let id = active.next;
            active.next += 1;
            active.queries.insert(
                id,
                QueryControl {
                    stop: stop.clone(),
                    done: receipt,
                },
            );
            QueryRegistration {
                active: self.active.clone(),
                id,
                done: Some(done),
            }
        };
        let config = self.config.clone();
        let execution = self.execution.clone();
        let worker_stop = stop.clone();
        let (reply, receive) = oneshot::channel();
        runtime.spawn(async move {
            let result = run_query(config, execution, request, cancellation, worker_stop).await;
            let (business, cleanup) = result;
            let output = match cleanup.clone() {
                Ok(()) => business,
                Err(e) => Err(e),
            };
            registration.finish(cleanup);
            let _ = reply.send(output);
        });
        // Construct the cancellation owner now, including an unpolled waiter.
        let mut guard = CancelWaiter { stop, armed: true };
        Box::pin(async move {
            let result = receive
                .await
                .map_err(|_| error("LSP_DISPOSED", "LSP query owner did not return a result"))?;
            guard.armed = false;
            result
        })
    }
    fn dispose(&self) -> LspFuture<()> {
        let controls = {
            let mut active = lock(&self.active);
            active.disposed = true;
            active
                .queries
                .values()
                .map(|query| {
                    query.stop.cancel();
                    query.done.clone()
                })
                .collect::<Vec<_>>()
        };
        Box::pin(async move {
            let mut first = None;
            for mut done in controls {
                loop {
                    let result = done.borrow_and_update().clone();
                    if let Some(result) = result {
                        if let Err(e) = result {
                            first.get_or_insert(e);
                        }
                        break;
                    }
                    if done.changed().await.is_err() {
                        first.get_or_insert(error("LSP_DISPOSED", "LSP cleanup receipt was lost"));
                        break;
                    }
                }
            }
            first.map_or(Ok(()), Err)
        })
    }
}

fn absolute(path: &Path) -> Result<PathBuf, LspError> {
    let full = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|e| malformed(e.to_string()))?
            .join(path)
    };
    let mut result = PathBuf::new();
    for component in full.components() {
        match component {
            Component::ParentDir => {
                result.pop();
            }
            Component::CurDir => {}
            other => result.push(other.as_os_str()),
        }
    }
    Ok(result)
}
fn uri(path: &Path) -> Result<String, LspError> {
    url::Url::from_file_path(path)
        .map(|url| url.to_string())
        .map_err(|_| malformed("Invalid LSP file path"))
}
async fn source(
    config: &StdioLspConfig,
    execution: Option<&LspSessionExecution>,
    request: &LspProviderQuery,
    cancel: &Cancellation,
) -> Result<(LspCommand, String, String, PathBuf), LspError> {
    if cancel.is_cancelled() {
        return Err(aborted());
    }
    let workspace = absolute(&request.request.workspace_root)?;
    let workspace_name = workspace
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "workspace".into());
    if execution.is_some_and(|e| absolute(&e.workspace_root).ok().as_ref() != Some(&workspace)) {
        return Err(error(
            "LSP_WORKSPACE_REQUIRED",
            "LSP workspace does not match its session",
        ));
    }
    // Fixed TS checks the workspace before resolving or reading its source.
    let workspace_is_directory = if let Some(execution) = execution {
        execution
            .io
            .stat(workspace.clone(), cancel.clone())
            .await?
            .is_directory
    } else {
        tokio::fs::metadata(&workspace)
            .await
            .map_err(|e| source_io_error("stat", &workspace, e))?
            .is_dir()
    };
    if !workspace_is_directory {
        return Err(malformed(format!(
            "LSP workspace is not a directory: {}",
            request.request.workspace_root.display(),
        )));
    }
    let input = Path::new(&request.request.file_path);
    let file = absolute(&if input.is_absolute() {
        input.to_owned()
    } else {
        workspace.join(input)
    })?;
    let relative = file.strip_prefix(&workspace).map_err(|_| {
        malformed(format!(
            "LSP source resolves outside the workspace: {}",
            request.request.file_path
        ))
    })?;
    // Match baseline's relativePath.startsWith('..') guard even for '..name'.
    if relative.to_string_lossy().starts_with("..") {
        return Err(malformed(format!(
            "LSP source resolves outside the workspace: {}",
            request.request.file_path
        )));
    }
    let bytes = if let Some(execution) = execution {
        let canonical = execution.io.realpath(file.clone(), cancel.clone()).await?;
        // The selected native port returns its constructor-pinned canonical
        // root here, never the current path after a root replacement. Keep
        // logical workspace URIs/names separate from this containment proof.
        let canonical_workspace = execution
            .io
            .realpath(workspace.clone(), cancel.clone())
            .await?;
        if !canonical.starts_with(&canonical_workspace) {
            return Err(malformed(
                "LSP source resolves outside its session workspace",
            ));
        }
        let info = execution.io.stat(file.clone(), cancel.clone()).await?;
        if !info.is_file {
            return Err(malformed(format!(
                "LSP source is not a file: {}",
                request.request.file_path
            )));
        }
        if info.size > config.max_document_bytes {
            return Err(malformed(format!(
                "LSP source exceeds {} bytes",
                config.max_document_bytes
            )));
        }
        execution
            .io
            .read_file_bounded(file.clone(), config.max_document_bytes, cancel.clone())
            .await?
    } else {
        let info = tokio::fs::metadata(&file)
            .await
            .map_err(|e| source_io_error("stat", &file, e))?;
        if !info.is_file() {
            return Err(malformed(format!(
                "LSP source is not a file: {}",
                request.request.file_path
            )));
        }
        if info.len() > config.max_document_bytes {
            return Err(malformed(format!(
                "LSP source exceeds {} bytes",
                config.max_document_bytes
            )));
        }
        // Unbound legacy providers retain their normal symlink semantics;
        // enforce the existing document byte contract during the one fd read.
        let opened = tokio::fs::File::open(&file)
            .await
            .map_err(|e| source_io_error("open", &file, e))?;
        let mut bytes = Vec::new();
        opened
            .take(config.max_document_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .await
            .map_err(|e| source_io_error("read", &file, e))?;
        bytes
    };
    if cancel.is_cancelled() {
        return Err(aborted());
    }
    if bytes.len() as u64 > config.max_document_bytes {
        return Err(malformed(format!(
            "LSP source exceeds {} bytes",
            config.max_document_bytes
        )));
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let mut command = LspCommand {
        command: config.command.clone(),
        args: config.args.clone(),
        cwd: workspace.clone(),
        env: std::env::vars().collect(),
        workspace_uri: Some(uri(&workspace)?),
        source_uri: Some(uri(&file)?),
    };
    command.env.extend(config.env.clone());
    if let Some(execution) = execution {
        command = execution
            .io
            .prepare_subprocess(command, cancel.clone())
            .await?;
    }
    Ok((command, text, workspace_name, workspace))
}
async fn run_query(
    config: StdioLspConfig,
    execution: Option<LspSessionExecution>,
    request: LspProviderQuery,
    cancellation: Cancellation,
    stop: Cancellation,
) -> (Result<LspQueryResult, LspError>, Result<(), LspError>) {
    let prepared = tokio::select! { biased; _ = stop.cancelled() => Err(aborted()), _ = cancellation.cancelled() => Err(aborted()), result = source(&config, execution.as_ref(), &request, &cancellation) => result };
    let (command, text, workspace_name, workspace_root) = match prepared {
        Ok(value) => value,
        Err(e) => return (Err(e), Ok(())),
    };
    if stop.is_cancelled() || cancellation.is_cancelled() {
        return (Err(aborted()), Ok(()));
    }
    let workspace_uri = command
        .workspace_uri
        .clone()
        .unwrap_or_else(|| uri(&workspace_root).unwrap_or_default());
    let file_uri = match command.source_uri.clone() {
        Some(uri) => uri,
        None => {
            return (
                Err(malformed("Prepared LSP command lacks source URI")),
                Ok(()),
            );
        }
    };
    let mut process = match Process::spawn(
        &command,
        config.clone(),
        workspace_uri.clone(),
        workspace_name,
    ) {
        Ok(p) => p,
        Err(e) => return (Err(e), Ok(())),
    };
    let result = process
        .query(&request, &file_uri, text, &cancellation, &stop)
        .await;
    let result = if execution.is_some() {
        result.and_then(|result| remap_locations(result, &workspace_root, &workspace_uri))
    } else {
        result
    };
    let cleanup = process.close().await;
    (result, cleanup)
}

struct Parts {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr: tokio::task::JoinHandle<std::io::Result<u64>>,
    group: GroupGuard,
}
struct Process {
    parts: Option<Parts>,
    config: StdioLspConfig,
    workspace_uri: String,
    workspace_name: String,
    buffer: Vec<u8>,
    next: u64,
}
impl Process {
    fn spawn(
        command: &LspCommand,
        config: StdioLspConfig,
        workspace_uri: String,
        workspace_name: String,
    ) -> Result<Self, LspError> {
        let mut spawn = Command::new(&command.command);
        spawn
            .args(&command.args)
            .current_dir(&command.cwd)
            .env_clear()
            .envs(&command.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        spawn.process_group(0);
        let mut child = spawn
            .spawn()
            .map_err(|e| error("LSP_UNAVAILABLE", e.to_string()))?;
        #[cfg(unix)]
        let group = GroupGuard(
            child
                .id()
                .and_then(|pid| rustix::process::Pid::from_raw(pid as i32)),
        );
        #[cfg(not(unix))]
        let group = GroupGuard(None);
        let stdin = child.stdin.take().expect("piped LSP stdin");
        let stdout = child.stdout.take().expect("piped LSP stdout");
        let mut stderr = child.stderr.take().expect("piped LSP stderr");
        let stderr =
            tokio::spawn(async move { tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await });
        Ok(Self {
            parts: Some(Parts {
                child,
                stdin,
                stdout,
                stderr,
                group,
            }),
            config,
            workspace_uri,
            workspace_name,
            buffer: vec![],
            next: 1,
        })
    }
    async fn write(&mut self, message: Value) -> Result<(), LspError> {
        let bytes = serde_json::to_vec(&message).map_err(|e| malformed(e.to_string()))?;
        let stdin = &mut self.parts.as_mut().expect("live LSP process").stdin;
        stdin
            .write_all(format!("Content-Length: {}\r\n\r\n", bytes.len()).as_bytes())
            .await
            .map_err(|e| error("LSP_UNAVAILABLE", e.to_string()))?;
        stdin
            .write_all(&bytes)
            .await
            .map_err(|e| error("LSP_UNAVAILABLE", e.to_string()))
    }
    async fn notify(&mut self, method: &str, params: Value) -> Result<(), LspError> {
        let limit = self.config.request_timeout;
        tokio::time::timeout(
            limit,
            self.write(json!({"jsonrpc":"2.0","method":method,"params":params})),
        )
        .await
        .map_err(|_| error("LSP_TIMEOUT", format!("LSP request timed out: {method}")))?
    }
    async fn read(&mut self) -> Result<Value, LspError> {
        loop {
            if let Some(split) = self.buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                let header = String::from_utf8_lossy(&self.buffer[..split]);
                let length = header
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        if name.eq_ignore_ascii_case("Content-Length") {
                            value.trim().parse::<usize>().ok()
                        } else {
                            None
                        }
                    })
                    .ok_or_else(|| {
                        malformed("Malformed LSP response: missing Content-Length header")
                    })?;
                let end = (split + 4)
                    .checked_add(length)
                    .ok_or_else(|| malformed("Malformed LSP response: invalid Content-Length"))?;
                if self.buffer.len() >= end {
                    let result = serde_json::from_slice::<Value>(&self.buffer[split + 4..end])
                        .map_err(|e| malformed(format!("Malformed LSP response: {e}")))?;
                    self.buffer.drain(..end);
                    if !result.is_object() && !result.is_array() {
                        return Err(malformed(
                            "Malformed LSP response: response is not an object",
                        ));
                    }
                    return Ok(result);
                }
            }
            let read = self
                .parts
                .as_mut()
                .expect("live LSP process")
                .stdout
                .read_buf(&mut self.buffer)
                .await
                .map_err(|e| error("LSP_UNAVAILABLE", e.to_string()))?;
            if read == 0 {
                return Err(error("LSP_UNAVAILABLE", "LSP process exited"));
            }
        }
    }
    async fn request(
        &mut self,
        method: &str,
        params: Value,
        cancellation: &Cancellation,
        stop: &Cancellation,
    ) -> Result<Value, LspError> {
        let id = self.next;
        self.next += 1;
        let limit = self.config.request_timeout;
        let exchange = async {
            self.write(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
                .await?;
            loop {
                let message = self.read().await?;
                if let (Some(method), Some(server_id)) = (
                    message.get("method").and_then(Value::as_str),
                    message.get("id"),
                ) && (server_id.is_string() || server_id.is_number())
                {
                    let reply = server_reply(
                        method,
                        message.get("params"),
                        &self.workspace_uri,
                        &self.workspace_name,
                        self.config.configuration.as_ref(),
                    );
                    self.write(json!({"jsonrpc":"2.0","id":server_id,"result":reply}))
                        .await?;
                    continue;
                }
                if message.get("id").and_then(Value::as_u64) != Some(id) {
                    continue;
                }
                if let Some(failure) = message
                    .get("error")
                    .filter(|v| v.is_object() || v.is_array())
                {
                    return Err(malformed(format!("LSP request failed: {failure}")));
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
        };
        let result = tokio::select! { biased;
            _ = cancellation.cancelled() => Err(aborted()),
            _ = stop.cancelled() => Err(aborted()),
            result = tokio::time::timeout(limit, exchange) => result.unwrap_or_else(|_| Err(error("LSP_TIMEOUT", format!("LSP request timed out: {method}")))),
        };
        if result
            .as_ref()
            .is_err_and(|e| e.code == "LSP_TIMEOUT" || e.code == "ABORT_ERR")
        {
            let _ = self.notify("$/cancelRequest", json!({"id":id})).await;
        }
        result
    }
    async fn query(
        &mut self,
        query: &LspProviderQuery,
        file: &str,
        text: String,
        cancellation: &Cancellation,
        stop: &Cancellation,
    ) -> Result<LspQueryResult, LspError> {
        self.request("initialize", json!({"processId":null,"rootUri":self.workspace_uri,"workspaceFolders":[{"uri":self.workspace_uri,"name":self.workspace_name}],"capabilities":{},"initializationOptions":self.config.initialization_options}), cancellation, stop).await?;
        self.notify("initialized", json!({})).await?;
        self.notify("textDocument/didOpen", json!({"textDocument":{"uri":file,"languageId":query.language_id,"version":1,"text":text}})).await?;
        let method = match query.request.operation {
            LspOperation::Definition => "textDocument/definition",
            LspOperation::References => "textDocument/references",
            LspOperation::Implementation => "textDocument/implementation",
            LspOperation::Hover => "textDocument/hover",
        };
        let mut params = json!({"textDocument":{"uri":file},"position":query.request.position});
        if matches!(query.request.operation, LspOperation::References) {
            params["context"] = json!({"includeDeclaration":true});
        }
        let result = self
            .request(method, params, cancellation, stop)
            .await
            .and_then(|value| normalize(query.request.operation, value, &self.workspace_uri));
        let _ = self
            .notify(
                "textDocument/didClose",
                json!({"textDocument":{"uri":file}}),
            )
            .await;
        result
    }
    async fn close(&mut self) -> Result<(), LspError> {
        let never = Cancellation::new();
        let _ = self.request("shutdown", Value::Null, &never, &never).await;
        let _ = self.notify("exit", json!({})).await;
        cleanup_parts(
            self.parts.take().expect("owned LSP process"),
            self.config.shutdown_timeout,
        )
        .await
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        if let Some(parts) = self.parts.take() {
            // The group guard remains armed in the owned task until real reap.
            parts.group.signal(rustix::process::Signal::KILL);
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = cleanup_parts(parts, Duration::from_millis(500)).await;
                });
            }
        }
    }
}
async fn cleanup_parts(mut parts: Parts, grace: Duration) -> Result<(), LspError> {
    let mut first = None;
    #[cfg(unix)]
    {
        let exited = match crate::process_group::wait_for_exit_unreaped(parts.group.0, grace).await
        {
            Ok(exited) => exited,
            Err(e) => {
                first.get_or_insert(error("LSP_DISPOSED", e.to_string()));
                false
            }
        };
        if !exited {
            parts.group.signal(rustix::process::Signal::TERM);
            if let Err(e) = crate::process_group::wait_for_exit_unreaped(
                parts.group.0,
                Duration::from_millis(500),
            )
            .await
            {
                first.get_or_insert(error("LSP_DISPOSED", e.to_string()));
            }
        }
        // Clear descendants while the unreaped leader still reserves its PGID.
        parts.group.signal(rustix::process::Signal::KILL);
    }
    #[cfg(not(unix))]
    if tokio::time::timeout(grace, parts.child.wait())
        .await
        .is_err()
    {
        let _ = parts.child.start_kill();
    }
    drop(parts.stdin);
    let reaped = match tokio::time::timeout(Duration::from_secs(2), parts.child.wait()).await {
        Ok(Ok(_)) => true,
        Ok(Err(e)) => {
            first.get_or_insert(error("LSP_DISPOSED", e.to_string()));
            false
        }
        Err(_) => {
            first.get_or_insert(error("LSP_DISPOSED", "LSP process did not exit after kill"));
            false
        }
    };
    crate::process_group::wait_for_group_empty(parts.group.0, Duration::from_secs(2)).await;
    let group_empty = !parts
        .group
        .0
        .is_some_and(crate::process_group::group_exists);
    if !group_empty {
        first.get_or_insert(error(
            "LSP_DISPOSED",
            "LSP process group still has owned members",
        ));
    }
    if reaped && group_empty {
        parts.group.disarm();
    }
    drop(parts.stdout);
    match tokio::time::timeout(Duration::from_secs(2), &mut parts.stderr).await {
        Ok(Ok(Ok(_))) => {}
        Ok(Ok(Err(e))) => {
            first.get_or_insert(error("LSP_DISPOSED", e.to_string()));
        }
        Ok(Err(e)) => {
            first.get_or_insert(error("LSP_DISPOSED", e.to_string()));
        }
        Err(_) => {
            parts.stderr.abort();
            let _ = parts.stderr.await;
            first.get_or_insert(error("LSP_DISPOSED", "LSP stderr did not close"));
        }
    }
    first.map_or(Ok(()), Err)
}

fn server_reply(
    method: &str,
    params: Option<&Value>,
    workspace: &str,
    name: &str,
    configuration: Option<&Value>,
) -> Value {
    match method {
        "workspace/configuration" => Value::Array(
            params
                .and_then(|p| p.get("items"))
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .map(|item| {
                            if !item.is_object() {
                                return Value::Null;
                            }
                            match item.get("section").and_then(Value::as_str) {
                                Some(section) if configuration.is_some_and(|v| v.is_object()) => {
                                    configuration
                                        .and_then(|v| v.get(section))
                                        .cloned()
                                        .unwrap_or(Value::Null)
                                }
                                _ => configuration.cloned().unwrap_or(Value::Null),
                            }
                        })
                        .collect()
                })
                .unwrap_or_default(),
        ),
        "workspace/workspaceFolders" => json!([{"uri":workspace,"name":name}]),
        "workspace/applyEdit" => {
            json!({"applied":false,"failureReason":"LSP provider does not apply workspace edits."})
        }
        _ => Value::Null,
    }
}
fn position(value: &Value) -> Option<LspPosition> {
    let coordinate = |value: &Value| {
        let value = value.as_f64()?;
        (value.is_finite() && value.fract() == 0.0 && value >= 0.0).then_some(value)
    };
    Some(LspPosition {
        line: coordinate(value.get("line")?)?,
        character: coordinate(value.get("character")?)?,
    })
}
fn range(value: &Value) -> Option<LspRange> {
    Some(LspRange {
        start: position(value.get("start")?)?,
        end: position(value.get("end")?)?,
    })
}
fn render(value: &Value) -> Result<String, LspError> {
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Array(values) => values
            .iter()
            .map(render)
            .collect::<Result<Vec<_>, _>>()
            .map(|v| v.join("\n\n")),
        Value::Object(value) => match (
            value.get("language").and_then(Value::as_str),
            value.get("value").and_then(Value::as_str),
        ) {
            (Some(language), Some(text)) => Ok(format!("```{language}\n{text}\n```")),
            (_, Some(text)) => Ok(text.into()),
            _ => Err(malformed("LSP hover contents are malformed")),
        },
        _ => Err(malformed("LSP hover contents are malformed")),
    }
}
fn normalize(
    operation: LspOperation,
    payload: Value,
    workspace: &str,
) -> Result<LspQueryResult, LspError> {
    if matches!(operation, LspOperation::Hover) {
        if payload.is_null() {
            return Ok(LspQueryResult::Hover { hover: None });
        }
        if !payload.is_object() {
            return Err(malformed("LSP hover is malformed"));
        }
        let contents = render(payload.get("contents").unwrap_or(&Value::Null))?;
        if contents.is_empty() {
            return Ok(LspQueryResult::Hover { hover: None });
        }
        let mut hover = json!({"contents":contents});
        if let Some(raw) = payload.get("range") {
            range(raw).ok_or_else(|| malformed("LSP hover range is malformed"))?;
            hover["range"] = raw.clone();
        }
        return Ok(LspQueryResult::Hover { hover: Some(hover) });
    }
    let values = match payload {
        Value::Null => vec![],
        Value::Array(values) => values,
        other => vec![other],
    };
    let locations = values
        .into_iter()
        .map(|value| {
            let uri = value
                .get("uri")
                .and_then(Value::as_str)
                .or_else(|| value.get("targetUri").and_then(Value::as_str))
                .filter(|v| !v.is_empty())
                .ok_or_else(|| malformed("LSP location is malformed"))?;
            let range = range(
                value
                    .get("range")
                    .filter(|v| !v.is_null())
                    .or_else(|| value.get("targetSelectionRange"))
                    .unwrap_or(&Value::Null),
            )
            .ok_or_else(|| malformed("LSP location is malformed"))?;
            Ok(LspLocation {
                uri: uri.into(),
                range,
            })
        })
        .collect::<Result<Vec<_>, LspError>>()?;
    Ok(LspQueryResult::Locations {
        locations,
        resolved_workspace_uri: workspace.into(),
    })
}
fn remap_locations(
    result: LspQueryResult,
    workspace: &Path,
    server_workspace_uri: &str,
) -> Result<LspQueryResult, LspError> {
    match result {
        LspQueryResult::Locations { mut locations, .. } => {
            let server_workspace = url::Url::parse(server_workspace_uri)
                .ok()
                .and_then(|u| u.to_file_path().ok())
                .ok_or_else(|| malformed("LSP returned a location outside its session"))?;
            for location in &mut locations {
                let file = url::Url::parse(&location.uri)
                    .ok()
                    .and_then(|u| u.to_file_path().ok())
                    .ok_or_else(|| malformed("LSP returned a location outside its session"))?;
                let relative = file
                    .strip_prefix(&server_workspace)
                    .map_err(|_| malformed("LSP returned a location outside its session"))?;
                location.uri = uri(&workspace.join(relative))?;
            }
            Ok(LspQueryResult::Locations {
                locations,
                resolved_workspace_uri: uri(workspace)?,
            })
        }
        hover => Ok(hover),
    }
}

#[cfg(test)]
#[path = "stdio_tests.rs"]
mod tests;
