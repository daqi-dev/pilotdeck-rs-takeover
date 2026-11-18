//! Standard JSON-RPC 2.0 codes, `pd-connect/1` extensions, contract errors, and CLI exit codes.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

// ---- JSON-RPC 2.0 & pd-connect/1 numeric wire error codes (007 §6.7) ----

pub const JSONRPC_PARSE_ERROR: i64 = -32700;
pub const JSONRPC_INVALID_REQUEST: i64 = -32600;
pub const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;
pub const JSONRPC_INVALID_PARAMS: i64 = -32602;
pub const ERR_NOT_INITIALIZED: i64 = -32002;
pub const ERR_CAPABILITY_UNAVAILABLE: i64 = -32004;
pub const ERR_UNSUPPORTED_VERSION: i64 = -32006;
pub const ERR_MISSING_IDEMPOTENCY_KEY: i64 = -32086;
pub const ERR_REQUEST_ABANDONED: i64 = -32800;
pub const ERR_APPLICATION: i64 = -32000;

// ---- CLI exit codes (007 §6.8) ----

pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_RESULT_UNKNOWN: i32 = 3;
pub const EXIT_INTERRUPTED: i32 = 130;

// ---- Registered contract application error codes ----

pub const CODE_INVALID_ARGUMENT: &str = "INVALID_ARGUMENT";
pub const CODE_UNSUPPORTED_OPERATION: &str = "UNSUPPORTED_OPERATION";
pub const CODE_UNAUTHENTICATED: &str = "UNAUTHENTICATED";
pub const CODE_FORBIDDEN: &str = "FORBIDDEN";
pub const CODE_INSTANCE_MISMATCH: &str = "INSTANCE_MISMATCH";
pub const CODE_NOT_FOUND: &str = "NOT_FOUND";
pub const CODE_ALREADY_EXISTS: &str = "ALREADY_EXISTS";
pub const CODE_REVISION_CONFLICT: &str = "REVISION_CONFLICT";
pub const CODE_IDEMPOTENCY_CONFLICT: &str = "IDEMPOTENCY_CONFLICT";
pub const CODE_RESULT_UNKNOWN: &str = "RESULT_UNKNOWN";
pub const CODE_CURSOR_EXPIRED: &str = "CURSOR_EXPIRED";
pub const CODE_SNAPSHOT_CHANGED: &str = "SNAPSHOT_CHANGED";
pub const CODE_CAPABILITY_UNAVAILABLE: &str = "CAPABILITY_UNAVAILABLE";
pub const CODE_CONNECTION_FAILED: &str = "CONNECTION_FAILED";
pub const CODE_QUEUE_PAUSED: &str = "QUEUE_PAUSED";
pub const CODE_CONFIGURATION_REQUIRED: &str = "CONFIGURATION_REQUIRED";
pub const CODE_RESOURCE_LIMIT: &str = "RESOURCE_LIMIT";
pub const CODE_UNAVAILABLE: &str = "UNAVAILABLE";
pub const CODE_CANCELLATION_UNSUPPORTED: &str = "CANCELLATION_UNSUPPORTED";
pub const CODE_RESOURCE_BUSY: &str = "RESOURCE_BUSY";

/// Allowed retry advice modes matching `$defs.retry`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryMode {
    Never,
    SameKey,
    AfterReconcile,
    BackoffRead,
}

/// Structured retry advice matching `$defs.retry`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetrySpec {
    pub mode: RetryMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_ms: Option<u64>,
}

impl RetrySpec {
    pub fn never() -> Self {
        Self {
            mode: RetryMode::Never,
            after_ms: None,
        }
    }

    pub fn same_key(after_ms: Option<u64>) -> Self {
        Self {
            mode: RetryMode::SameKey,
            after_ms,
        }
    }

    pub fn after_reconcile() -> Self {
        Self {
            mode: RetryMode::AfterReconcile,
            after_ms: None,
        }
    }

