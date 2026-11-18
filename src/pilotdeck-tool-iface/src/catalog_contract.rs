//! Multi-tool Kernel ifACE adapter implementing both [`ToolContract`] and
//! [`ToolStaging`] with schema validation, effect classification, and
//! TC-01/TC-04 governance checks.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use agentcpm_iface::{
    BoxFuture, ERR_INVALID_PARAMS,
    tool::{ToolCall, ToolContract, ToolEffect, ToolResult, ToolSpec, ToolStaging},
    types::{CallId, CallMeta, ERR_APP, Error, ErrorCategory},
};
use pilotdeck_protocol::{JsObject, JsValue, parse_json};
use pilotdeck_tool_core::{
    ToolCatalogEntry, ToolConcurrencyPolicy,
    schema::{SchemaValidationError, validate_schema_shape, validate_tool_input},
};

use crate::governance::{
    FinalizedToolCall, PreAuthArgumentFinalizer, VerifiedToolEnvironment, finalize_and_lock_args,
};

/// Verified per-call execution context passed to [`CatalogToolHandler`] after
/// environment verification, PreToolUse finalization, and JSON Schema check.
#[derive(Clone, Debug, PartialEq)]
pub struct VerifiedToolContext {
    pub meta: CallMeta,
    pub environment: VerifiedToolEnvironment,
    pub finalized_args: JsValue,
}

/// Execution port for one trusted registered tool in [`CatalogToolContract`].
pub trait CatalogToolHandler: Send + Sync {
    fn execute(&self, ctx: VerifiedToolContext) -> BoxFuture<'_, Result<ToolResult, Error>>;
}

/// Closure adapter for [`CatalogToolHandler`].
pub struct CatalogToolFn<F>(pub F);

impl<F> CatalogToolFn<F>
where
    F: Fn(VerifiedToolContext) -> BoxFuture<'static, Result<ToolResult, Error>> + Send + Sync,
{
    pub fn new(f: F) -> Self {
        Self(f)
    }
}

impl<F> CatalogToolHandler for CatalogToolFn<F>
where
    F: Fn(VerifiedToolContext) -> BoxFuture<'static, Result<ToolResult, Error>> + Send + Sync,
{
    fn execute(&self, ctx: VerifiedToolContext) -> BoxFuture<'_, Result<ToolResult, Error>> {
        (self.0)(ctx)
    }
}

#[derive(Clone)]
enum EntryKind {
    Stub,
    DeclaredSpec,
    Executable {
        entry: ToolCatalogEntry,
        effect: ToolEffect,
        handler: Arc<dyn CatalogToolHandler>,
    },
}

#[derive(Clone)]
struct RegisteredTool {
    spec: ToolSpec,
    schema: JsValue,
    kind: EntryKind,
}

#[derive(Default)]
struct CatalogState {
    entries: BTreeMap<String, RegisteredTool>,
    aliases: BTreeMap<String, String>,
}

/// Multi-tool [`ToolContract`] + [`ToolStaging`] implementation backed by
/// PilotDeck JSON Schema validation and TC-01/TC-04 governance checks.
pub struct CatalogToolContract {
    state: Mutex<CatalogState>,
    finalizer: Option<Arc<dyn PreAuthArgumentFinalizer>>,
}

impl Default for CatalogToolContract {
    fn default() -> Self {
        Self::new()
    }
}

