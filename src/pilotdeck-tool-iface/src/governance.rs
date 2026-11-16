//! TC-01 & TC-04 governance primitives for PilotDeck tool adapters:
//! - [`VerifiedToolEnvironment`]: kernel-bound environment provenance and
//!   anti-spoofing guard against model-supplied reserved host fields.
//! - [`PreAuthArgumentFinalizer`] / [`FinalizedToolCall`]: PreToolUse argument
//!   mutation + SHA-256 digest binding before Kernel final authorization.
//! - [`NestedToolInvoker`] / [`GovernedNestedToolInvoker`]: Kernel-authorized
//!   re-entry for nested tool invocations carrying preserved parent environment.

use std::fmt::Write as _;
use std::sync::Arc;

use agentcpm_iface::{
    BoxFuture, ERR_INVALID_PARAMS,
    env::EnvId,
    tool::{ToolCall, ToolContract, ToolEffect, ToolResult},
    types::{CallMeta, Error, ErrorCategory},
};
use pilotdeck_protocol::{JsString, JsValue, parse_json};
use sha2::{Digest, Sha256};

const MAX_INSPECT_DEPTH: usize = 128;

/// Host/Kernel-owned boundary keys that model-supplied JSON tool arguments must
/// never supply to spoof execution environment or call identity.
pub const RESERVED_HOST_FIELDS: &[&str] = &[
    "environment",
    "environment_id",
    "environmentId",
    "env_id",
    "envId",
    "__environment",
    "__env",
    "host_env",
    "hostEnv",
    "verified_environment",
    "verifiedEnvironment",
    "kernel_env",
    "kernelEnv",
    "session_id",
    "sessionId",
    "call_id",
    "callId",
    "turn_id",
    "turnId",
    "_meta",
    "__host",
    "host_authority",
    "hostAuthority",
];

/// Trusted execution environment provenance bound strictly from the Kernel's
/// [`ToolCall::environment`] boundary (TC-01).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedToolEnvironment {
    pub environment_id: Option<String>,
    kernel_bound: bool,
}

impl VerifiedToolEnvironment {
    /// Construct from a Kernel [`ToolCall`] without ever inspecting `call.args`.
    pub fn from_tool_call(call: &ToolCall) -> Result<Self, Error> {
        Self::from_kernel_environment(call.environment.clone())
    }

    /// Construct from a Kernel-supplied optional environment identifier.
    pub fn from_kernel_environment(environment_id: Option<String>) -> Result<Self, Error> {
        let validated = match environment_id {
            None => None,
            Some(raw) => {
                let env_id = EnvId::try_from(raw)?;
                Some(String::from(env_id))
            }
        };
        Ok(Self {
            environment_id: validated,
            kernel_bound: true,
        })
    }

    /// Returns the verified environment identifier when present.
    pub fn environment_id(&self) -> Option<&str> {
        self.environment_id.as_deref()
    }

    /// Returns the strongly typed [`EnvId`] when an environment is bound.
    pub fn env_id(&self) -> Result<Option<EnvId>, Error> {
        self.environment_id.as_deref().map(EnvId::new).transpose()
    }

    /// Indicates that this provenance came from a trusted Kernel constructor.
    pub fn is_kernel_bound(&self) -> bool {
        self.kernel_bound
    }

    /// Require a non-empty bound environment identifier, failing closed otherwise.
    pub fn require_environment_id(&self) -> Result<&str, Error> {
        if !self.kernel_bound {
            return Err(Error::new(
                ErrorCategory::Environment,
                ERR_INVALID_PARAMS,
                "tool environment provenance is not kernel-bound",
            ));
        }
        match self.environment_id.as_deref() {
            Some(env_id) if !env_id.is_empty() => Ok(env_id),
            _ => Err(Error::new(
                ErrorCategory::Environment,
                ERR_INVALID_PARAMS,
                "missing verified kernel environment id",
            )),
        }
    }

    /// Detect and reject attempts by model JSON arguments to supply reserved host
    /// or environment-spoofing fields.
    pub fn reject_reserved_host_fields(args: &JsValue) -> Result<(), Error> {
        inspect_value_for_reserved_fields(args, 0)
    }

