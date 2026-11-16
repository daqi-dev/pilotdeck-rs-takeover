//! `pd-connect/1` long-lived transport wire protocol (007 §6).
//!
//! Shared identically by host <-> CLI (`stdio` NDJSON) and CLI <-> Gateway (`/v1/ws` WebSocket Text frames).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::contract::{EventEnvelope, GapFrame};
use crate::errors::ContractError;

pub const JSONRPC_VERSION: &str = "2.0";
pub const PROTOCOL_NAME: &str = "pd-connect/1";
pub const ABI_VERSION: u32 = 1;

// ---- Fixed protocol limits from `007 §6` & `transportContract.limits` ----

pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
pub const MAX_REQUEST_ID_LEN: usize = 128;
pub const MAX_PENDING: usize = 64;
pub const MAX_CONTROL_PENDING: usize = 16;
pub const MAX_SUBSCRIPTIONS: usize = 64;
pub const DEFAULT_CREDIT: u32 = 256;
pub const SUBSCRIPTION_BUFFER_ITEMS: usize = 1024;
pub const SUBSCRIPTION_BUFFER_BYTES: usize = 4 * 1024 * 1024;
pub const DEFAULT_HEARTBEAT_MS: u64 = 5_000;
pub const DEFAULT_UNARY_TIMEOUT_MS: u64 = 30_000;
pub const EOF_DRAIN_MS: u64 = 5_000;

/// Control-channel method names (`007 §6.3`, 11 operations).
pub const CONTROL_METHODS: [&str; 11] = [
    "run.cancel",
    "operations.cancel",
    "interaction.respond",
    "approval.respond",
    "requests.lookup",
    "run.snapshot",
    "operations.snapshot",
    "run.get",
    "operations.get",
    "interaction.get",
    "interaction.list",
];

/// Streaming method names (`007 §6.4`, 6 operations).
pub const STREAM_METHODS: [&str; 6] = [
    "run.events",
    "operations.events",
    "terminals.output",
    "resources.watch",
    "host.stream_model",
    "notifications.events",
];

/// Wire method names reserved for `pd-connect/1` connection control / subscriptions.
pub const WIRE_INITIALIZE: &str = "initialize";
pub const WIRE_SUB_OPEN: &str = "sub.open";
pub const WIRE_SUB_CREDIT: &str = "sub.credit";
pub const WIRE_SUB_CLOSE: &str = "sub.close";
pub const WIRE_SUB_EVENT: &str = "sub.event";
pub const WIRE_SUB_END: &str = "sub.end";
pub const WIRE_CONN_ABANDON: &str = "conn.abandon";
pub const WIRE_CONN_HEARTBEAT: &str = "conn.heartbeat";
pub const WIRE_CONN_STATE: &str = "conn.state";

/// Validates a JSON-RPC request ID (`pd-connect/1`: string, non-empty, <= 128 chars).
pub fn validate_request_id(id: &str) -> Result<(), &'static str> {
    if id.is_empty() || id.len() > MAX_REQUEST_ID_LEN {
        return Err("request id must be 1..=128 bytes");
    }
    Ok(())
}

/// JSON-RPC 2.0 request frame (`id` is always a validated non-null string).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// JSON-RPC 2.0 notification frame (no `id`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// JSON-RPC 2.0 error object (`-32000` carries `ContractError` inside `data`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// JSON-RPC 2.0 response frame (`id` may be `None` / `null` on parse/frame errors).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcErrorObject>,
}

impl JsonRpcResponse {
    pub fn ok(id: impl Into<String>, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: Some(id.into()),
            result: Some(result),
            error: None,
        }
    }

    pub fn err(
        id: Option<String>,
        code: i64,
        message: impl Into<String>,
        data: Option<Value>,
    ) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id,
            result: None,
            error: Some(JsonRpcErrorObject {
                code,
                message: message.into(),
                data,
            }),
        }
    }

    pub fn application_error(id: impl Into<String>, err: &ContractError) -> Self {
        Self::err(
            Some(id.into()),
            crate::errors::ERR_APPLICATION,
            err.message.clone(),
            Some(serde_json::to_value(err).unwrap_or(Value::Null)),
        )
    }
}

/// Parsed inbound JSON-RPC 2.0 frame.
#[derive(Clone, Debug, PartialEq)]
pub enum JsonRpcFrame {
    Request(JsonRpcRequest),
    Notification(JsonRpcNotification),
    Response(JsonRpcResponse),
}

/// Client descriptor sent in `initialize.params.client`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientDescriptor {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