impl CatalogToolContract {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(CatalogState::default()),
            finalizer: None,
        }
    }

    #[must_use]
    pub fn with_finalizer(mut self, finalizer: Arc<dyn PreAuthArgumentFinalizer>) -> Self {
        self.finalizer = Some(finalizer);
        self
    }

    /// Register a trusted tool implementation, deriving its [`ToolEffect`]
    /// from [`ToolCatalogEntry::concurrency`].
    pub fn register_tool(
        &self,
        entry: ToolCatalogEntry,
        handler: Arc<dyn CatalogToolHandler>,
    ) -> Result<(), Error> {
        let effect = match entry.concurrency {
            ToolConcurrencyPolicy::ConcurrentReadOnly => ToolEffect::ReadOnly,
            ToolConcurrencyPolicy::SerialSideEffect => ToolEffect::SideEffect,
        };
        self.register_tool_with_effect(entry, effect, handler)
    }

    /// Register a trusted tool implementation with an explicit [`ToolEffect`].
    pub fn register_tool_with_effect(
        &self,
        entry: ToolCatalogEntry,
        effect: ToolEffect,
        handler: Arc<dyn CatalogToolHandler>,
    ) -> Result<(), Error> {
        self.register_tools_atomic(vec![(entry, effect, handler)])
    }

    /// Atomically validate and register a batch of trusted tool implementations
    /// so either every entry lands or none do.
    pub fn register_tools_atomic(
        &self,
        items: Vec<(ToolCatalogEntry, ToolEffect, Arc<dyn CatalogToolHandler>)>,
    ) -> Result<(), Error> {
        let mut prepared = Vec::with_capacity(items.len());
        let mut batch_names = BTreeSet::new();

        for (entry, effect, handler) in items {
            let tool = prepare_executable_entry(entry, effect, handler)?;
            if !batch_names.insert(tool.spec.name.clone()) {
                return Err(Error::new(
                    ErrorCategory::Protocol,
                    ERR_INVALID_PARAMS,
                    format!("duplicate tool name '{}' in batch", tool.spec.name),
                ));
            }
            if let EntryKind::Executable { entry, .. } = &tool.kind {
                for alias in &entry.aliases {
                    if !batch_names.insert(alias.clone()) {
                        return Err(Error::new(
                            ErrorCategory::Protocol,
                            ERR_INVALID_PARAMS,
                            format!("duplicate alias '{alias}' in batch"),
                        ));
                    }
                }
            }
            prepared.push(tool);
        }

        let mut state = self.state.lock().map_err(|_| poisoned_error())?;
        for name in &batch_names {
            if state.entries.contains_key(name) || state.aliases.contains_key(name) {
                return Err(Error::new(
                    ErrorCategory::Execution,
                    ERR_APP,
                    format!("tool or alias '{name}' is already registered"),
                ));
            }
        }

        for tool in prepared {
            let canonical = tool.spec.name.clone();
            let aliases = match &tool.kind {
                EntryKind::Executable { entry, .. } => entry.aliases.clone(),
                EntryKind::Stub | EntryKind::DeclaredSpec => Vec::new(),
            };
            state.entries.insert(canonical.clone(), tool);
            for alias in aliases {
                state.aliases.insert(alias, canonical.clone());
            }
        }
        Ok(())
    }

    /// Run PreToolUse / finalization before Kernel terminal authorization and
    /// lock the resulting [`FinalizedToolCall`] digest (`TC-04`).
    pub fn pre_authorize_call(
        &self,
        call: &ToolCall,
    ) -> BoxFuture<'_, Result<FinalizedToolCall, Error>> {
        let resolved = self.resolve_executable(&call.tool);
        let environment = call.environment.clone();
        let raw_args = call.args.clone();
        let finalizer = self.finalizer.clone();
        Box::pin(async move {
            let (canonical_name, _schema, _handler) = resolved?;
            let env = VerifiedToolEnvironment::from_kernel_environment(environment)?;
            env.require_kernel_bound()?;
            finalize_and_lock_args(finalizer.as_deref(), &canonical_name, &env, &raw_args).await
        })
    }

    /// Execute a call using a pre-authorized [`FinalizedToolCall`] (TC-04) after
    /// verifying its SHA-256 digest matches the Kernel's `authorized_sha256_hex`.
    pub fn invoke_authorized(
        &self,
        call: ToolCall,
        finalized: FinalizedToolCall,
        authorized_sha256_hex: &str,
    ) -> BoxFuture<'_, Result<ToolResult, Error>> {
        let authorized_digest = authorized_sha256_hex.to_owned();
        Box::pin(async move {
            finalized.verify_authorized_digest(&authorized_digest)?;
            self.invoke_finalized(call, finalized).await
        })
    }

    /// Execute a call using a pre-authorized [`FinalizedToolCall`] (TC-04),
    /// verifying digest integrity, anti-spoofing, and JSON Schema before
    /// dispatching the registered handler.
    pub fn invoke_finalized(
        &self,
        call: ToolCall,
        finalized: FinalizedToolCall,
    ) -> BoxFuture<'_, Result<ToolResult, Error>> {
        Box::pin(async move {
            let (_canonical_name, schema, handler) = self.resolve_executable(&call.tool)?;
            let env = VerifiedToolEnvironment::from_tool_call(&call)?;
            env.require_kernel_bound()?;
            VerifiedToolEnvironment::reject_reserved_host_fields_bytes(&call.args)?;
            finalized.verify_digest()?;
            if !finalized.redacted_or_mutated && call.args != finalized.args_bytes {
                return Err(Error::new(
                    ErrorCategory::Policy,
                    ERR_INVALID_PARAMS,
                    "tool call arguments diverge from pre-authorized finalized payload",
                ));
            }
            dispatch_verified_bytes(call.meta, env, &schema, &finalized.args_bytes, handler).await
        })
    }

    fn resolve_executable(
        &self,
        tool_name: &str,
    ) -> Result<(String, JsValue, Arc<dyn CatalogToolHandler>), Error> {
        let resolved = {
            let state = self.state.lock().map_err(|_| poisoned_error())?;
            let canonical = state
                .aliases
                .get(tool_name)
                .cloned()
                .unwrap_or_else(|| tool_name.to_owned());
            state.entries.get(&canonical).map(|item| {
                let handler = match &item.kind {
                    EntryKind::Executable { handler, .. } => Some(handler.clone()),
                    EntryKind::Stub | EntryKind::DeclaredSpec => None,
                };
                (canonical, item.schema.clone(), handler)
            })
        };
        let Some((canonical_name, schema, handler)) = resolved else {
            return Err(Error::new(
                ErrorCategory::Execution,
                -32033,
                format!("unknown tool: {tool_name}"),
            ));
        };
        let Some(handler) = handler else {
            return Err(Error::new(
                ErrorCategory::Execution,
                -32033,
                format!("tool '{tool_name}' has no executable implementation"),
            ));
        };
        Ok((canonical_name, schema, handler))
    }

    /// Snapshot pure catalog metadata entries for all executable tools (for
    /// [`pilotdeck_tool_core::partition_tool_batch`]).
    pub fn catalog_entries(&self) -> Vec<ToolCatalogEntry> {
        let Ok(state) = self.state.lock() else {
            return Vec::new();
        };
        state
            .entries
            .values()
            .filter_map(|item| match &item.kind {
                EntryKind::Executable { entry, .. } => Some(entry.clone()),
                EntryKind::Stub | EntryKind::DeclaredSpec => None,
            })
            .collect()
    }
}