    /// Parse UTF-8 JSON bytes (if valid JSON) and reject reserved host fields.
    /// Non-UTF-8 or invalid JSON bytes are left untouched so downstream schema
    /// parsing reports structured input errors.
    pub fn reject_reserved_host_fields_bytes(raw_args: &[u8]) -> Result<(), Error> {
        let Ok(text) = std::str::from_utf8(raw_args) else {
            return Ok(());
        };
        let Ok(parsed) = parse_json(text) else {
            return Ok(());
        };
        Self::reject_reserved_host_fields(&parsed)
    }
}

fn inspect_value_for_reserved_fields(value: &JsValue, depth: usize) -> Result<(), Error> {
    if depth > MAX_INSPECT_DEPTH {
        return Err(Error::new(
            ErrorCategory::Policy,
            ERR_INVALID_PARAMS,
            "tool argument nesting exceeds maximum inspection depth",
        ));
    }
    match value {
        JsValue::Object(obj) => {
            for (key, child) in obj.own_entries() {
                if child.is_undefined() {
                    continue;
                }
                if is_reserved_host_field(key) {
                    let key_name = String::from_utf16_lossy(key.units());
                    return Err(Error::new(
                        ErrorCategory::Policy,
                        ERR_INVALID_PARAMS,
                        format!(
                            "reserved host field '{key_name}' cannot be supplied in tool arguments"
                        ),
                    ));
                }
                inspect_value_for_reserved_fields(child, depth + 1)?;
            }
        }
        JsValue::Array(items) => {
            for item in items {
                inspect_value_for_reserved_fields(item, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn is_reserved_host_field(key: &JsString) -> bool {
    RESERVED_HOST_FIELDS
        .iter()
        .any(|reserved| key.equals_str(reserved))
}

/// Exact post-PreToolUse argument bytes and SHA-256 digest locked prior to
/// Kernel final authorization (TC-04).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalizedToolCall {
    pub args_bytes: Vec<u8>,
    pub sha256_hex: String,
    pub redacted_or_mutated: bool,
}

impl FinalizedToolCall {
    /// Compute the canonical SHA-256 digest over `finalized_bytes` and record
    /// whether PreToolUse mutated or redacted `raw_args`.
    pub fn new(raw_args: &[u8], finalized_bytes: Vec<u8>) -> Self {
        let sha256_hex = Self::sha256_hex(&finalized_bytes);
        let redacted_or_mutated = finalized_bytes != raw_args;
        Self {
            args_bytes: finalized_bytes,
            sha256_hex,
            redacted_or_mutated,
        }
    }

    /// Lowercase hexadecimal SHA-256 digest of `bytes`.
    pub fn sha256_hex(bytes: &[u8]) -> String {
        let digest = Sha256::digest(bytes);
        let mut out = String::with_capacity(digest.len() * 2);
        for byte in digest {
            let _ = write!(&mut out, "{byte:02x}");
        }
        out
    }

    /// Fail closed if `args_bytes` does not match the authorized `sha256_hex`.
    pub fn verify_digest(&self) -> Result<(), Error> {
        let actual = Self::sha256_hex(&self.args_bytes);
        if actual != self.sha256_hex {
            return Err(Error::new(
                ErrorCategory::Policy,
                ERR_INVALID_PARAMS,
                format!(
                    "finalized tool call digest mismatch: expected {}, got {actual}",
                    self.sha256_hex
                ),
            ));
        }
        Ok(())
    }
}

/// Pre-authorization hook/redaction boundary (TC-04).
///
/// Implementations materialize the exact bytes that Kernel authorization sees
/// and that tool execution must consume without post-auth drift.
pub trait PreAuthArgumentFinalizer: Send + Sync {
    fn finalize_before_auth(
        &self,
        tool: &str,
        env: &VerifiedToolEnvironment,
        raw_args: &[u8],
    ) -> BoxFuture<'_, Result<FinalizedToolCall, Error>>;
}

/// Default [`PreAuthArgumentFinalizer`] that preserves raw arguments, rejects
/// reserved host fields, and binds the SHA-256 digest.
#[derive(Clone, Copy, Debug, Default)]
pub struct PassthroughArgumentFinalizer;

impl PreAuthArgumentFinalizer for PassthroughArgumentFinalizer {
    fn finalize_before_auth(
        &self,
        _tool: &str,
        env: &VerifiedToolEnvironment,
        raw_args: &[u8],
    ) -> BoxFuture<'_, Result<FinalizedToolCall, Error>> {
        let kernel_bound = env.is_kernel_bound();
        let bytes = raw_args.to_vec();
        Box::pin(async move {
            if !kernel_bound {
                return Err(Error::new(
                    ErrorCategory::Environment,
                    ERR_INVALID_PARAMS,
                    "tool environment provenance is not kernel-bound",
                ));
            }
            VerifiedToolEnvironment::reject_reserved_host_fields_bytes(&bytes)?;
            Ok(FinalizedToolCall::new(&bytes, bytes.clone()))
        })
    }
}

/// Execute pre-auth finalization and enforce both digest integrity and
/// anti-spoofing invariants on the resulting bytes.
pub async fn finalize_and_lock_args(
    finalizer: Option<&dyn PreAuthArgumentFinalizer>,
    tool: &str,
    env: &VerifiedToolEnvironment,
    raw_args: &[u8],
) -> Result<FinalizedToolCall, Error> {
    if !env.is_kernel_bound() {
        return Err(Error::new(
            ErrorCategory::Environment,
            ERR_INVALID_PARAMS,
            "tool environment provenance is not kernel-bound",
        ));
    }
    VerifiedToolEnvironment::reject_reserved_host_fields_bytes(raw_args)?;
    let finalized = match finalizer {
        Some(custom) => custom.finalize_before_auth(tool, env, raw_args).await?,
        None => FinalizedToolCall::new(raw_args, raw_args.to_vec()),
    };
    finalized.verify_digest()?;
    VerifiedToolEnvironment::reject_reserved_host_fields_bytes(&finalized.args_bytes)?;
    Ok(finalized)
}

/// Boundary for nested tool calls invoked inside an active tool execution
/// (e.g., `execute_code` calling back into registered tools).
pub trait NestedToolInvoker: Send + Sync {
    fn invoke_nested(
        &self,
        meta: CallMeta,
        environment: &VerifiedToolEnvironment,
        tool: &str,
        args: Vec<u8>,
    ) -> BoxFuture<'_, Result<ToolResult, Error>>;
}

/// Governed nested tool invoker that enforces parent-preserved environment
/// propagation and routes every nested call through an `Arc<dyn ToolContract>`
/// rather than bypassing Kernel authorization via direct handler calls.
pub struct GovernedNestedToolInvoker {
    contract: Arc<dyn ToolContract>,
    finalizer: Option<Arc<dyn PreAuthArgumentFinalizer>>,
}

impl GovernedNestedToolInvoker {
    pub fn new(contract: Arc<dyn ToolContract>) -> Self {
        Self {
            contract,
            finalizer: None,
        }
    }

    #[must_use]
    pub fn with_finalizer(mut self, finalizer: Arc<dyn PreAuthArgumentFinalizer>) -> Self {
        self.finalizer = Some(finalizer);
        self
    }

    pub fn contract(&self) -> &Arc<dyn ToolContract> {
        &self.contract
    }
}

impl NestedToolInvoker for GovernedNestedToolInvoker {
    fn invoke_nested(
        &self,
        meta: CallMeta,
        environment: &VerifiedToolEnvironment,
        tool: &str,
        args: Vec<u8>,
    ) -> BoxFuture<'_, Result<ToolResult, Error>> {
        let env = environment.clone();
        let tool_name = tool.to_owned();
        Box::pin(async move {
            let env_id = env.require_environment_id()?.to_owned();
            if tool_name.trim().is_empty() {
                return Err(Error::new(
                    ErrorCategory::Protocol,
                    ERR_INVALID_PARAMS,
                    "nested tool name cannot be empty",
                ));
            }
            let effect = self.contract.effect(&tool_name);
            if effect == ToolEffect::Unknown {
                let listed = self.contract.list().await;
                let known_in_list = listed.iter().any(|spec| spec.name == tool_name);
                if !known_in_list {
                    return Err(Error::new(
                        ErrorCategory::Execution,
                        -32033,
                        format!("unknown nested tool: {tool_name}"),
                    ));
                }
            }
            let finalized =
                finalize_and_lock_args(self.finalizer.as_deref(), &tool_name, &env, &args).await?;
            let nested_call = ToolCall::new(meta, tool_name, Some(env_id), finalized.args_bytes);
            self.contract.invoke(nested_call).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn sample_meta() -> CallMeta {
        CallMeta {
            session_id: "sess-1".into(),
            call_id: "call-1".into(),
            turn: 1,
        }
    }

    #[test]
    fn verified_environment_only_reads_kernel_call_environment() {
        let call = ToolCall::new(
            sample_meta(),
            "read_file".into(),
            Some("sandbox-a".into()),
            br#"{"path":"a.txt","environment":"host-escape"}"#.to_vec(),
        );
        let env = VerifiedToolEnvironment::from_tool_call(&call).unwrap();
        assert_eq!(env.environment_id(), Some("sandbox-a"));
        assert!(env.is_kernel_bound());

        let err = VerifiedToolEnvironment::reject_reserved_host_fields_bytes(&call.args)
            .expect_err("reserved environment key in args must be blocked");
        assert_eq!(err.category, ErrorCategory::Policy);

        let bad_env_call = ToolCall::new(
            sample_meta(),
            "read_file".into(),
            Some("bad env/id".into()),
            b"{}".to_vec(),
        );
        let env_err = VerifiedToolEnvironment::from_tool_call(&bad_env_call)
            .expect_err("invalid kernel env id must fail closed");
        assert_eq!(env_err.category, ErrorCategory::Protocol);
        assert_eq!(env_err.code, ERR_INVALID_PARAMS);
    }

    #[test]
    fn anti_spoofing_blocks_nested_and_identity_fields() {
        for payload in [
            br#"{"environment":"host"}"#.as_slice(),
            br#"{"nested":{"environment_id":"prod"}}"#.as_slice(),
            br#"{"items":[{"_meta":{"session":"spoof"}}]}"#.as_slice(),
            br#"{"sessionId":"spoof"}"#.as_slice(),
            br#"{"__host":true}"#.as_slice(),
        ] {
            let err = VerifiedToolEnvironment::reject_reserved_host_fields_bytes(payload)
                .expect_err("spoof payload must be rejected");
            assert_eq!(err.category, ErrorCategory::Policy);
        }

        assert!(
            VerifiedToolEnvironment::reject_reserved_host_fields_bytes(
                br#"{"path":"src/environment.rs","env":{"RUST_LOG":"info"}}"#
            )
            .is_ok()
        );
    }

    #[test]
    fn finalized_tool_call_detects_mutation_and_digest_tampering() {
        let raw = br#"{"secret":"raw"}"#;
        let mutated = br#"{"secret":"[REDACTED]"}"#.to_vec();
        let mut finalized = FinalizedToolCall::new(raw, mutated.clone());
        assert!(finalized.redacted_or_mutated);
        assert_eq!(
            finalized.sha256_hex,
            FinalizedToolCall::sha256_hex(&mutated)
        );
        assert!(finalized.verify_digest().is_ok());

        finalized.args_bytes = br#"{"secret":"tampered"}"#.to_vec();
        let err = finalized
            .verify_digest()
            .expect_err("post-finalization mutation must fail digest check");
        assert_eq!(err.category, ErrorCategory::Policy);

        let unchanged = FinalizedToolCall::new(raw, raw.to_vec());
        assert!(!unchanged.redacted_or_mutated);
    }

    #[test]
    fn governed_nested_invoker_preserves_env_and_rejects_missing_env_or_unknown_tool() {
        use crate::catalog_contract::{CatalogToolContract, CatalogToolFn, VerifiedToolContext};
        use agentcpm_iface::tool::ToolContract;
        use pilotdeck_tool_core::{ToolCatalogEntry, ToolConcurrencyPolicy};
        use std::future::Future;
        use std::pin::pin;
        use std::task::{Context, Poll, Waker};

        fn ready<F: Future>(future: F) -> F::Output {
            let mut future = pin!(future);
            let mut cx = Context::from_waker(Waker::noop());
            match future.as_mut().poll(&mut cx) {
                Poll::Ready(v) => v,
                Poll::Pending => panic!("expected ready future"),
            }
        }

        let observed_calls = Arc::new(Mutex::new(Vec::<(Option<String>, String)>::new()));
        let observed_ref = observed_calls.clone();

        let catalog = Arc::new(CatalogToolContract::new());
        catalog
            .register_tool(
                ToolCatalogEntry {
                    name: "echo_env".into(),
                    aliases: vec![],
                    description: "echoes env".into(),
                    kind: "custom".into(),
                    input_schema: parse_json(r#"{"type":"object","properties":{"x":{"type":"string"}},"additionalProperties":false}"#).unwrap(),
                    concurrency: ToolConcurrencyPolicy::ConcurrentReadOnly,
                },
                Arc::new(CatalogToolFn::new(move |ctx: VerifiedToolContext| {
                    let observed = observed_ref.clone();
                    Box::pin(async move {
                        observed
                            .lock()
                            .unwrap()
                            .push((ctx.environment.environment_id.clone(), ctx.meta.call_id.clone()));
                        Ok(ToolResult {
                            ok: true,
                            bytes: b"{}".to_vec(),
                            unknown: false,
                        })
                    })
                })),
            )
            .unwrap();

        struct RecordingFinalizer;
        impl PreAuthArgumentFinalizer for RecordingFinalizer {
            fn finalize_before_auth(
                &self,
                _tool: &str,
                _env: &VerifiedToolEnvironment,
                raw_args: &[u8],
            ) -> BoxFuture<'_, Result<FinalizedToolCall, Error>> {
                let updated = if raw_args == br#"{"x":"secret"}"# {
                    br#"{"x":"redacted"}"#.to_vec()
                } else {
                    raw_args.to_vec()
                };
                let finalized = FinalizedToolCall::new(raw_args, updated);
                Box::pin(async move { Ok(finalized) })
            }
        }

        let invoker = GovernedNestedToolInvoker::new(catalog.clone() as Arc<dyn ToolContract>)
            .with_finalizer(Arc::new(RecordingFinalizer));

        // Missing parent environment must fail closed without invoking contract.
        let unbound = VerifiedToolEnvironment::from_kernel_environment(None).unwrap();
        let err = ready(invoker.invoke_nested(
            sample_meta(),
            &unbound,
            "echo_env",
            br#"{"x":"ok"}"#.to_vec(),
        ))
        .unwrap_err();
        assert_eq!(err.category, ErrorCategory::Environment);
        assert!(observed_calls.lock().unwrap().is_empty());

        // Unregistered tool must fail closed with -32033.
        let bound = VerifiedToolEnvironment::from_kernel_environment(Some("box-9".into())).unwrap();
        let err =
            ready(invoker.invoke_nested(sample_meta(), &bound, "missing_tool", b"{}".to_vec()))
                .unwrap_err();
        assert_eq!(err.category, ErrorCategory::Execution);
        assert_eq!(err.code, -32033);

        // Registered tool with bound parent env preserves env across ToolCall::new boundary.
        let mut nested_meta = sample_meta();
        nested_meta.call_id = "call-1:code:1".into();
        let result = ready(invoker.invoke_nested(
            nested_meta,
            &bound,
            "echo_env",
            br#"{"x":"secret"}"#.to_vec(),
        ))
        .unwrap();
        assert!(result.ok);
        assert_eq!(
            *observed_calls.lock().unwrap(),
            vec![(Some("box-9".into()), "call-1:code:1".into())]
        );

        // Staged stub tool is also rejected fail-closed at the ToolContract boundary.
        use agentcpm_iface::tool::ToolStaging;
        catalog.register_stub("staged.stub").unwrap();
        let stub_err =
            ready(invoker.invoke_nested(sample_meta(), &bound, "staged.stub", b"{}".to_vec()))
                .unwrap_err();
        assert_eq!(stub_err.category, ErrorCategory::Execution);
        assert_eq!(stub_err.code, -32033);
    }
}
