use crate::runtime::{network::Cancellation, tools::sandbox::Sandbox};
use std::{collections::BTreeMap, future::Future, path::PathBuf, pin::Pin, sync::Arc};
pub type LspFuture<T> = Pin<Box<dyn Future<Output = Result<T, LspError>> + Send + 'static>>;
#[derive(Clone, Debug, PartialEq)]
pub struct LspError {
    pub code: String,
    pub message: String,
}
impl LspError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}
impl std::fmt::Display for LspError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for LspError {}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LspOperation {
    Definition,
    References,
    Implementation,
    Hover,
}
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LspPosition {
    #[serde(serialize_with = "serialize_coordinate")]
    pub line: f64,
    #[serde(serialize_with = "serialize_coordinate")]
    pub character: f64,
}
fn serialize_coordinate<S: serde::Serializer>(
    value: &f64,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    // Preserve JSON number semantics without narrowing a JS Number to u32.
    // Ordinary exact unsigned integers retain their integer wire spelling.
    if value.is_finite()
        && *value >= 0.0
        && *value < 18_446_744_073_709_551_616.0
        && value.fract() == 0.0
    {
        serializer.serialize_u64(*value as u64)
    } else {
        serializer.serialize_f64(*value)
    }
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LspRange {
    pub start: LspPosition,
    pub end: LspPosition,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LspLocation {
    pub uri: String,
    pub range: LspRange,
}
#[derive(Clone, Debug, PartialEq)]
pub struct LspQueryRequest {
    pub operation: LspOperation,
    pub file_path: String,
    pub position: LspPosition,
    pub workspace_root: PathBuf,
}
#[derive(Clone, Debug, PartialEq)]
pub struct LspProviderQuery {
    pub request: LspQueryRequest,
    pub language_id: String,
}
#[derive(Clone, Debug, PartialEq)]
pub enum LspQueryResult {
    Locations {
        locations: Vec<LspLocation>,
        resolved_workspace_uri: String,
    },
    Hover {
        hover: Option<serde_json::Value>,
    },
}
#[derive(Clone, Debug)]
pub struct LspFileStat {
    pub is_file: bool,
    pub is_directory: bool,
    pub size: u64,
}
#[derive(Clone, Debug)]
pub struct LspCommand {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub workspace_uri: Option<String>,
    pub source_uri: Option<String>,
}
pub trait LspExecutionPort: Send + Sync {
    fn stat(&self, path: PathBuf, cancellation: Cancellation) -> LspFuture<LspFileStat>;
    fn realpath(&self, path: PathBuf, cancellation: Cancellation) -> LspFuture<PathBuf>;
    fn read_file(&self, path: PathBuf, cancellation: Cancellation) -> LspFuture<Vec<u8>>;
    /// Compatibility default for custom guest ports. This checks returned
    /// bytes, but does not claim an OS-level confinement or allocation bound.
    fn read_file_bounded(
        &self,
        path: PathBuf,
        max_document_bytes: u64,
        cancellation: Cancellation,
    ) -> LspFuture<Vec<u8>> {
        let read = self.read_file(path, cancellation);
        Box::pin(async move {
            let bytes = read.await?;
            if bytes.len() as u64 > max_document_bytes {
                return Err(LspError::new(
                    "LSP_MALFORMED_RESPONSE",
                    format!("LSP source exceeds {max_document_bytes} bytes"),
                ));
            }
            Ok(bytes)
        })
    }
    fn prepare_subprocess(
        &self,
        command: LspCommand,
        cancellation: Cancellation,
    ) -> LspFuture<LspCommand>;
}
#[derive(Clone)]
pub struct LspSessionExecution {
    pub workspace_root: PathBuf,
    pub io: Arc<dyn LspExecutionPort>,
}
pub trait LspProvider: Send + Sync {
    fn id(&self) -> &str;
    fn extension_to_language(&self) -> BTreeMap<String, String>;
    fn query(
        &self,
        request: LspProviderQuery,
        cancellation: Cancellation,
    ) -> LspFuture<LspQueryResult>;
    fn bind_session(
        &self,
        execution: LspSessionExecution,
    ) -> Result<Arc<dyn LspProvider>, LspError>;
    fn dispose(&self) -> LspFuture<()>;
}
/// Generation-scoped source-level supplier. The factory is called once when
/// the project generation stages, never once per model/tool invocation.
pub trait LspServiceFactory: Send + Sync {
    fn create(&self, project_root: &std::path::Path) -> Result<Arc<super::LspService>, LspError>;
    /// Optional explicit session execution binding, equivalent to the fixed
    /// baseline sessionExecutionProvider seam. None keeps generation sharing.
    fn session_execution(
        &self,
        _project_root: &std::path::Path,
        _session_key: &str,
    ) -> Result<Option<LspSessionExecution>, LspError> {
        Ok(None)
    }
}
/// The existing native execution world permits reads and confines process
/// writes through its selected sandbox. Custom embeddings may supply guest fs
/// and URI mappings by implementing LspExecutionPort instead.
pub struct NativeLspExecutionPort {
    pub sandbox: Sandbox,
    pub workspace_root: PathBuf,
    logical_root: PathBuf,
    canonical_root: PathBuf,
    root: Arc<std::fs::File>,
}
fn source_io_error(syscall: &str, path: &std::path::Path, error: std::io::Error) -> LspError {
    let (code, message) =
        crate::node_io::diagnostic_or_unknown(&error, syscall, Some(&path.to_string_lossy()));
    LspError::new(code, message)
}
fn io_error(error: std::io::Error) -> LspError {
    LspError::new("LSP_MALFORMED_RESPONSE", error.to_string())
}
fn open_pinned(
    root: &std::fs::File,
    relative: &std::path::Path,
) -> Result<std::fs::File, LspError> {
    #[cfg(unix)]
    {
        crate::project_files::open_file_at(root, relative).map_err(io_error)
    }
    #[cfg(not(unix))]
    {
        let _ = (root, relative);
        Err(LspError::new(
            "LSP_UNAVAILABLE",
            "Native LSP confined filesystem is unavailable on this platform",
        ))
    }
}
fn aborted() -> LspError {
    LspError::new("ABORT_ERR", "LSP query aborted")
}
impl NativeLspExecutionPort {
    pub fn new(sandbox: Sandbox, workspace_root: PathBuf) -> Result<Self, LspError> {
        #[cfg(unix)]
        {
            let cwd = std::env::current_dir().map_err(io_error)?;
            let workspace_root = crate::metadata::resolve_path(&cwd, &workspace_root);
            let logical_root = workspace_root.clone();
            let canonical_root = workspace_root.canonicalize().map_err(io_error)?;
            let root =
                crate::project_files::open_directory_beneath(&canonical_root, &canonical_root)
                    .map_err(io_error)?;
            Ok(Self {
                sandbox,
                workspace_root,
                logical_root,
                canonical_root,
                root: Arc::new(root),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = (sandbox, workspace_root);
            Err(LspError::new(
                "LSP_UNAVAILABLE",
                "Native LSP confined filesystem is unavailable on this platform",
            ))
        }
    }
    fn read_with_barrier(
        &self,
        path: PathBuf,
        max: u64,
        cancel: Cancellation,
        after_resolve: Option<Box<dyn FnOnce() + Send>>,
    ) -> LspFuture<Vec<u8>> {
        let canonical_root = self.canonical_root.clone();
        let root = self.root.clone();
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(aborted());
            }
            let canonical = tokio::fs::canonicalize(&path).await.map_err(io_error)?;
            let relative = canonical
                .strip_prefix(&canonical_root)
                .map_err(|_| {
                    LspError::new(
                        "LSP_MALFORMED_RESPONSE",
                        format!(
                            "LSP source resolves outside the workspace: {}",
                            path.display()
                        ),
                    )
                })?
                .to_owned();
            if let Some(after) = after_resolve {
                after();
            }
            if cancel.is_cancelled() {
                return Err(aborted());
            }
            let file = open_pinned(&root, &relative)?;
            let stat = file.metadata().map_err(io_error)?;
            if !stat.is_file() {
                return Err(LspError::new(
                    "LSP_MALFORMED_RESPONSE",
                    format!("LSP source is not a file: {}", path.display()),
                ));
            }
            if stat.len() > max {
                return Err(LspError::new(
                    "LSP_MALFORMED_RESPONSE",
                    format!("LSP source exceeds {max} bytes"),
                ));
            }
            use tokio::io::AsyncReadExt;
            let mut reader = tokio::fs::File::from_std(file).take(max.saturating_add(1));
            let mut bytes = Vec::new();
            tokio::select! {
                result=reader.read_to_end(&mut bytes)=>{result.map_err(io_error)?;},
                _=cancel.cancelled()=>return Err(aborted()),
            }
            if bytes.len() as u64 > max {
                return Err(LspError::new(
                    "LSP_MALFORMED_RESPONSE",
                    format!("LSP source exceeds {max} bytes"),
                ));
            }
            Ok(bytes)
        })
    }
}
impl LspExecutionPort for NativeLspExecutionPort {
    fn stat(&self, path: PathBuf, cancellation: Cancellation) -> LspFuture<LspFileStat> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(aborted());
            }
            let s = tokio::fs::metadata(&path)
                .await
                .map_err(|error| source_io_error("stat", &path, error))?;
            Ok(LspFileStat {
                is_file: s.is_file(),
                is_directory: s.is_dir(),
                size: s.len(),
            })
        })
    }
    fn realpath(&self, path: PathBuf, cancellation: Cancellation) -> LspFuture<PathBuf> {
        let normalized = std::env::current_dir()
            .ok()
            .map(|cwd| crate::metadata::resolve_path(&cwd, &path));
        let pinned_root = normalized
            .as_ref()
            .is_some_and(|path| path == &self.logical_root || path == &self.canonical_root)
            .then(|| self.canonical_root.clone());
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(aborted());
            }
            if let Some(root) = pinned_root {
                return Ok(root);
            }
            tokio::fs::canonicalize(&path)
                .await
                .map_err(|error| source_io_error("realpath", &path, error))
        })
    }
    fn read_file(&self, path: PathBuf, cancellation: Cancellation) -> LspFuture<Vec<u8>> {
        self.read_with_barrier(path, u64::MAX, cancellation, None)
    }
    fn read_file_bounded(
        &self,
        path: PathBuf,
        max_document_bytes: u64,
        cancellation: Cancellation,
    ) -> LspFuture<Vec<u8>> {
        self.read_with_barrier(path, max_document_bytes, cancellation, None)
    }
    fn prepare_subprocess(
        &self,
        mut command: LspCommand,
        cancellation: Cancellation,
    ) -> LspFuture<LspCommand> {
        let prepared = self.sandbox.prepare(
            &command.command,
            command.args.clone(),
            &self.workspace_root,
            None,
        );
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(aborted());
            }
            let p = prepared.map_err(|e| LspError::new("SandboxUnavailableError", e))?;
            command.command = p.executable;
            command.args = p.args;
            Ok(command)
        })
    }
}

#[cfg(test)]
#[path = "types_tests.rs"]
mod tests;