impl ToolContract for CatalogToolContract {
    fn effect(&self, tool: &str) -> ToolEffect {
        let Ok(state) = self.state.lock() else {
            return ToolEffect::Unknown;
        };
        let canonical = state.aliases.get(tool).map(String::as_str).unwrap_or(tool);
        match state.entries.get(canonical).map(|item| &item.kind) {
            Some(EntryKind::Executable { effect, .. }) => *effect,
            Some(EntryKind::Stub | EntryKind::DeclaredSpec) | None => ToolEffect::Unknown,
        }
    }

    fn register(&self, spec: ToolSpec) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            validate_tool_name(&spec.name)?;
            if spec.description.trim().is_empty() {
                return Err(Error::new(
                    ErrorCategory::Protocol,
                    ERR_INVALID_PARAMS,
                    format!("tool '{}' description cannot be empty", spec.name),
                ));
            }
            let schema = parse_schema_bytes(&spec.parameters)?;

            let mut state = self.state.lock().map_err(|_| poisoned_error())?;
            if state.aliases.contains_key(&spec.name) {
                return Err(Error::new(
                    ErrorCategory::Execution,
                    ERR_APP,
                    format!("tool '{}' conflicts with an existing alias", spec.name),
                ));
            }
            if let Some(existing) = state.entries.get(&spec.name) {
                return match &existing.kind {
                    EntryKind::Stub => Err(Error::new(
                        ErrorCategory::Execution,
                        ERR_APP,
                        format!(
                            "tool '{}' is currently staged as a stub and cannot be registered",
                            spec.name
                        ),
                    )),
                    EntryKind::DeclaredSpec | EntryKind::Executable { .. } => {
                        if existing.spec == spec {
                            Ok(())
                        } else {
                            Err(Error::new(
                                ErrorCategory::Execution,
                                ERR_APP,
                                format!(
                                    "tool '{}' spec conflicts with existing registration",
                                    spec.name
                                ),
                            ))
                        }
                    }
                };
            }