/// `initialize` request parameters (`007 §6.2`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializeParams {
    pub abi_version: u32,
    pub api_version: String,
    #[serde(default)]
    pub features: Vec<String>,
    pub client: ClientDescriptor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

/// Local CLI binary provenance returned on stdio `initialize` response (`cli`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliBuildInfo {
    pub version: String,
    pub build: String,
    pub pid: u32,
    pub executable: String,
}

/// Connected target summary returned on `initialize` response (`target`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetHandshakeInfo {
    pub name: String,
    pub instance_id: String,
    pub node_id: String,
    pub gateway_version: String,
    pub schema_digest: String,
    pub transport: String,
}

/// Negotiated protocol limits returned inside `initialize` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolLimits {
    pub max_frame_bytes: usize,
    pub max_pending: usize,
    pub max_control_pending: usize,
    pub max_subscriptions: usize,
    pub default_credit: u32,
    pub subscription_buffer_items: usize,
    pub subscription_buffer_bytes: usize,
    pub heartbeat_ms: u64,
    pub unary_timeout_ms: u64,
    pub eof_drain_ms: u64,
}

impl Default for ProtocolLimits {
    fn default() -> Self {
        Self {
            max_frame_bytes: MAX_FRAME_BYTES,
            max_pending: MAX_PENDING,
            max_control_pending: MAX_CONTROL_PENDING,
            max_subscriptions: MAX_SUBSCRIPTIONS,
            default_credit: DEFAULT_CREDIT,
            subscription_buffer_items: SUBSCRIPTION_BUFFER_ITEMS,
            subscription_buffer_bytes: SUBSCRIPTION_BUFFER_BYTES,
            heartbeat_ms: DEFAULT_HEARTBEAT_MS,
            unary_timeout_ms: DEFAULT_UNARY_TIMEOUT_MS,
            eof_drain_ms: EOF_DRAIN_MS,
        }
    }
}

/// `initialize` response payload (`007 §6.2`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializeResult {
    pub abi_version: u32,
    pub supported: bool,
    pub api_version: String,
    pub supported_features: Vec<String>,
    #[serde(rename = "connectionId")]
    pub connection_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli: Option<CliBuildInfo>,
    pub target: TargetHandshakeInfo,
    pub limits: ProtocolLimits,
}

/// Business RPC call params on `pd-connect/1` (`007 §6.3`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationRpcParams {
    pub input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<String>,
}

/// Business RPC success/accepted result on `pd-connect/1` (`007 §6.3`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OperationRpcResult {
    pub outcome: crate::contract::ResponseOutcome,
    pub result: Value,
}

/// `sub.open` request params.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubOpenParams {
    pub stream: String,
    pub input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credit: Option<u32>,
}

/// `sub.open` response payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubOpenResult {
    pub subscription_id: String,
}

/// `sub.credit` request/notification params.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubCreditParams {
    pub subscription_id: String,
    pub add: u32,
}

/// `sub.close` request params.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubCloseParams {
    pub subscription_id: String,
}

/// `sub.event` notification payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubEventNotification {
    pub subscription_id: String,
    pub event: EventEnvelope,
}

/// Explicit subscription terminal reason (`007 §6.4`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubTerminal {
    Completed,
    Closed,
    SlowConsumer,
    Gap,
    Disconnected,
    Shutdown,
    Error,
}

/// `sub.end` notification payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubEndNotification {
    pub subscription_id: String,
    pub terminal: SubTerminal,
    pub resume_cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap: Option<GapFrame>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ContractError>,
}

/// `conn.abandon` params (stdio only, `007 §6.5`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnAbandonParams {
    pub id: String,
}

/// `conn.heartbeat` notification params.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnHeartbeatNotification {
    pub ts_ms: u64,
}

/// Connection health transitions surfaced over stdio.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnStateKind {
    Connected,
    Reconnecting,
    Failed,
}

/// `conn.state` notification params.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnStateNotification {
    pub state: ConnStateKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_end_terminals_and_limits_match_007() {
        let end = SubEndNotification {
            subscription_id: "sub-1".into(),
            terminal: SubTerminal::SlowConsumer,
            resume_cursor: Some("cur-9".into()),
            gap: None,
            error: None,
        };
        let v = serde_json::to_value(&end).unwrap();
        assert_eq!(v["terminal"], "slow_consumer");
        assert_eq!(v["subscriptionId"], "sub-1");
        assert_eq!(v["resumeCursor"], "cur-9");
        assert_eq!(ProtocolLimits::default().default_credit, 256);
    }
}