    pub fn backoff_read(after_ms: Option<u64>) -> Self {
        Self {
            mode: RetryMode::BackoffRead,
            after_ms,
        }
    }
}

/// Side-effect certainty recorded in `$defs.error`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectStatus {
    NotApplied,
    Unknown,
    Applied,
}

/// Public contract error payload matching `$defs.error`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Map<String, Value>>,
    pub retry: RetrySpec,
    pub effect: EffectStatus,
}

impl ContractError {
    /// Builds a contract error, truncating message to the 256-char schema ceiling on char boundary.
    pub fn new(
        code: impl Into<String>,
        message: impl Into<String>,
        retry: RetrySpec,
        effect: EffectStatus,
    ) -> Self {
        Self {
            code: code.into(),
            message: clamp_chars(message.into(), 256),
            details: None,
            retry,
            effect,
        }
    }

    /// Attaches an object details map.
    pub fn with_details(mut self, details: Map<String, Value>) -> Self {
        self.details = Some(details);
        self
    }

    /// Maps common contract error codes to CLI unary/invoke exit codes (`0/1/2/3/130`).
    pub fn suggested_cli_exit_code(&self) -> i32 {
        if self.effect == EffectStatus::Unknown || self.code == CODE_RESULT_UNKNOWN {
            return EXIT_RESULT_UNKNOWN;
        }
        if matches!(
            self.code.as_str(),
            CODE_INVALID_ARGUMENT | CODE_CONFIGURATION_REQUIRED | CODE_UNSUPPORTED_OPERATION
        ) {
            return EXIT_USAGE;
        }
        EXIT_FAILED
    }

    pub fn instance_mismatch(expected: &str, actual: &str) -> Self {
        let mut details = Map::new();
        details.insert("expectedInstanceId".into(), json!(expected));
        details.insert("actualInstanceId".into(), json!(actual));
        Self::new(
            CODE_INSTANCE_MISMATCH,
            format!("instanceId mismatch: bound to '{expected}', got '{actual}'"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
        .with_details(details)
    }

    pub fn capability_unavailable(method: &str, reason: &str) -> Self {
        let mut details = Map::new();
        details.insert("methodId".into(), json!(method));
        details.insert("reason".into(), json!(reason));
        Self::new(
            CODE_CAPABILITY_UNAVAILABLE,
            format!("capability unavailable for {method}: {reason}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
        .with_details(details)
    }

    pub fn result_unknown(idempotency_key: Option<&str>, message: impl Into<String>) -> Self {
        let mut details = Map::new();
        if let Some(key) = idempotency_key {
            details.insert("idempotencyKey".into(), json!(key));
        }
        let mut err = Self::new(
            CODE_RESULT_UNKNOWN,
            message,
            RetrySpec::after_reconcile(),
            EffectStatus::Unknown,
        );
        if !details.is_empty() {
            err.details = Some(details);
        }
        err
    }
}

fn clamp_chars(mut s: String, max_chars: usize) -> String {
    if s.is_empty() {
        return "error".to_owned();
    }
    if s.chars().count() > max_chars {
        let end = s
            .char_indices()
            .nth(max_chars)
            .map(|(idx, _)| idx)
            .unwrap_or(s.len());
        s.truncate(end);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_error_serializes_matching_public_schema() {
        let err = ContractError::instance_mismatch("local-1", "local-2");
        let val = serde_json::to_value(&err).unwrap();
        assert_eq!(val["code"], "INSTANCE_MISMATCH");
        assert_eq!(val["effect"], "not_applied");
        assert_eq!(val["retry"]["mode"], "never");
        assert!(val["details"]["expectedInstanceId"] == "local-1");
        assert_eq!(err.suggested_cli_exit_code(), EXIT_FAILED);
        assert_eq!(
            ContractError::result_unknown(Some("k1"), "timeout").suggested_cli_exit_code(),
            EXIT_RESULT_UNKNOWN
        );
    }
}