            state.entries.insert(
                spec.name.clone(),
                RegisteredTool {
                    spec,
                    schema,
                    kind: EntryKind::DeclaredSpec,
                },
            );
            Ok(())
        })
    }

    fn list(&self) -> BoxFuture<'_, Vec<ToolSpec>> {
        Box::pin(async move {
            let Ok(state) = self.state.lock() else {
                return Vec::new();
            };
            state
                .entries
                .values()
                .map(|entry| entry.spec.clone())
                .collect()
        })
    }

    fn invoke(&self, call: ToolCall) -> BoxFuture<'_, Result<ToolResult, Error>> {
        Box::pin(async move {
            let (canonical_name, schema, handler) = self.resolve_executable(&call.tool)?;
            let env = VerifiedToolEnvironment::from_tool_call(&call)?;
            let finalized = finalize_and_lock_args(
                self.finalizer.as_deref(),
                &canonical_name,
                &env,
                &call.args,
            )
            .await?;

            dispatch_verified_bytes(call.meta, env, &schema, &finalized.args_bytes, handler).await
        })
    }

    fn abort(&self, _call: &CallId) -> BoxFuture<'_, Result<(), Error>> {
        crate::support::unsupported_abort("catalog tool does not support abort")
    }
}

impl ToolStaging for CatalogToolContract {
    fn contains(&self, name: &str) -> bool {
        let Ok(state) = self.state.lock() else {
            return false;
        };
        state.entries.contains_key(name) || state.aliases.contains_key(name)
    }

    fn register_stub(&self, name: &str) -> Result<(), Error> {
        validate_tool_name(name)?;
        let mut state = self.state.lock().map_err(|_| poisoned_error())?;
        if state.entries.contains_key(name) || state.aliases.contains_key(name) {
            return Err(Error::new(
                ErrorCategory::Execution,
                ERR_APP,
                format!("tool or stub '{name}' is already registered"),
            ));
        }
        let schema = parse_json(r#"{"type":"object"}"#).map_err(|err| {
            Error::new(
                ErrorCategory::Internal,
                ERR_APP,
                format!("stub schema creation failed: {err}"),
            )
        })?;
        let spec = ToolSpec {
            name: name.to_owned(),
            description: format!("staged stub tool: {name}"),
            parameters: br#"{"type":"object"}"#.to_vec(),
        };
        state.entries.insert(
            name.to_owned(),
            RegisteredTool {
                spec,
                schema,
                kind: EntryKind::Stub,
            },
        );
        Ok(())
    }

    fn remove_stub(&self, name: &str) -> Result<(), Error> {
        let mut state = self.state.lock().map_err(|_| poisoned_error())?;
        let Some(existing) = state.entries.get(name) else {
            return Ok(());
        };
        match existing.kind {
            EntryKind::Stub | EntryKind::DeclaredSpec => {
                state.entries.remove(name);
                Ok(())
            }
            EntryKind::Executable { .. } => Err(Error::new(
                ErrorCategory::Execution,
                ERR_APP,
                format!("cannot remove active executable tool '{name}' as stub"),
            )),
        }
    }
}

fn validate_tool_name(name: &str) -> Result<(), Error> {
    if name.is_empty()
        || name != name.trim()
        || name.chars().any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err(Error::new(
            ErrorCategory::Protocol,
            ERR_INVALID_PARAMS,
            format!("invalid tool name: {name:?}"),
        ));
    }
    Ok(())
}

