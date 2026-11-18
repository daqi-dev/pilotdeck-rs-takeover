//! Builtin PilotDeck workspace tool adapters registered on [`CatalogToolContract`]:
//! `read_file`, `write_file`, `edit_file`, `glob`, `grep`, and `todo_write`.
//!
//! Enforces:
//! - exact JSON Schema validation before handler invocation,
//! - honest [`ToolEffect`] classifications (`ReadOnly` vs `SideEffect`),
//! - mandatory [`VerifiedToolContext`] + [`VerifiedToolEnvironment`] + pre-auth
//!   [`crate::governance::FinalizedToolCall`] digest verification (TC-01 / TC-04),
//! - trusted workspace boundary binding from caller config / verified environment
//!   (never spoofable from raw tool arguments).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use agentcpm_iface::{
    ERR_INVALID_PARAMS,
    tool::{ToolEffect, ToolResult},
    types::{Error, ErrorCategory},
};
use pilotdeck_protocol::{JsObject, JsValue};
use pilotdeck_tool_core::{
    DEFAULT_MAX_GLOB_RESULTS, DEFAULT_MAX_GREP_FILE_BYTES, DEFAULT_MAX_GREP_LINE_BYTES,
    DEFAULT_MAX_GREP_MATCHES, DEFAULT_MAX_READ_LINE_BYTES, DEFAULT_MAX_READ_OUTPUT_BYTES,
    EDIT_FILE_TOOL_NAME, GLOB_TOOL_NAME, GREP_TOOL_NAME, READ_FILE_TOOL_NAME, TODO_TOOL_NAME,
    TodoItem, ToolCatalogEntry, ToolConcurrencyPolicy, WRITE_FILE_TOOL_NAME, WorkspaceBoundary,
    apply_todo_update, edit_file, edit_file_definition, glob, glob_definition, grep,
    grep_definition, parse_edit_file_request, parse_glob_request, parse_grep_request,
    parse_read_file_request, parse_todo_write_request, parse_write_file_request, read_file,
    read_file_definition, todo_definition, write_file, write_file_definition,
};

use crate::catalog_contract::{CatalogToolContract, CatalogToolFn, VerifiedToolContext};

/// Trusted host-supplied workspace configuration for builtin workspace tools.
///
/// Workspace boundaries are bound exclusively by trusted caller setup and keyed
/// by verified Kernel [`crate::governance::VerifiedToolEnvironment`] provenance —
/// never parsed from model-supplied tool arguments.
#[derive(Clone, Debug)]
pub struct WorkspaceToolConfig {
    default_boundary: Option<WorkspaceBoundary>,
    env_boundaries: BTreeMap<String, WorkspaceBoundary>,
    require_environment_id: bool,
    max_read_output_bytes: usize,
    max_read_line_bytes: usize,
    max_glob_results: usize,
    max_grep_matches: usize,
    max_grep_file_bytes: usize,
    max_grep_line_bytes: usize,
}

impl WorkspaceToolConfig {
    /// Shared constructor applying the product-default bounded tool limits.
    fn with_limits(
        default_boundary: Option<WorkspaceBoundary>,
        env_boundaries: BTreeMap<String, WorkspaceBoundary>,
        require_environment_id: bool,
    ) -> Self {
        Self {
            default_boundary,
            env_boundaries,
            require_environment_id,
            max_read_output_bytes: DEFAULT_MAX_READ_OUTPUT_BYTES,
            max_read_line_bytes: DEFAULT_MAX_READ_LINE_BYTES,
            max_glob_results: DEFAULT_MAX_GLOB_RESULTS,
            max_grep_matches: DEFAULT_MAX_GREP_MATCHES,
            max_grep_file_bytes: DEFAULT_MAX_GREP_FILE_BYTES,
            max_grep_line_bytes: DEFAULT_MAX_GREP_LINE_BYTES,
        }
    }

    /// Bind a single trusted [`WorkspaceBoundary`] supplied by the caller.
    pub fn new(boundary: WorkspaceBoundary) -> Self {
        Self::with_limits(Some(boundary), BTreeMap::new(), false)
    }

    /// Construct a strict environment-pinned config that requires
    /// `ToolCall.environment` to carry `env_id` matching a registered boundary.
    pub fn for_environment(env_id: impl Into<String>, boundary: WorkspaceBoundary) -> Self {
        let mut env_boundaries = BTreeMap::new();
        env_boundaries.insert(env_id.into(), boundary);
        Self::with_limits(None, env_boundaries, true)
    }

    /// Register an environment-specific trusted [`WorkspaceBoundary`].
    #[must_use]
    pub fn with_environment_boundary(
        mut self,
        env_id: impl Into<String>,
        boundary: WorkspaceBoundary,
    ) -> Self {
        self.env_boundaries.insert(env_id.into(), boundary);
        self
    }

    /// Require every invocation to carry a verified non-empty Kernel `environment` id.
    #[must_use]
    pub fn with_require_environment_id(mut self, require: bool) -> Self {
        self.require_environment_id = require;
        self
    }

    /// Override `read_file` byte budget and per-line truncation cap.
    #[must_use]
    pub fn with_read_limits(mut self, max_output_bytes: usize, max_line_bytes: usize) -> Self {
        self.max_read_output_bytes = max_output_bytes;
        self.max_read_line_bytes = max_line_bytes;
        self
    }

    /// Override `glob` result cap.
    #[must_use]
    pub fn with_glob_limit(mut self, max_results: usize) -> Self {
        self.max_glob_results = max_results;
        self
    }

    /// Override `grep` match cap and per-file/per-line bounds.
    #[must_use]
    pub fn with_grep_limits(
        mut self,
        max_matches: usize,
        max_file_bytes: usize,
        max_line_bytes: usize,
    ) -> Self {
        self.max_grep_matches = max_matches;
        self.max_grep_file_bytes = max_file_bytes;
        self.max_grep_line_bytes = max_line_bytes;
        self
    }

    fn verify_context(&self, ctx: &VerifiedToolContext) -> Result<(), Error> {
        ctx.environment.require_kernel_bound()?;
        if self.require_environment_id {
            ctx.environment.require_environment_id()?;
        }
        Ok(())
    }

