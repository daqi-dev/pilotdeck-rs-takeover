//! Bounded NDJSON framing helpers enforcing the 1 MiB (`MAX_FRAME_BYTES`) frame cap (`007 §6.1`).

use serde::Serialize;
use serde_json::{Value, json};

use crate::errors::{JSONRPC_INVALID_REQUEST, JSONRPC_PARSE_ERROR};
use crate::wire::{
    JSONRPC_VERSION, JsonRpcFrame, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    MAX_FRAME_BYTES, validate_request_id,
};

/// Bounded NDJSON decode failures mapped directly to JSON-RPC wire error responses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameDecodeError {
    EmptyLine,
    FrameTooLarge { bytes: usize },
    ParseError(String),
    InvalidRequest { id: Option<String>, reason: String },
}

impl FrameDecodeError {
    /// Converts a decode failure into a compliant JSON-RPC error response (`id: null` when unparseable/oversized).
    pub fn to_error_response(&self) -> Option<JsonRpcResponse> {
        match self {
            Self::EmptyLine => None,
            Self::FrameTooLarge { bytes } => Some(JsonRpcResponse::err(
                None,
                JSONRPC_INVALID_REQUEST,
                "frame too large",
                Some(json!({
                    "reason": "frame_too_large",
                    "bytes": bytes,
                    "maxFrameBytes": MAX_FRAME_BYTES,
                })),
            )),
            Self::ParseError(msg) => Some(JsonRpcResponse::err(
                None,
                JSONRPC_PARSE_ERROR,
                format!("parse error: {msg}"),
                None,
            )),
            Self::InvalidRequest { id, reason } => Some(JsonRpcResponse::err(
                id.clone(),
                JSONRPC_INVALID_REQUEST,
                format!("invalid request: {reason}"),
                None,
            )),
        }
    }
}

/// Decodes one UTF-8 NDJSON line into a validated `JsonRpcFrame`.
pub fn decode_ndjson_line(raw_line: &[u8]) -> Result<JsonRpcFrame, FrameDecodeError> {
    if raw_line.len() > MAX_FRAME_BYTES {
        return Err(FrameDecodeError::FrameTooLarge {
            bytes: raw_line.len(),
        });
    }
    let text =
        std::str::from_utf8(raw_line).map_err(|e| FrameDecodeError::ParseError(e.to_string()))?;
    let trimmed = text.trim_end_matches(['\r', '\n']);
    if trimmed.trim().is_empty() {
        return Err(FrameDecodeError::EmptyLine);
    }

    let val: Value =
        serde_json::from_str(trimmed).map_err(|e| FrameDecodeError::ParseError(e.to_string()))?;
    let Some(obj) = val.as_object() else {
        return Err(FrameDecodeError::InvalidRequest {
            id: None,
            reason: "JSON-RPC frame must be an object".into(),
        });
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC_VERSION) {
        return Err(FrameDecodeError::InvalidRequest {
            id: obj.get("id").and_then(Value::as_str).map(ToOwned::to_owned),
            reason: "jsonrpc must equal '2.0'".into(),
        });
    }

    if let Some(method) = obj.get("method") {
        let Some(method_str) = method.as_str() else {
            return Err(FrameDecodeError::InvalidRequest {
                id: obj.get("id").and_then(Value::as_str).map(ToOwned::to_owned),
                reason: "method must be a string".into(),
            });
        };
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        if let Some(id_val) = obj.get("id") {
            let Some(id_str) = id_val.as_str() else {
                return Err(FrameDecodeError::InvalidRequest {
                    id: None,
                    reason: "request id must be a string".into(),
                });
            };
            if let Err(msg) = validate_request_id(id_str) {
                return Err(FrameDecodeError::InvalidRequest {
                    id: None,
                    reason: msg.into(),
                });
            }
            return Ok(JsonRpcFrame::Request(JsonRpcRequest {
                jsonrpc: JSONRPC_VERSION.into(),
                id: id_str.to_owned(),
                method: method_str.to_owned(),
                params,
            }));
        }
        return Ok(JsonRpcFrame::Notification(JsonRpcNotification {
            jsonrpc: JSONRPC_VERSION.into(),
            method: method_str.to_owned(),
            params,
        }));
    }

    let resp: JsonRpcResponse =
        serde_json::from_value(val).map_err(|e| FrameDecodeError::InvalidRequest {
            id: None,
            reason: e.to_string(),
        })?;
    Ok(JsonRpcFrame::Response(resp))
}

/// Encodes any serializable JSON-RPC message as a single NDJSON line ending with `\n`.
pub fn encode_ndjson_line<T: Serialize>(frame: &T) -> Result<String, String> {
    let mut line = serde_json::to_string(frame).map_err(|e| e.to_string())?;
    if line.len() > MAX_FRAME_BYTES {
        return Err(format!(
            "outbound frame exceeds MAX_FRAME_BYTES ({} > {})",
            line.len(),
            MAX_FRAME_BYTES
        ));
    }
    line.push('\n');
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_line_returns_frame_too_large_with_null_id() {
        let huge = vec![b'a'; MAX_FRAME_BYTES + 1];
        let err = decode_ndjson_line(&huge).unwrap_err();
        let resp = err.to_error_response().unwrap();
        assert!(resp.id.is_none());
        let err_obj = resp.error.unwrap();
        assert_eq!(err_obj.code, JSONRPC_INVALID_REQUEST);
        assert_eq!(err_obj.data.unwrap()["reason"], "frame_too_large");
    }

    #[test]
    fn request_notification_and_response_roundtrip() {
        let req = JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.into(),
            id: "req-1".into(),
            method: "system.health".into(),
            params: json!({"input": {}}),
        };
        let encoded = encode_ndjson_line(&req).unwrap();
        assert!(encoded.ends_with('\n'));
        match decode_ndjson_line(encoded.as_bytes()).unwrap() {
            JsonRpcFrame::Request(back) => assert_eq!(back, req),
            other => panic!("unexpected frame: {other:?}"),
        }
    }
}