fn parse_schema_bytes(raw: &[u8]) -> Result<JsValue, Error> {
    let text = std::str::from_utf8(raw).map_err(|_| {
        Error::new(
            ErrorCategory::Protocol,
            ERR_INVALID_PARAMS,
            "tool parameters must be valid UTF-8 JSON Schema",
        )
    })?;
    let schema = parse_json(text).map_err(|err| {
        Error::new(
            ErrorCategory::Protocol,
            ERR_INVALID_PARAMS,
            format!("tool parameters must be valid JSON Schema: {err}"),
        )
    })?;
    if !matches!(schema, JsValue::Object(_)) {
        return Err(Error::new(
            ErrorCategory::Protocol,
            ERR_INVALID_PARAMS,
            "tool parameters schema must be a JSON object",
        ));
    }
    if let Err(err) = validate_schema_shape(&schema) {
        return Err(Error::new(
            ErrorCategory::Protocol,
            ERR_INVALID_PARAMS,
            format!("invalid tool parameters schema: {err}"),
        ));
    }
    Ok(schema)
}

fn poisoned_error() -> Error {
    Error::new(
        ErrorCategory::Internal,
        ERR_APP,
        "catalog tool registry lock poisoned",
    )
}

fn json_bytes(value: &JsValue) -> Result<Vec<u8>, Error> {
    crate::support::json_bytes(value, "catalog tool JSON encoding failed")
}

fn invalid_input_result(
    message: &str,
    validation: Option<&SchemaValidationError>,
) -> Result<ToolResult, Error> {
    let mut obj = JsObject::new();
    obj.insert("code".into(), "invalid_input".into());
    obj.insert("message".into(), message.into());
    if let Some(err) = validation {
        let issues = err
            .issues()
            .iter()
            .map(|issue| {
                let mut item = JsObject::new();
                item.insert("path".into(), issue.path.as_str().into());
                item.insert("code".into(), issue.code.as_str().into());
                item.insert("message".into(), issue.message.as_str().into());
                JsValue::Object(item)
            })
            .collect();
        obj.insert("issues".into(), JsValue::Array(issues));
    }
    Ok(ToolResult {
        ok: false,
        bytes: json_bytes(&JsValue::Object(obj))?,
        unknown: false,
    })
}

fn prepare_executable_entry(
    entry: ToolCatalogEntry,
    effect: ToolEffect,
    handler: Arc<dyn CatalogToolHandler>,
) -> Result<RegisteredTool, Error> {
    validate_tool_name(&entry.name)?;
    if entry.description.trim().is_empty() {
        return Err(Error::new(
            ErrorCategory::Protocol,
            ERR_INVALID_PARAMS,
            format!("tool '{}' description cannot be empty", entry.name),
        ));
    }
    for (idx, alias) in entry.aliases.iter().enumerate() {
        validate_tool_name(alias)?;
        if alias == &entry.name {
            return Err(Error::new(
                ErrorCategory::Protocol,
                ERR_INVALID_PARAMS,
                format!("tool '{}' alias duplicates canonical name", entry.name),
            ));
        }
        if entry.aliases[..idx].contains(alias) {
            return Err(Error::new(
                ErrorCategory::Protocol,
                ERR_INVALID_PARAMS,
                format!("duplicate alias '{alias}' on tool '{}'", entry.name),
            ));
        }
    }
    if let Err(err) = validate_schema_shape(&entry.input_schema) {
        return Err(Error::new(
            ErrorCategory::Protocol,
            ERR_INVALID_PARAMS,
            format!("tool '{}' has invalid input schema: {err}", entry.name),
        ));
    }
    let parameters = json_bytes(&entry.input_schema)?;
    let spec = ToolSpec {
        name: entry.name.clone(),
        description: entry.description.clone(),
        parameters,
    };
    let schema = entry.input_schema.clone();
    Ok(RegisteredTool {
        spec,
        schema,
        kind: EntryKind::Executable {
            entry,
            effect,
            handler,
        },
    })
}