    fn resolve_boundary(&self, ctx: &VerifiedToolContext) -> Result<&WorkspaceBoundary, Error> {
        self.verify_context(ctx)?;
        match ctx.environment.environment_id() {
            Some(env_id) => {
                if let Some(bound) = self.env_boundaries.get(env_id) {
                    return Ok(bound);
                }
                if self.env_boundaries.is_empty()
                    && let Some(default_bound) = &self.default_boundary
                {
                    return Ok(default_bound);
                }
                Err(Error::new(
                    ErrorCategory::Environment,
                    ERR_INVALID_PARAMS,
                    format!("no trusted workspace boundary bound for environment '{env_id}'"),
                ))
            }
            None => self.default_boundary.as_ref().ok_or_else(|| {
                Error::new(
                    ErrorCategory::Environment,
                    ERR_INVALID_PARAMS,
                    "missing verified kernel environment for workspace tool",
                )
            }),
        }
    }
}

type TodoSessionKey = (Option<String>, String);
type TodoSessionMap = BTreeMap<TodoSessionKey, Vec<TodoItem>>;

/// Thread-safe session-isolated in-memory checklist state backing `todo_write`.
#[derive(Clone, Debug, Default)]
pub struct WorkspaceTodoStore {
    sessions: Arc<Mutex<TodoSessionMap>>,
}

impl WorkspaceTodoStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read current checklist items for `(environment_id, session_id)`.
    pub fn items(&self, environment_id: Option<&str>, session_id: &str) -> Vec<TodoItem> {
        let key = (environment_id.map(str::to_owned), session_id.to_owned());
        self.sessions
            .lock()
            .ok()
            .and_then(|map| map.get(&key).cloned())
            .unwrap_or_default()
    }
}

/// Construct a [`CatalogToolContract`] pre-populated with all 6 builtin workspace tools.
pub fn workspace_tool_contract(
    config: WorkspaceToolConfig,
) -> Result<(CatalogToolContract, WorkspaceTodoStore), Error> {
    let contract = CatalogToolContract::new();
    let store = register_workspace_tools(&contract, config)?;
    Ok((contract, store))
}

/// Atomically register `read_file`, `write_file`, `edit_file`, `glob`, `grep`,
/// and `todo_write` on an existing [`CatalogToolContract`].
pub fn register_workspace_tools(
    contract: &CatalogToolContract,
    config: WorkspaceToolConfig,
) -> Result<WorkspaceTodoStore, Error> {
    let cfg = Arc::new(config);
    let todo_store = WorkspaceTodoStore::new();

    let read_entry = catalog_entry_from_definition(
        read_file_definition(),
        ToolConcurrencyPolicy::ConcurrentReadOnly,
    )?;
    let write_entry = catalog_entry_from_definition(
        write_file_definition(),
        ToolConcurrencyPolicy::SerialSideEffect,
    )?;
    let edit_entry = catalog_entry_from_definition(
        edit_file_definition(),
        ToolConcurrencyPolicy::SerialSideEffect,
    )?;
    let glob_entry = catalog_entry_from_definition(
        glob_definition(),
        ToolConcurrencyPolicy::ConcurrentReadOnly,
    )?;
    let grep_entry = catalog_entry_from_definition(
        grep_definition(),
        ToolConcurrencyPolicy::ConcurrentReadOnly,
    )?;
    let todo_entry =
        catalog_entry_from_definition(todo_definition(), ToolConcurrencyPolicy::SerialSideEffect)?;

    let cfg_read = cfg.clone();
    let read_handler = Arc::new(CatalogToolFn::new(move |ctx: VerifiedToolContext| {
        let cfg = cfg_read.clone();
        Box::pin(async move { execute_read_file(&cfg, &ctx) })
    }));

    let cfg_write = cfg.clone();
    let write_handler = Arc::new(CatalogToolFn::new(move |ctx: VerifiedToolContext| {
        let cfg = cfg_write.clone();
        Box::pin(async move { execute_write_file(&cfg, &ctx) })
    }));

    let cfg_edit = cfg.clone();
    let edit_handler = Arc::new(CatalogToolFn::new(move |ctx: VerifiedToolContext| {
        let cfg = cfg_edit.clone();
        Box::pin(async move { execute_edit_file(&cfg, &ctx) })
    }));

    let cfg_glob = cfg.clone();
    let glob_handler = Arc::new(CatalogToolFn::new(move |ctx: VerifiedToolContext| {
        let cfg = cfg_glob.clone();
        Box::pin(async move { execute_glob(&cfg, &ctx) })
    }));

    let cfg_grep = cfg.clone();
    let grep_handler = Arc::new(CatalogToolFn::new(move |ctx: VerifiedToolContext| {
        let cfg = cfg_grep.clone();
        Box::pin(async move { execute_grep(&cfg, &ctx) })
    }));

    let cfg_todo = cfg;
    let store_todo = todo_store.clone();
    let todo_handler = Arc::new(CatalogToolFn::new(move |ctx: VerifiedToolContext| {
        let cfg = cfg_todo.clone();
        let store = store_todo.clone();
        Box::pin(async move { execute_todo_write(&cfg, &store, &ctx) })
    }));

    contract.register_tools_atomic(vec![
        (read_entry, ToolEffect::ReadOnly, read_handler),
        (write_entry, ToolEffect::SideEffect, write_handler),
        (edit_entry, ToolEffect::SideEffect, edit_handler),
        (glob_entry, ToolEffect::ReadOnly, glob_handler),
        (grep_entry, ToolEffect::ReadOnly, grep_handler),
        (todo_entry, ToolEffect::SideEffect, todo_handler),
    ])?;

    Ok(todo_store)
}

/// Return pure catalog metadata entries for all 6 workspace tools
/// (for [`pilotdeck_tool_core::partition_tool_batch`]).
pub fn workspace_catalog_entries() -> Result<Vec<ToolCatalogEntry>, Error> {
    Ok(vec![
        catalog_entry_from_definition(
            read_file_definition(),
            ToolConcurrencyPolicy::ConcurrentReadOnly,
        )?,
        catalog_entry_from_definition(
            write_file_definition(),
            ToolConcurrencyPolicy::SerialSideEffect,
        )?,
        catalog_entry_from_definition(
            edit_file_definition(),
            ToolConcurrencyPolicy::SerialSideEffect,
        )?,
        catalog_entry_from_definition(
            glob_definition(),
            ToolConcurrencyPolicy::ConcurrentReadOnly,
        )?,
        catalog_entry_from_definition(
            grep_definition(),
            ToolConcurrencyPolicy::ConcurrentReadOnly,
        )?,
        catalog_entry_from_definition(todo_definition(), ToolConcurrencyPolicy::SerialSideEffect)?,
    ])
}

