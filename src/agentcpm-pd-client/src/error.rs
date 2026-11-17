//! Client error normalization and deterministic CLI exit code mapping (`007 §6.7–§6.8`).

use std::fmt;

use agentcpm_pd_protocol::errors::{
    CODE_CONNECTION_FAILED, CODE_FORBIDDEN, CODE_IDEMPOTENCY_CONFLICT, CODE_INSTANCE_MISMATCH,
    CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_RESULT_UNKNOWN, CODE_UNAUTHENTICATED,
    CODE_UNAVAILABLE, ContractError, ERR_CAPABILITY_UNAVAILABLE, ERR_MISSING_IDEMPOTENCY_KEY,
    ERR_NOT_INITIALIZED, ERR_UNSUPPORTED_VERSION, EXIT_FAILED, EXIT_INTERRUPTED,
    EXIT_RESULT_UNKNOWN, EXIT_USAGE, EffectStatus, JSONRPC_INVALID_PARAMS,
    JSONRPC_METHOD_NOT_FOUND, RetrySpec,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientError {
    Usage(String),
    Config(String),
    NotInitialized(String),
    ConnectionFailed(String),
    ProtocolError { code: i64, message: String },
    Contract(ContractError),
    InstanceChanged { expected: String, actual: String },
    Interrupted(String),
}

impl ClientError {
    pub fn usage(msg: impl Into<String>) -> Self {
        Self::Usage(msg.into())
    }

    pub fn config(msg: impl Into<String>) -> Self {
        Self::Config(msg.into())
    }

    pub fn not_initialized(msg: impl Into<String>) -> Self {
        Self::NotInitialized(msg.into())
    }

    pub fn connection_failed(msg: impl Into<String>) -> Self {
        Self::ConnectionFailed(msg.into())
    }

    pub fn suggested_exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) => EXIT_USAGE,
            Self::Interrupted(_) => EXIT_INTERRUPTED,
            Self::Contract(err) => err.suggested_cli_exit_code(),
            Self::Config(_)
            | Self::NotInitialized(_)
            | Self::ConnectionFailed(_)
            | Self::ProtocolError { .. }
            | Self::InstanceChanged { .. } => EXIT_FAILED,
        }
    }

    pub fn is_result_unknown(&self) -> bool {
        matches!(self, Self::Contract(c) if c.code == CODE_RESULT_UNKNOWN || c.effect == EffectStatus::Unknown)
    }

    pub fn into_contract_error(&self) -> ContractError {
        match self {
            Self::Usage(msg) => ContractError::new(
                CODE_INVALID_ARGUMENT,
                msg.clone(),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ),
            Self::Config(msg) => ContractError::new(
                CODE_NOT_FOUND,
                msg.clone(),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ),
            Self::NotInitialized(msg) => ContractError::new(
                agentcpm_pd_protocol::errors::CODE_CONFIGURATION_REQUIRED,
                msg.clone(),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ),
            Self::ConnectionFailed(msg) => ContractError::new(
                CODE_CONNECTION_FAILED,
                msg.clone(),
                RetrySpec::backoff_read(Some(500)),
                EffectStatus::NotApplied,
            ),
            Self::ProtocolError { code, message } => {
                let contract_code = match *code {
                    JSONRPC_INVALID_PARAMS => CODE_INVALID_ARGUMENT,
                    JSONRPC_METHOD_NOT_FOUND => {
                        agentcpm_pd_protocol::errors::CODE_UNSUPPORTED_OPERATION
                    }
                    ERR_CAPABILITY_UNAVAILABLE => {
                        agentcpm_pd_protocol::errors::CODE_CAPABILITY_UNAVAILABLE
                    }
                    ERR_MISSING_IDEMPOTENCY_KEY => CODE_IDEMPOTENCY_CONFLICT,
                    ERR_NOT_INITIALIZED | ERR_UNSUPPORTED_VERSION => CODE_FORBIDDEN,
                    _ => CODE_UNAVAILABLE,
                };
                ContractError::new(
                    contract_code,
                    message.clone(),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )
            }
            Self::Contract(c) => c.clone(),
            Self::InstanceChanged { expected, actual } => {
                ContractError::instance_mismatch(expected, actual)
            }
            Self::Interrupted(msg) => ContractError::new(
                CODE_UNAVAILABLE,
                msg.clone(),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ),
        }
    }

    pub fn from_ws_auth_or_connect(msg: impl Into<String>) -> Self {
        let s = msg.into();
        if s.contains("401") || s.contains("403") {
            Self::Contract(ContractError::new(
                CODE_UNAUTHENTICATED,
                s,
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ))
        } else {
            Self::ConnectionFailed(s)
        }
    }

    pub fn exit_code_for_outcome(&self) -> i32 {
        if self.is_result_unknown() {
            EXIT_RESULT_UNKNOWN
        } else {
            self.suggested_exit_code()
        }
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(msg) => write!(f, "usage error: {msg}"),
            Self::Config(msg) => write!(f, "configuration error: {msg}"),
            Self::NotInitialized(msg) => write!(f, "instance not initialized: {msg}"),
            Self::ConnectionFailed(msg) => write!(f, "connection failed: {msg}"),
            Self::ProtocolError { code, message } => {
                write!(f, "protocol error ({code}): {message}")
            }
            Self::Contract(c) => write!(f, "{}: {}", c.code, c.message),
            Self::InstanceChanged { expected, actual } => write!(
                f,
                "{CODE_INSTANCE_MISMATCH}: expected instance `{expected}`, got `{actual}`"
            ),
            Self::Interrupted(msg) => write!(f, "interrupted: {msg}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<ContractError> for ClientError {
    fn from(value: ContractError) -> Self {
        Self::Contract(value)
    }
}

/// Extracts a contract error from an application-error (`-32000`) data
/// payload. Accepts both the full `$defs.error` object and the minimal
/// `{code, message, details?}` shape some gateway replies use (e.g. sub.open
/// rejections answering `CURSOR_EXPIRED` with `details.gap`), so registered
/// codes surface as themselves instead of falling through to `UNAVAILABLE`.
pub fn contract_error_from_rpc_data(data: &serde_json::Value) -> Option<ContractError> {
    if let Ok(full) = serde_json::from_value::<ContractError>(data.clone()) {
        return Some(full);
    }
    let code = data.get("code").and_then(serde_json::Value::as_str)?;
    let message = data
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let mut contract = ContractError::new(
        code.to_owned(),
        if message.is_empty() {
            code.to_owned()
        } else {
            message.to_owned()
        },
        RetrySpec::never(),
        EffectStatus::NotApplied,
    );
    if let Some(details) = data.get("details").and_then(|d| d.as_object().cloned()) {
        contract = contract.with_details(details);
    }
    Some(contract)
}