async fn dispatch_verified_bytes(
    meta: CallMeta,
    env: VerifiedToolEnvironment,
    schema: &JsValue,
    finalized_bytes: &[u8],
    handler: Arc<dyn CatalogToolHandler>,
) -> Result<ToolResult, Error> {
    let Ok(text) = std::str::from_utf8(finalized_bytes) else {
        return invalid_input_result("Tool arguments must be valid UTF-8 JSON", None);
    };
    let parsed_args = match parse_json(text) {
        Ok(value) => value,
        Err(err) => {
            return invalid_input_result(&format!("Invalid JSON arguments: {err}"), None);
        }
    };

    VerifiedToolEnvironment::reject_reserved_host_fields(&parsed_args)?;

    if let Err(err) = validate_tool_input(schema, &parsed_args) {
        return invalid_input_result(&err.message(), Some(&err));
    }

    let ctx = VerifiedToolContext {
        meta,
        environment: env,
        finalized_args: parsed_args,
    };
    handler.execute(ctx).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilotdeck_tool_core::{
        TODO_TOOL_NAME, apply_todo_update, parse_todo_write_request, todo_definition,
    };
    use std::future::Future;
    use std::pin::pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll, Waker};

    fn ready<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let mut cx = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("catalog contract test future must complete synchronously"),
        }
    }

    fn make_call(tool: &str, env: Option<&str>, args: &[u8]) -> ToolCall {
        ToolCall::new(
            CallMeta {
                session_id: "sess".into(),
                call_id: "call-1".into(),
                turn: 1,
            },
            tool.into(),
            env.map(str::to_owned),
            args.to_vec(),
        )
    }

    #[test]
    fn staging_is_atomic_idempotent_and_fails_closed_for_effects_and_invoke() {
        let contract = CatalogToolContract::new();

        assert!(!contract.contains("pkg.stub"));
        assert_eq!(contract.effect("pkg.stub"), ToolEffect::Unknown);

        // Empty or whitespace names fail without leaving half-state.
        for bad in ["", "   ", "bad name", "bad\t"] {
            assert!(contract.register_stub(bad).is_err());
            assert!(!contract.contains(bad));
        }

        contract.register_stub("pkg.stub").unwrap();
        assert!(contract.contains("pkg.stub"));
        assert_eq!(contract.effect("pkg.stub"), ToolEffect::Unknown);
        assert!(
            ready(contract.list())
                .iter()
                .any(|spec| spec.name == "pkg.stub")
        );

        // Duplicate stub registration fails without mutating existing state.
        assert!(contract.register_stub("pkg.stub").is_err());
        assert!(contract.contains("pkg.stub"));

        // register(spec) over an existing stub refuses cleanly.
        let stub_conflict = ToolSpec {
            name: "pkg.stub".into(),
            description: "conflict".into(),
            parameters: br#"{"type":"object"}"#.to_vec(),
        };
        assert!(ready(contract.register(stub_conflict)).is_err());

        // Invoking a stub fails closed with -32033.
        let err = ready(contract.invoke(make_call("pkg.stub", Some("local"), b"{}"))).unwrap_err();
        assert_eq!(err.category, ErrorCategory::Execution);
        assert_eq!(err.code, -32033);

        // Rollback + idempotent remove_stub on missing.
        contract.remove_stub("pkg.stub").unwrap();
        assert!(!contract.contains("pkg.stub"));
        contract.remove_stub("pkg.stub").unwrap();
        contract.remove_stub("never.existed").unwrap();
    }

    #[test]
    fn register_validates_schema_and_detects_conflicting_specs() {
        let contract = CatalogToolContract::new();

        // Invalid UTF-8 or non-object schema rejected.
        for bad_params in [&b"\xff"[..], &b"null"[..], &br#"{"type":123}"#[..]] {
            let bad = ToolSpec {
                name: "bad.tool".into(),
                description: "desc".into(),
                parameters: bad_params.to_vec(),
            };
            assert!(ready(contract.register(bad)).is_err());
            assert!(!contract.contains("bad.tool"));
        }

        let valid = ToolSpec {
            name: "valid.spec".into(),
            description: "desc".into(),
            parameters: br#"{"type":"object","additionalProperties":false}"#.to_vec(),
        };
        ready(contract.register(valid.clone())).unwrap();
        assert!(contract.contains("valid.spec"));
        assert_eq!(contract.effect("valid.spec"), ToolEffect::Unknown);

        // Idempotent re-register of identical spec succeeds; modified spec fails.
        ready(contract.register(valid.clone())).unwrap();
        let mut modified = valid;
        modified.description.push('!');
        assert!(ready(contract.register(modified)).is_err());
    }

    #[test]
    fn validates_schema_before_handler_and_runs_pre_auth_finalizer_and_anti_spoofing() {
        let dispatches = Arc::new(AtomicUsize::new(0));
        let dispatches_ref = dispatches.clone();

        struct MutatingFinalizer;
        impl PreAuthArgumentFinalizer for MutatingFinalizer {
            fn finalize_before_auth(
                &self,
                _tool: &str,
                _env: &VerifiedToolEnvironment,
                raw_args: &[u8],
            ) -> BoxFuture<'_, Result<FinalizedToolCall, Error>> {
                let finalized = if raw_args
                    == br#"{"todos":[{"id":"1","content":"raw","status":"pending"}]}"# {
                    FinalizedToolCall::new(
                        raw_args,
                        br#"{"todos":[{"id":"1","content":"sanitized","status":"in_progress"}]}"#
                            .to_vec(),
                    )
                } else {
                    FinalizedToolCall::new(raw_args, raw_args.to_vec())
                };
                Box::pin(async move { Ok(finalized) })
            }
        }

        let contract = CatalogToolContract::new().with_finalizer(Arc::new(MutatingFinalizer));
        let def = todo_definition();
        let todo_entry = ToolCatalogEntry {
            name: TODO_TOOL_NAME.into(),
            aliases: vec!["TodoWrite".into()],
            description: String::from_utf16_lossy(
                def.get("description").unwrap().as_string().unwrap().units(),
            ),
            kind: "session".into(),
            input_schema: def.get("inputSchema").unwrap().clone(),
            concurrency: ToolConcurrencyPolicy::SerialSideEffect,
        };
        contract
            .register_tool(
                todo_entry,
                Arc::new(CatalogToolFn::new(move |ctx| {
                    let dispatches = dispatches_ref.clone();
                    Box::pin(async move {
                        dispatches.fetch_add(1, Ordering::SeqCst);
                        assert_eq!(ctx.environment.environment_id(), Some("env-1"));
                        let req = parse_todo_write_request(&ctx.finalized_args).unwrap();
                        let updated = apply_todo_update(&[], &req).unwrap();
                        assert_eq!(updated[0].content, "sanitized");
                        assert_eq!(updated[0].status, "in_progress");
                        Ok(ToolResult {
                            ok: true,
                            bytes: br#"{"ok":true}"#.to_vec(),
                            unknown: false,
                        })
                    })
                })),
            )
            .unwrap();

        assert_eq!(contract.effect(TODO_TOOL_NAME), ToolEffect::SideEffect);
        assert_eq!(contract.effect("TodoWrite"), ToolEffect::SideEffect);
        assert_eq!(contract.effect("unknown"), ToolEffect::Unknown);

        // Invalid enum value fails closed with structured ok=false before handler runs.
        let bad_enum = ready(contract.invoke(make_call(
            "TodoWrite",
            Some("env-1"),
            br#"{"todos":[{"id":"1","content":"x","status":"bogus"}]}"#,
        )))
        .unwrap();
        assert!(!bad_enum.ok && !bad_enum.unknown);
        let err_json = parse_json(std::str::from_utf8(&bad_enum.bytes).unwrap()).unwrap();
        assert_eq!(err_json.get("code"), Some(&JsValue::from("invalid_input")));
        assert_eq!(dispatches.load(Ordering::SeqCst), 0);

        // Spoofed environment field in args fails with Policy error before handler runs.
        let spoof_err = ready(contract.invoke(make_call(
            TODO_TOOL_NAME,
            Some("env-1"),
            br#"{"environment":"host"}"#,
        )))
        .unwrap_err();
        assert_eq!(spoof_err.category, ErrorCategory::Policy);
        assert_eq!(dispatches.load(Ordering::SeqCst), 0);

        // Valid input runs PreAuthArgumentFinalizer and passes finalized_args + VerifiedToolEnvironment.
        let ok = ready(contract.invoke(make_call(
            "TodoWrite",
            Some("env-1"),
            br#"{"todos":[{"id":"1","content":"raw","status":"pending"}]}"#,
        )))
        .unwrap();
        assert!(ok.ok && !ok.unknown);
        assert_eq!(dispatches.load(Ordering::SeqCst), 1);

        // Cannot remove active executable tool via remove_stub.
        assert!(contract.remove_stub(TODO_TOOL_NAME).is_err());

        // Catalog metadata integrates with pure batch partitioning.
        let batch = pilotdeck_tool_core::partition_tool_batch(
            &contract.catalog_entries(),
            &[pilotdeck_tool_core::ToolCallCandidate {
                call_id: "c1".into(),
                tool_name: "TodoWrite".into(),
                read_only_call: true,
            }],
        );
        assert_eq!(
            batch,
            vec![pilotdeck_tool_core::ToolBatchSlice {
                concurrent: false,
                call_ids: vec!["c1".into()],
            }]
        );
    }

    #[test]
    fn rejects_mismatched_pre_auth_digest_before_dispatching_handler() {
        let dispatches = Arc::new(AtomicUsize::new(0));
        let dispatches_ref = dispatches.clone();

        struct BrokenDigestFinalizer;
        impl PreAuthArgumentFinalizer for BrokenDigestFinalizer {
            fn finalize_before_auth(
                &self,
                _tool: &str,
                _env: &VerifiedToolEnvironment,
                raw_args: &[u8],
            ) -> BoxFuture<'_, Result<FinalizedToolCall, Error>> {
                let broken = FinalizedToolCall {
                    args_bytes: raw_args.to_vec(),
                    sha256_hex: "0".repeat(64),
                    redacted_or_mutated: false,
                };
                Box::pin(async move { Ok(broken) })
            }
        }

        let contract = CatalogToolContract::new().with_finalizer(Arc::new(BrokenDigestFinalizer));
        contract
            .register_tool(
                ToolCatalogEntry {
                    name: "probe".into(),
                    aliases: vec![],
                    description: "probe".into(),
                    kind: "custom".into(),
                    input_schema: parse_json(r#"{"type":"object"}"#).unwrap(),
                    concurrency: ToolConcurrencyPolicy::ConcurrentReadOnly,
                },
                Arc::new(CatalogToolFn::new(move |_| {
                    dispatches_ref.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async {
                        Ok(ToolResult {
                            ok: true,
                            bytes: b"{}".to_vec(),
                            unknown: false,
                        })
                    })
                })),
            )
            .unwrap();

        let err = ready(contract.invoke(make_call("probe", Some("env-1"), b"{}"))).unwrap_err();
        assert_eq!(err.category, ErrorCategory::Policy);
        assert_eq!(dispatches.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn register_tools_atomic_rejects_in_batch_alias_conflict_without_partial_state() {
        let contract = CatalogToolContract::new();
        let make_entry = |name: &str, aliases: Vec<&str>| ToolCatalogEntry {
            name: name.into(),
            aliases: aliases.into_iter().map(str::to_owned).collect(),
            description: format!("tool {name}"),
            kind: "custom".into(),
            input_schema: parse_json(r#"{"type":"object"}"#).unwrap(),
            concurrency: ToolConcurrencyPolicy::ConcurrentReadOnly,
        };
        let noop = Arc::new(CatalogToolFn::new(|_| {
            Box::pin(async {
                Ok(ToolResult {
                    ok: true,
                    bytes: b"{}".to_vec(),
                    unknown: false,
                })
            })
        }));

        let err = contract
            .register_tools_atomic(vec![
                (
                    make_entry("tool.a", vec!["SharedAlias"]),
                    ToolEffect::ReadOnly,
                    noop.clone(),
                ),
                (
                    make_entry("tool.b", vec!["SharedAlias"]),
                    ToolEffect::ReadOnly,
                    noop,
                ),
            ])
            .unwrap_err();
        assert_eq!(err.category, ErrorCategory::Protocol);
        assert!(!contract.contains("tool.a"));
        assert!(!contract.contains("tool.b"));
        assert!(!contract.contains("SharedAlias"));
    }
}