/// Canonical tool names registered by [`register_workspace_tools`].
pub const WORKSPACE_TOOL_NAMES: [&str; 6] = [
    READ_FILE_TOOL_NAME,
    WRITE_FILE_TOOL_NAME,
    EDIT_FILE_TOOL_NAME,
    GLOB_TOOL_NAME,
    GREP_TOOL_NAME,
    TODO_TOOL_NAME,
];

fn catalog_entry_from_definition(
    def: JsObject,
    concurrency: ToolConcurrencyPolicy,
) -> Result<ToolCatalogEntry, Error> {
    let field_utf8 = |key: &str| -> Result<String, Error> {
        def.get(key)
            .and_then(JsValue::as_string)
            .and_then(|s| s.to_utf8().ok())
            .ok_or_else(|| {
                Error::new(
                    ErrorCategory::Internal,
                    -32000,
                    format!("builtin tool definition missing UTF-8 string field '{key}'"),
                )
            })
    };
    let name = field_utf8("name")?;
    let description = field_utf8("description")?;
    let kind = field_utf8("kind")?;
    let aliases = match def.get("aliases") {
        Some(JsValue::Array(items)) => items
            .iter()
            .filter_map(JsValue::as_string)
            .filter_map(|s| s.to_utf8().ok())
            .collect(),
        _ => Vec::new(),
    };
    let input_schema = def.get("inputSchema").cloned().ok_or_else(|| {
        Error::new(
            ErrorCategory::Internal,
            -32000,
            format!("builtin tool '{name}' definition missing inputSchema"),
        )
    })?;
    Ok(ToolCatalogEntry {
        name,
        aliases,
        description,
        kind,
        input_schema,
        concurrency,
    })
}

fn execute_read_file(
    cfg: &WorkspaceToolConfig,
    ctx: &VerifiedToolContext,
) -> Result<ToolResult, Error> {
    let boundary = cfg.resolve_boundary(ctx)?;
    let mut req = match parse_read_file_request(&ctx.finalized_args) {
        Ok(req) => req,
        Err(err) => return product_failure(&err.code, &err.message),
    };
    req.max_output_bytes = cfg.max_read_output_bytes;
    req.max_line_bytes = cfg.max_read_line_bytes;
    match read_file(boundary, &req) {
        Ok(res) => product_success(&res.to_js_value()),
        Err(err) => product_failure(&err.code, &err.message),
    }
}

fn execute_write_file(
    cfg: &WorkspaceToolConfig,
    ctx: &VerifiedToolContext,
) -> Result<ToolResult, Error> {
    let boundary = cfg.resolve_boundary(ctx)?;
    let req = match parse_write_file_request(&ctx.finalized_args) {
        Ok(req) => req,
        Err(err) => return product_failure(&err.code, &err.message),
    };
    match write_file(boundary, &req) {
        Ok(res) => product_success(&res.to_js_value()),
        Err(err) => product_failure(&err.code, &err.message),
    }
}

fn execute_edit_file(
    cfg: &WorkspaceToolConfig,
    ctx: &VerifiedToolContext,
) -> Result<ToolResult, Error> {
    let boundary = cfg.resolve_boundary(ctx)?;
    let req = match parse_edit_file_request(&ctx.finalized_args) {
        Ok(req) => req,
        Err(err) => return product_failure(&err.code, &err.message),
    };
    match edit_file(boundary, &req) {
        Ok(res) => product_success(&res.to_js_value()),
        Err(err) => product_failure(&err.code, &err.message),
    }
}

fn execute_glob(cfg: &WorkspaceToolConfig, ctx: &VerifiedToolContext) -> Result<ToolResult, Error> {
    let boundary = cfg.resolve_boundary(ctx)?;
    let mut req = match parse_glob_request(&ctx.finalized_args) {
        Ok(req) => req,
        Err(err) => return product_failure(&err.code, &err.message),
    };
    req.limit = req.limit.min(cfg.max_glob_results);
    match glob(boundary, &req) {
        Ok(res) => product_success(&res.to_js_value()),
        Err(err) => product_failure(&err.code, &err.message),
    }
}

fn execute_grep(cfg: &WorkspaceToolConfig, ctx: &VerifiedToolContext) -> Result<ToolResult, Error> {
    let boundary = cfg.resolve_boundary(ctx)?;
    let mut req = match parse_grep_request(&ctx.finalized_args) {
        Ok(req) => req,
        Err(err) => return product_failure(&err.code, &err.message),
    };
    req.max_matches = req.max_matches.min(cfg.max_grep_matches);
    req.max_file_bytes = cfg.max_grep_file_bytes;
    req.max_line_bytes = cfg.max_grep_line_bytes;
    match grep(boundary, &req) {
        Ok(res) => product_success(&res.to_js_value()),
        Err(err) => product_failure(&err.code, &err.message),
    }
}

fn execute_todo_write(
    cfg: &WorkspaceToolConfig,
    store: &WorkspaceTodoStore,
    ctx: &VerifiedToolContext,
) -> Result<ToolResult, Error> {
    cfg.verify_context(ctx)?;
    let req = match parse_todo_write_request(&ctx.finalized_args) {
        Ok(req) => req,
        Err(message) => return product_failure("invalid_input", &message),
    };

    let key = (
        ctx.environment.environment_id.clone(),
        ctx.meta.session_id.clone(),
    );
    let mut guard = store
        .sessions
        .lock()
        .map_err(|_| Error::new(ErrorCategory::Internal, -32000, "todo store lock poisoned"))?;
    let existing = guard.get(&key).map(Vec::as_slice).unwrap_or(&[]);
    let updated = match apply_todo_update(existing, &req) {
        Ok(items) => items,
        Err(message) => return product_failure("invalid_input", &message),
    };
    if req.todos.is_some() || req.markdown.is_some() {
        guard.insert(key, updated.clone());
    }
    drop(guard);

    let todo_values = updated
        .iter()
        .map(|item| {
            let mut obj = JsObject::new();
            obj.insert("id".into(), item.id.as_str().into());
            obj.insert("content".into(), item.content.as_str().into());
            obj.insert("status".into(), item.status.as_str().into());
            if let Some(priority) = &item.priority {
                obj.insert("priority".into(), priority.as_str().into());
            }
            JsValue::Object(obj)
        })
        .collect();

    let mut out = JsObject::new();
    out.insert("todos".into(), JsValue::Array(todo_values));
    out.insert("count".into(), JsValue::Number(updated.len() as f64));
    if let Some(reason) = &req.reason {
        out.insert("reason".into(), reason.as_str().into());
    }
    product_success(&JsValue::Object(out))
}

fn product_success(value: &JsValue) -> Result<ToolResult, Error> {
    let bytes = crate::support::json_bytes(value, "workspace tool JSON encoding failed")?;
    Ok(ToolResult {
        ok: true,
        bytes,
        unknown: false,
    })
}

fn product_failure(code: &str, message: &str) -> Result<ToolResult, Error> {
    let mut obj = JsObject::new();
    obj.insert("code".into(), code.into());
    obj.insert("message".into(), message.into());
    let bytes = crate::support::json_bytes(
        &JsValue::Object(obj),
        "workspace tool error JSON encoding failed",
    )?;
    Ok(ToolResult {
        ok: false,
        bytes,
        unknown: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::governance::{FinalizedToolCall, PreAuthArgumentFinalizer, VerifiedToolEnvironment};
    use agentcpm_iface::{
        BoxFuture,
        tool::{ToolCall, ToolContract, ToolStaging},
        types::CallMeta,
    };
    use pilotdeck_protocol::parse_json;
    use pilotdeck_tool_core::{ToolCallCandidate, partition_tool_batch};
    use std::fs;
    use std::future::Future;
    use std::path::{Path, PathBuf};
    use std::pin::pin;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::task::{Context, Poll, Waker};

    struct TempWorkspace {
        dir: PathBuf,
    }

    impl TempWorkspace {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let base = std::env::temp_dir().join(format!(
                "pilotdeck-tool-iface-ws-{}-{}-{}",
                tag,
                std::process::id(),
                n
            ));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            Self { dir: base }
        }

        fn path(&self) -> &Path {
            &self.dir
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn ready<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let mut cx = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("workspace tool future must complete synchronously"),
        }
    }

    fn call(tool: &str, env: Option<&str>, args: &[u8]) -> ToolCall {
        ToolCall::new(
            CallMeta {
                session_id: "sess-ws".into(),
                call_id: "call-ws".into(),
                turn: 1,
            },
            tool.into(),
            env.map(str::to_owned),
            args.to_vec(),
        )
    }

    fn parse_result(res: &ToolResult) -> JsValue {
        parse_json(std::str::from_utf8(&res.bytes).unwrap()).unwrap()
    }

    #[test]
    fn registers_all_six_tools_with_honest_effects_and_staging_compatibility() {
        let ws = TempWorkspace::new("effects");
        let boundary = WorkspaceBoundary::new(ws.path()).unwrap();
        let (contract, _) = workspace_tool_contract(WorkspaceToolConfig::new(boundary)).unwrap();

        for ro in ["read_file", "Read", "glob", "Glob", "grep", "Grep"] {
            assert_eq!(contract.effect(ro), ToolEffect::ReadOnly, "{ro}");
        }
        for rw in [
            "write_file",
            "Write",
            "edit_file",
            "Edit",
            "todo_write",
            "TodoWrite",
        ] {
            assert_eq!(contract.effect(rw), ToolEffect::SideEffect, "{rw}");
        }
        assert_eq!(contract.effect("unknown_tool"), ToolEffect::Unknown);

        let listed = ready(contract.list());
        assert_eq!(listed.len(), 6);
        for name in WORKSPACE_TOOL_NAMES {
            assert!(contract.contains(name));
            assert!(contract.remove_stub(name).is_err());
        }

        // ToolStaging still works cleanly alongside registered workspace tools.
        contract.register_stub("staged.extra").unwrap();
        assert_eq!(contract.effect("staged.extra"), ToolEffect::Unknown);
        contract.remove_stub("staged.extra").unwrap();

        // Batch partitioning groups ReadOnly tools and isolates SideEffect tools.
        let slices = partition_tool_batch(
            &contract.catalog_entries(),
            &[
                ToolCallCandidate {
                    call_id: "1".into(),
                    tool_name: "read_file".into(),
                    read_only_call: true,
                },
                ToolCallCandidate {
                    call_id: "2".into(),
                    tool_name: "glob".into(),
                    read_only_call: true,
                },
                ToolCallCandidate {
                    call_id: "3".into(),
                    tool_name: "grep".into(),
                    read_only_call: true,
                },
                ToolCallCandidate {
                    call_id: "4".into(),
                    tool_name: "write_file".into(),
                    read_only_call: true,
                },
                ToolCallCandidate {
                    call_id: "5".into(),
                    tool_name: "edit_file".into(),
                    read_only_call: false,
                },
                ToolCallCandidate {
                    call_id: "6".into(),
                    tool_name: "todo_write".into(),
                    read_only_call: true,
                },
            ],
        );
        assert_eq!(slices.len(), 4);
        assert!(slices[0].concurrent);
        assert_eq!(slices[0].call_ids, vec!["1", "2", "3"]);
        assert!(!slices[1].concurrent);
        assert!(!slices[2].concurrent);
        assert!(!slices[3].concurrent);
    }

    #[test]
    fn enforces_schema_anti_spoofing_and_environment_binding_before_execution() {
        let ws = TempWorkspace::new("gov");
        let outside = TempWorkspace::new("gov-outside");
        fs::write(ws.path().join("ok.txt"), b"inside\n").unwrap();
        fs::write(outside.path().join("secret.txt"), b"outside\n").unwrap();

        let boundary = WorkspaceBoundary::new(ws.path()).unwrap();
        let (contract, _) =
            workspace_tool_contract(WorkspaceToolConfig::for_environment("sandbox-1", boundary))
                .unwrap();

        // Missing environment fails closed with ErrorCategory::Environment.
        let missing_env =
            ready(contract.invoke(call("read_file", None, br#"{"file_path":"ok.txt"}"#)))
                .unwrap_err();
        assert_eq!(missing_env.category, ErrorCategory::Environment);

        // Wrong environment id fails closed with ErrorCategory::Environment.
        let wrong_env = ready(contract.invoke(call(
            "read_file",
            Some("other-box"),
            br#"{"file_path":"ok.txt"}"#,
        )))
        .unwrap_err();
        assert_eq!(wrong_env.category, ErrorCategory::Environment);

        // Spoofing workspace_root or environment in args is blocked with ErrorCategory::Policy.
        let spoof_root_payload = format!(
            r#"{{"file_path":"secret.txt","workspace_root":"{}"}}"#,
            outside.path().display()
        );
        let spoof_err = ready(contract.invoke(call(
            "read_file",
            Some("sandbox-1"),
            spoof_root_payload.as_bytes(),
        )))
        .unwrap_err();
        assert_eq!(spoof_err.category, ErrorCategory::Policy);

        let spoof_camel = ready(contract.invoke(call(
            "glob",
            Some("sandbox-1"),
            br#"{"pattern":"*","workspaceRoot":"/etc"}"#,
        )))
        .unwrap_err();
        assert_eq!(spoof_camel.category, ErrorCategory::Policy);

        // Invalid schema inputs fail with ok=false and code="invalid_input".
        let bad_offset = ready(contract.invoke(call(
            "read_file",
            Some("sandbox-1"),
            br#"{"file_path":"ok.txt","offset":0}"#,
        )))
        .unwrap();
        assert!(!bad_offset.ok);
        assert_eq!(
            parse_result(&bad_offset).get("code"),
            Some(&JsValue::from("invalid_input"))
        );

        let unknown_prop = ready(contract.invoke(call(
            "write_file",
            Some("sandbox-1"),
            br#"{"file_path":"a.txt","content":"x","extra":true}"#,
        )))
        .unwrap();
        assert!(!unknown_prop.ok);
        assert_eq!(
            parse_result(&unknown_prop).get("code"),
            Some(&JsValue::from("invalid_input"))
        );

        // Valid bound call succeeds.
        let ok = ready(contract.invoke(call(
            "read_file",
            Some("sandbox-1"),
            br#"{"file_path":"ok.txt"}"#,
        )))
        .unwrap();
        assert!(ok.ok);
        assert_eq!(
            parse_result(&ok).get("content"),
            Some(&JsValue::from("1|inside"))
        );
    }

    #[test]
    fn rejects_tampered_pre_auth_digest_and_finalized_payload_before_workspace_mutation() {
        let ws = TempWorkspace::new("tc04");
        let boundary = WorkspaceBoundary::new(ws.path()).unwrap();

        struct TamperingFinalizer(Arc<AtomicUsize>);
        impl PreAuthArgumentFinalizer for TamperingFinalizer {
            fn finalize_before_auth(
                &self,
                _tool: &str,
                _env: &VerifiedToolEnvironment,
                raw_args: &[u8],
            ) -> BoxFuture<'_, Result<FinalizedToolCall, Error>> {
                self.0.fetch_add(1, Ordering::SeqCst);
                let mut finalized = FinalizedToolCall::new(raw_args, raw_args.to_vec());
                // Simulate post-finalization tampering of args_bytes without updating sha256_hex.
                finalized.args_bytes = br#"{"file_path":"pwned.txt","content":"evil"}"#.to_vec();
                Box::pin(async move { Ok(finalized) })
            }
        }

        let hits = Arc::new(AtomicUsize::new(0));
        let contract =
            CatalogToolContract::new().with_finalizer(Arc::new(TamperingFinalizer(hits.clone())));
        register_workspace_tools(&contract, WorkspaceToolConfig::new(boundary.clone())).unwrap();

        let err = ready(contract.invoke(call(
            "write_file",
            Some("local"),
            br#"{"file_path":"safe.txt","content":"ok"}"#,
        )))
        .unwrap_err();
        assert_eq!(err.category, ErrorCategory::Policy);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(!ws.path().join("pwned.txt").exists());
        assert!(!ws.path().join("safe.txt").exists());

        // invoke_finalized also rejects post-auth tampered FinalizedToolCall or mismatched call.args.
        let clean_contract = workspace_tool_contract(WorkspaceToolConfig::new(boundary))
            .unwrap()
            .0;
        let raw = br#"{"file_path":"auth.txt","content":"hello"}"#;
        let mut locked = FinalizedToolCall::new(raw, raw.to_vec());
        locked.args_bytes = br#"{"file_path":"tampered.txt","content":"bad"}"#.to_vec();
        let err =
            ready(clean_contract.invoke_finalized(call("write_file", Some("local"), raw), locked))
                .unwrap_err();
        assert_eq!(err.category, ErrorCategory::Policy);
        assert!(!ws.path().join("tampered.txt").exists());

        let valid_locked = FinalizedToolCall::new(raw, raw.to_vec());
        let diverged_err = ready(clean_contract.invoke_finalized(
            call(
                "write_file",
                Some("local"),
                br#"{"file_path":"diverged.txt","content":"bad"}"#,
            ),
            valid_locked,
        ))
        .unwrap_err();
        assert_eq!(diverged_err.category, ErrorCategory::Policy);
        assert!(!ws.path().join("diverged.txt").exists());
    }

    #[test]
    fn end_to_end_workspace_file_search_and_todo_roundtrip() {
        let ws = TempWorkspace::new("e2e");
        let boundary = WorkspaceBoundary::new(ws.path()).unwrap();
        let (contract, todo_store) =
            workspace_tool_contract(WorkspaceToolConfig::new(boundary)).unwrap();

        // Write a file via alias.
        let w = ready(contract.invoke(call(
            "Write",
            None,
            br#"{"file_path":"src/main.rs","content":"fn main() {\n    println!(\"hi\");\n}\n"}"#,
        )))
        .unwrap();
        assert!(w.ok);
        let w_json = parse_result(&w);
        let sha1 =
            String::from_utf16_lossy(w_json.get("sha256").unwrap().as_string().unwrap().units());

        // Read back with sha256.
        let r =
            ready(contract.invoke(call("Read", None, br#"{"file_path":"src/main.rs"}"#))).unwrap();
        assert!(r.ok);
        let r_json = parse_result(&r);
        assert_eq!(r_json.get("sha256"), Some(&JsValue::from(sha1.as_str())));

        // Edit with CAS lock.
        let edit_args = format!(
            r#"{{"file_path":"src/main.rs","old_string":"hi","new_string":"hello","expected_sha256":"{sha1}"}}"#
        );
        let e = ready(contract.invoke(call("Edit", None, edit_args.as_bytes()))).unwrap();
        assert!(e.ok);
        assert_eq!(
            parse_result(&e).get("replacements"),
            Some(&JsValue::Number(1.0))
        );

        // Glob & Grep find the edited file.
        let gl = ready(contract.invoke(call("Glob", None, br#"{"pattern":"**/*.rs"}"#))).unwrap();
        assert!(gl.ok);
        assert_eq!(
            parse_result(&gl).get("files"),
            Some(&JsValue::Array(vec![JsValue::from("src/main.rs")]))
        );

        let gr = ready(contract.invoke(call(
            "Grep",
            None,
            br#"{"pattern":"HELLO","case_insensitive":true}"#,
        )))
        .unwrap();
        assert!(gr.ok);
        assert_eq!(parse_result(&gr).get("count"), Some(&JsValue::Number(1.0)));

        // TodoWrite persists per-session and merges cleanly.
        let t1 = ready(contract.invoke(call(
            "TodoWrite",
            Some("env-a"),
            br#"{"todos":[{"id":"t1","content":"check","status":"in_progress"}]}"#,
        )))
        .unwrap();
        assert!(t1.ok);
        assert_eq!(todo_store.items(Some("env-a"), "sess-ws").len(), 1);
        assert!(todo_store.items(Some("env-b"), "sess-ws").is_empty());

        let t2 = ready(contract.invoke(call(
            "todo_write",
            Some("env-a"),
            br#"{"todos":[{"id":"t1","status":"completed"}],"merge":true}"#,
        )))
        .unwrap();
        assert!(t2.ok);
        assert_eq!(
            todo_store.items(Some("env-a"), "sess-ws")[0].status,
            "completed"
        );
    }

    #[test]
    fn atomic_registration_fails_cleanly_without_partial_install() {
        let ws = TempWorkspace::new("atomic-reg");
        let boundary = WorkspaceBoundary::new(ws.path()).unwrap();
        let contract = CatalogToolContract::new();
        contract.register_stub("grep").unwrap();

        let err =
            register_workspace_tools(&contract, WorkspaceToolConfig::new(boundary)).unwrap_err();
        assert_eq!(err.code, agentcpm_iface::types::ERR_APP);
        assert!(!contract.contains("read_file"));
        assert!(!contract.contains("write_file"));
        assert!(contract.contains("grep"));
    }

    #[test]
    fn rejects_boundary_escapes_and_binary_files_through_catalog_contract() {
        let ws = TempWorkspace::new("escape-iface");
        let outside = TempWorkspace::new("escape-outside");
        fs::write(outside.path().join("secret.txt"), b"outside").unwrap();
        fs::write(ws.path().join("bin.dat"), b"abc\x00def").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), ws.path().join("link_dir")).unwrap();
            std::os::unix::fs::symlink(
                outside.path().join("secret.txt"),
                ws.path().join("link_file.txt"),
            )
            .unwrap();
        }

        let boundary = WorkspaceBoundary::new(ws.path()).unwrap();
        let (contract, _) = workspace_tool_contract(WorkspaceToolConfig::new(boundary)).unwrap();

        let escape_read = ready(contract.invoke(call(
            "read_file",
            None,
            br#"{"file_path":"../escape-outside/secret.txt"}"#,
        )))
        .unwrap();
        assert!(!escape_read.ok);
        assert_eq!(
            parse_result(&escape_read).get("code"),
            Some(&JsValue::from("path_not_allowed"))
        );

        let escape_write = ready(contract.invoke(call(
            "write_file",
            None,
            br#"{"file_path":"../pwn.txt","content":"bad"}"#,
        )))
        .unwrap();
        assert!(!escape_write.ok);
        assert_eq!(
            parse_result(&escape_write).get("code"),
            Some(&JsValue::from("path_not_allowed"))
        );

        let bin_read =
            ready(contract.invoke(call("read_file", None, br#"{"file_path":"bin.dat"}"#))).unwrap();
        assert!(!bin_read.ok);
        assert_eq!(
            parse_result(&bin_read).get("code"),
            Some(&JsValue::from("binary_file"))
        );

        #[cfg(unix)]
        {
            let sym_write = ready(contract.invoke(call(
                "write_file",
                None,
                br#"{"file_path":"link_dir/new_dir/pwn.txt","content":"bad"}"#,
            )))
            .unwrap();
            assert!(!sym_write.ok);
            assert_eq!(
                parse_result(&sym_write).get("code"),
                Some(&JsValue::from("symlink_escape"))
            );
            assert!(!outside.path().join("new_dir/pwn.txt").exists());

            let sym_grep = ready(contract.invoke(call(
                "grep",
                None,
                br#"{"pattern":"outside","path":"link_file.txt"}"#,
            )))
            .unwrap();
            assert!(!sym_grep.ok);
            assert_eq!(
                parse_result(&sym_grep).get("code"),
                Some(&JsValue::from("symlink_escape"))
            );
        }
    }

    #[test]
    fn enforces_cas_mismatch_and_unique_vs_replace_all_edit_through_contract() {
        let ws = TempWorkspace::new("cas-edit-iface");
        let boundary = WorkspaceBoundary::new(ws.path()).unwrap();
        let (contract, _) = workspace_tool_contract(WorkspaceToolConfig::new(boundary)).unwrap();

        let w = ready(contract.invoke(call(
            "write_file",
            None,
            br#"{"file_path":"app.txt","content":"dup\ndup\n"}"#,
        )))
        .unwrap();
        assert!(w.ok);
        let sha = String::from_utf16_lossy(
            parse_result(&w)
                .get("sha256")
                .unwrap()
                .as_string()
                .unwrap()
                .units(),
        );

        let bad_write_cas = ready(
            contract.invoke(call(
                "write_file",
                None,
                format!(
                    r#"{{"file_path":"app.txt","content":"overwrite","expected_sha256":"{}"}}"#,
                    "0".repeat(64)
                )
                .as_bytes(),
            )),
        )
        .unwrap();
        assert!(!bad_write_cas.ok);
        assert_eq!(
            parse_result(&bad_write_cas).get("code"),
            Some(&JsValue::from("cas_mismatch"))
        );

        let ambiguous = ready(contract.invoke(call(
            "edit_file",
            None,
            format!(
                r#"{{"file_path":"app.txt","old_string":"dup","new_string":"one","expected_sha256":"{sha}"}}"#
            )
            .as_bytes(),
        )))
        .unwrap();
        assert!(!ambiguous.ok);
        assert_eq!(
            parse_result(&ambiguous).get("code"),
            Some(&JsValue::from("ambiguous_match"))
        );

        let replace_all = ready(contract.invoke(call(
            "edit_file",
            None,
            format!(
                r#"{{"file_path":"app.txt","old_string":"dup","new_string":"one","replace_all":true,"expected_sha256":"{sha}"}}"#
            )
            .as_bytes(),
        )))
        .unwrap();
        assert!(replace_all.ok);
        assert_eq!(
            parse_result(&replace_all).get("replacements"),
            Some(&JsValue::Number(2.0))
        );
        assert_eq!(
            fs::read_to_string(ws.path().join("app.txt")).unwrap(),
            "one\none\n"
        );
    }

    #[test]
    fn isolates_multiple_environments_and_preserves_todo_on_invalid_input() {
        let ws_a = TempWorkspace::new("env-a");
        let ws_b = TempWorkspace::new("env-b");
        fs::write(ws_a.path().join("env.txt"), b"from-a\n").unwrap();
        fs::write(ws_b.path().join("env.txt"), b"from-b\n").unwrap();

        let cfg = WorkspaceToolConfig::for_environment(
            "env-a",
            WorkspaceBoundary::new(ws_a.path()).unwrap(),
        )
        .with_environment_boundary("env-b", WorkspaceBoundary::new(ws_b.path()).unwrap());

        let (contract, store) = workspace_tool_contract(cfg).unwrap();

        let read_a = ready(contract.invoke(call(
            "read_file",
            Some("env-a"),
            br#"{"file_path":"env.txt"}"#,
        )))
        .unwrap();
        assert_eq!(
            parse_result(&read_a).get("content"),
            Some(&JsValue::from("1|from-a"))
        );

        let read_b = ready(contract.invoke(call(
            "read_file",
            Some("env-b"),
            br#"{"file_path":"env.txt"}"#,
        )))
        .unwrap();
        assert_eq!(
            parse_result(&read_b).get("content"),
            Some(&JsValue::from("1|from-b"))
        );

        // Seed todo, then fail invalid status without mutating existing checklist.
        let ok_todo = ready(contract.invoke(call(
            "todo_write",
            Some("env-a"),
            br#"{"markdown":"- [ ] step one\n- [x] step zero"}"#,
        )))
        .unwrap();
        assert!(ok_todo.ok);
        assert_eq!(store.items(Some("env-a"), "sess-ws").len(), 2);

        let bad_todo = ready(contract.invoke(call(
            "todo_write",
            Some("env-a"),
            br#"{"todos":[{"id":"1","content":"bad","status":"not_valid"}]}"#,
        )))
        .unwrap();
        assert!(!bad_todo.ok);
        assert_eq!(
            parse_result(&bad_todo).get("code"),
            Some(&JsValue::from("invalid_input"))
        );
        assert_eq!(store.items(Some("env-a"), "sess-ws").len(), 2);
    }

    #[test]
    fn honors_pre_auth_finalizer_and_read_glob_grep_config_caps() {
        let ws = TempWorkspace::new("finalizer-caps");
        fs::write(
            ws.path().join("a.txt"),
            b"hello world\nsecond line\nthird line\n",
        )
        .unwrap();
        fs::write(ws.path().join("b.txt"), b"hello again\n").unwrap();

        struct PathRewriteFinalizer;
        impl PreAuthArgumentFinalizer for PathRewriteFinalizer {
            fn finalize_before_auth(
                &self,
                tool: &str,
                _env: &VerifiedToolEnvironment,
                raw_args: &[u8],
            ) -> BoxFuture<'_, Result<FinalizedToolCall, Error>> {
                let next = if tool == "read_file" && raw_args == br#"{"file_path":"alias.txt"}"# {
                    br#"{"file_path":"a.txt"}"#.to_vec()
                } else {
                    raw_args.to_vec()
                };
                let finalized = FinalizedToolCall::new(raw_args, next);
                Box::pin(async move { Ok(finalized) })
            }
        }

        let cfg = WorkspaceToolConfig::new(WorkspaceBoundary::new(ws.path()).unwrap())
            .with_read_limits(18, 12)
            .with_glob_limit(1)
            .with_grep_limits(1, 1024, 64);

        let contract = CatalogToolContract::new().with_finalizer(Arc::new(PathRewriteFinalizer));
        register_workspace_tools(&contract, cfg).unwrap();

        let read_res =
            ready(contract.invoke(call("read_file", None, br#"{"file_path":"alias.txt"}"#)))
                .unwrap();
        assert!(read_res.ok);
        let rj = parse_result(&read_res);
        assert_eq!(rj.get("truncated"), Some(&JsValue::Bool(true)));
        let content =
            String::from_utf16_lossy(rj.get("content").unwrap().as_string().unwrap().units());
        assert!(content.len() <= 18);

        let glob_res =
            ready(contract.invoke(call("glob", None, br#"{"pattern":"*.txt"}"#))).unwrap();
        let glj = parse_result(&glob_res);
        assert_eq!(glj.get("truncated"), Some(&JsValue::Bool(true)));
        assert_eq!(
            glj.get("files"),
            Some(&JsValue::Array(vec![JsValue::from("a.txt")]))
        );

        let grep_res =
            ready(contract.invoke(call("grep", None, br#"{"pattern":"hello"}"#))).unwrap();
        let grj = parse_result(&grep_res);
        assert_eq!(grj.get("truncated"), Some(&JsValue::Bool(true)));
        assert_eq!(grj.get("count"), Some(&JsValue::Number(1.0)));
    }

    #[test]
    fn tc01_only_uses_kernel_verified_environment_and_rejects_reserved_fields_zero_fs_effects() {
        let ws_allowed = TempWorkspace::new("tc01-allowed");
        let ws_other = TempWorkspace::new("tc01-other");
        let sentinel = ws_allowed.path().join("sentinel.txt");
        fs::write(&sentinel, b"original-bytes").unwrap();

        let cfg = WorkspaceToolConfig::for_environment(
            "kernel-env-allowed",
            WorkspaceBoundary::new(ws_allowed.path()).unwrap(),
        )
        .with_environment_boundary(
            "kernel-env-other",
            WorkspaceBoundary::new(ws_other.path()).unwrap(),
        );
        let (contract, _) = workspace_tool_contract(cfg).unwrap();

        // Missing or wrong Kernel ToolCall::environment fails closed even if host env exists.
        let missing = ready(contract.invoke(call(
            "write_file",
            None,
            br#"{"file_path":"blocked.txt","content":"nope"}"#,
        )))
        .unwrap_err();
        assert_eq!(missing.category, ErrorCategory::Environment);
        assert!(!ws_allowed.path().join("blocked.txt").exists());

        // Model JSON reserved host fields (top-level or nested) fail closed with Policy error and zero FS effects.
        for payload in [
            br#"{"file_path":"spoof1.txt","content":"bad","environment":"kernel-env-other"}"#
                .as_slice(),
            br#"{"file_path":"spoof2.txt","content":"bad","host_env":{"kernel_env":"kernel-env-other"}}"#
                .as_slice(),
            br#"{"file_path":"spoof3.txt","content":"bad","verifiedEnvironment":"kernel-env-other"}"#
                .as_slice(),
            br#"{"file_path":"spoof4.txt","content":"bad","_meta":{"turnId":"forged"}}"#
                .as_slice(),
        ] {
            let err = ready(contract.invoke(call(
                "write_file",
                Some("kernel-env-allowed"),
                payload,
            )))
            .unwrap_err();
            assert_eq!(err.category, ErrorCategory::Policy);
        }
        assert_eq!(fs::read(&sentinel).unwrap(), b"original-bytes");
        assert!(!ws_allowed.path().join("spoof1.txt").exists());
        assert!(!ws_allowed.path().join("spoof2.txt").exists());
        assert!(!ws_allowed.path().join("spoof3.txt").exists());
        assert!(!ws_allowed.path().join("spoof4.txt").exists());
        assert_eq!(fs::read_dir(ws_other.path()).unwrap().count(), 0);

        // Verified ToolCall::environment routes exclusively to that environment's boundary.
        let ok = ready(contract.invoke(call(
            "write_file",
            Some("kernel-env-allowed"),
            br#"{"file_path":"created.txt","content":"verified"}"#,
        )))
        .unwrap();
        assert!(ok.ok);
        assert_eq!(
            fs::read(ws_allowed.path().join("created.txt")).unwrap(),
            b"verified"
        );
        assert!(!ws_other.path().join("created.txt").exists());
    }

    #[test]
    fn tc04_pretooluse_rewrite_recomputes_digest_and_reject_cancel_leaves_zero_fs_effects() {
        let ws = TempWorkspace::new("tc04-rewrite");
        let boundary = WorkspaceBoundary::new(ws.path()).unwrap();
        let existing = ws.path().join("existing.txt");
        fs::write(&existing, b"keep-me").unwrap();

        struct RewritingOrCancellingFinalizer {
            deny: bool,
        }
        impl PreAuthArgumentFinalizer for RewritingOrCancellingFinalizer {
            fn finalize_before_auth(
                &self,
                tool: &str,
                _env: &VerifiedToolEnvironment,
                raw_args: &[u8],
            ) -> BoxFuture<'_, Result<FinalizedToolCall, Error>> {
                let deny = self.deny;
                let tool = tool.to_owned();
                let raw = raw_args.to_vec();
                Box::pin(async move {
                    if deny {
                        return Err(Error::new(
                            ErrorCategory::Policy,
                            agentcpm_iface::ERR_INVALID_PARAMS,
                            "pretooluse cancelled/denied",
                        ));
                    }
                    if tool == "write_file" {
                        let rewritten =
                            br#"{"file_path":"authorized.txt","content":"redacted-by-hook"}"#
                                .to_vec();
                        Ok(FinalizedToolCall::new(&raw, rewritten))
                    } else {
                        Ok(FinalizedToolCall::new(&raw, raw.clone()))
                    }
                })
            }
        }

        // 1. Hook rewrite recomputes digest; stale pre-rewrite digest fails closed with 0 FS mutation.
        let contract = CatalogToolContract::new()
            .with_finalizer(Arc::new(RewritingOrCancellingFinalizer { deny: false }));
        register_workspace_tools(&contract, WorkspaceToolConfig::new(boundary.clone())).unwrap();

        let raw_call = call(
            "write_file",
            Some("env-tc04"),
            br#"{"file_path":"draft.txt","content":"raw-secret"}"#,
        );
        let stale_raw_digest = FinalizedToolCall::sha256_hex(&raw_call.args);
        let finalized = ready(contract.pre_authorize_call(&raw_call)).unwrap();
        assert!(finalized.redacted_or_mutated);
        assert_ne!(finalized.sha256_hex, stale_raw_digest);

        let stale_err = ready(contract.invoke_authorized(
            raw_call.clone(),
            finalized.clone(),
            &stale_raw_digest,
        ))
        .unwrap_err();
        assert_eq!(stale_err.category, ErrorCategory::Policy);
        assert!(!ws.path().join("draft.txt").exists());
        assert!(!ws.path().join("authorized.txt").exists());

        // 2. Hook reject/cancel before execution leaves zero FS mutation.
        let denying_contract = CatalogToolContract::new()
            .with_finalizer(Arc::new(RewritingOrCancellingFinalizer { deny: true }));
        register_workspace_tools(&denying_contract, WorkspaceToolConfig::new(boundary)).unwrap();
        let denied_err = ready(denying_contract.invoke(call(
            "edit_file",
            Some("env-tc04"),
            br#"{"file_path":"existing.txt","old_string":"keep-me","new_string":"mutated"}"#,
        )))
        .unwrap_err();
        assert_eq!(denied_err.category, ErrorCategory::Policy);
        assert_eq!(fs::read(&existing).unwrap(), b"keep-me");

        // 3. Matching recomputed terminal authorization digest executes the rewritten payload only.
        let recomputed_digest = finalized.sha256_hex.clone();
        let res =
            ready(contract.invoke_authorized(raw_call, finalized, &recomputed_digest)).unwrap();
        assert!(res.ok);
        assert!(!ws.path().join("draft.txt").exists());
        assert_eq!(
            fs::read(ws.path().join("authorized.txt")).unwrap(),
            b"redacted-by-hook"
        );
    }
}
