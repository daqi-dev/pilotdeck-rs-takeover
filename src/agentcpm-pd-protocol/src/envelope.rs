//! Deterministic mapping between `OperationRequest`/`OperationResponse` and `pd-connect/1` JSON-RPC (`007 §6.3`).

#[cfg(test)]
use serde_json::Map;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::contract::{
    OPERATION_VERSION, OperationRequest, OperationResponse, ResponseOutcome, Target,
};
use crate::errors::{ContractError, EffectStatus, RetrySpec};
use crate::wire::{
    CONTROL_METHODS, DEFAULT_UNARY_TIMEOUT_MS, JsonRpcRequest, JsonRpcResponse, OperationRpcParams,
    OperationRpcResult, STREAM_METHODS,
};

/// Returns whether `method_id` uses the high-priority control queue.
pub fn is_control_method(method_id: &str) -> bool {
    CONTROL_METHODS.contains(&method_id)
}

/// Returns whether `method_id` supports `sub.open` streaming.
pub fn is_stream_method(method_id: &str) -> bool {
    STREAM_METHODS.contains(&method_id)
}

/// Computes timeout duration in ms (`007 §6.3`: `timeoutMs + 5s`, else `30s` default; subscriptions have no timeout).
pub fn unary_timeout_ms(input: &Value) -> u64 {
    if let Some(ms) = input.get("timeoutMs").and_then(Value::as_u64) {
        ms.saturating_add(5_000)
    } else {
        DEFAULT_UNARY_TIMEOUT_MS
    }
}

/// Conditional `instanceId` injection (`007 v0.7 §5, §6.3`):
///
/// - When `declares_instance_id == true`, injects `bound_instance_id` if absent, or verifies strict
///   equality and rejects with `INSTANCE_MISMATCH` (`EffectStatus::NotApplied`) before side effects.
/// - When `declares_instance_id == false`, never mutates `input` with an undeclared `instanceId`
///   property (preserving `additionalProperties: false`).
pub fn bind_instance_id(
    input: &mut Value,
    bound_instance_id: &str,
    declares_instance_id: bool,
) -> Result<(), ContractError> {
    let Some(obj) = input.as_object_mut() else {
        return Err(ContractError::new(
            crate::errors::CODE_INVALID_ARGUMENT,
            "params.input must be a JSON object",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    };

    if declares_instance_id {
        if let Some(existing) = obj.get("instanceId") {
            let Some(actual) = existing.as_str() else {
                return Err(ContractError::new(
                    crate::errors::CODE_INVALID_ARGUMENT,
                    "params.input.instanceId must be a string",
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            };
            if actual != bound_instance_id {
                return Err(ContractError::instance_mismatch(bound_instance_id, actual));
            }
        } else {
            obj.insert("instanceId".into(), json!(bound_instance_id));
        }
    } else if let Some(actual) = obj.get("instanceId").and_then(Value::as_str)
        && actual != bound_instance_id
    {
        return Err(ContractError::instance_mismatch(bound_instance_id, actual));
    }

    Ok(())
}

/// Converts a JSON-RPC request and connection-bound `instance_id` into a canonical `OperationRequest`.
pub fn rpc_to_operation_request(
    rpc: &JsonRpcRequest,
    bound_instance_id: &str,
    declares_instance_id: bool,
) -> Result<OperationRequest, ContractError> {
    let parsed: OperationRpcParams = serde_json::from_value(rpc.params.clone()).map_err(|e| {
        ContractError::new(
            crate::errors::CODE_INVALID_ARGUMENT,
            format!("invalid RPC params for {}: {e}", rpc.method),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;

    let mut input = parsed.input;
    bind_instance_id(&mut input, bound_instance_id, declares_instance_id)?;
    let params = input.as_object().cloned().unwrap_or_default();

    Ok(OperationRequest {
        version: OPERATION_VERSION.to_owned(),
        request_id: rpc.id.clone(),
        method_id: rpc.method.clone(),
        target: Target {
            instance_id: bound_instance_id.to_owned(),
            project_id: params
                .get("projectId")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            session_id: params
                .get("sessionId")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            run_id: params
                .get("runId")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            interaction_id: params
                .get("interactionId")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            operation_id: params
                .get("operationId")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        },
        params,
        idempotency_key: parsed.idempotency_key,
        expected_revision: parsed.expected_revision,
    })
}

/// Converts an `OperationRequest` to a JSON-RPC request frame.
pub fn operation_request_to_rpc(req: &OperationRequest) -> JsonRpcRequest {
    let params = OperationRpcParams {
        input: Value::Object(req.params.clone()),
        idempotency_key: req.idempotency_key.clone(),
        expected_revision: req.expected_revision.clone(),
    };
    JsonRpcRequest {
        jsonrpc: crate::wire::JSONRPC_VERSION.to_owned(),
        id: req.request_id.clone(),
        method: req.method_id.clone(),
        params: serde_json::to_value(params).unwrap_or(Value::Null),
    }
}

/// Converts a canonical `OperationResponse` into a JSON-RPC response frame.
pub fn operation_response_to_rpc(resp: &OperationResponse) -> JsonRpcResponse {
    match resp.outcome {
        ResponseOutcome::Succeeded | ResponseOutcome::Accepted => {
            let rpc_result = OperationRpcResult {
                outcome: resp.outcome,
                result: resp.result.clone().unwrap_or(Value::Null),
            };
            JsonRpcResponse::ok(
                resp.request_id.clone(),
                serde_json::to_value(rpc_result).unwrap_or(Value::Null),
            )
        }
        ResponseOutcome::Failed => {
            if let Some(err) = &resp.error {
                JsonRpcResponse::application_error(resp.request_id.clone(), err)
            } else {
                JsonRpcResponse::application_error(
                    resp.request_id.clone(),
                    &ContractError::new(
                        crate::errors::CODE_UNAVAILABLE,
                        "operation failed without structured error",
                        RetrySpec::never(),
                        EffectStatus::Unknown,
                    ),
                )
            }
        }
    }
}

/// Computes a deterministic canonical SHA-256 hex digest for `(method_id, input)` used by idempotency conflict checks.
pub fn compute_input_digest(method_id: &str, input: &Value) -> String {
    let canonical = canonicalize_json(input);
    let mut hasher = Sha256::new();
    hasher.update(method_id.as_bytes());
    hasher.update([0u8]);
    hasher.update(canonical.as_bytes());
    format!("{:x}", hasher.finalize())
}

const MAX_CANONICAL_JSON_DEPTH: usize = 256;

fn canonicalize_json(val: &Value) -> String {
    enum Frame<'a> {
        Value(&'a Value, usize),
        Key(String),
        Literal(&'static str),
    }

    let mut out = String::new();
    let mut stack = vec![Frame::Value(val, 0)];

    while let Some(frame) = stack.pop() {
        match frame {
            Frame::Literal(lit) => out.push_str(lit),
            Frame::Key(key_json) => {
                out.push_str(&key_json);
                out.push(':');
            }
            Frame::Value(cur, depth) => {
                if depth > MAX_CANONICAL_JSON_DEPTH {
                    out.push_str("\"__depth_exceeded__\"");
                    continue;
                }
                match cur {
                    Value::Object(map) => {
                        out.push('{');
                        if map.is_empty() {
                            out.push('}');
                            continue;
                        }
                        let mut entries: Vec<(&String, &Value)> = map.iter().collect();
                        entries.sort_by_key(|(a, _)| *a);
                        stack.push(Frame::Literal("}"));
                        for (idx, (k, v)) in entries.into_iter().enumerate().rev() {
                            stack.push(Frame::Value(v, depth + 1));
                            let key_json =
                                serde_json::to_string(k).unwrap_or_else(|_| "\"\"".to_owned());
                            stack.push(Frame::Key(key_json));
                            if idx > 0 {
                                stack.push(Frame::Literal(","));
                            }
                        }
                    }
                    Value::Array(arr) => {
                        out.push('[');
                        if arr.is_empty() {
                            out.push(']');
                            continue;
                        }
                        stack.push(Frame::Literal("]"));
                        for (idx, item) in arr.iter().enumerate().rev() {
                            stack.push(Frame::Value(item, depth + 1));
                            if idx > 0 {
                                stack.push(Frame::Literal(","));
                            }
                        }
                    }
                    _ => {
                        let prim = serde_json::to_string(cur).unwrap_or_else(|_| "null".to_owned());
                        out.push_str(&prim);
                    }
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conditional_instance_id_injection_and_mismatch_rejection() {
        let mut declared = json!({"sessionId": "s1"});
        assert!(bind_instance_id(&mut declared, "local-1", true).is_ok());
        assert_eq!(declared["instanceId"], "local-1");

        let mut mismatch = json!({"instanceId": "local-99", "sessionId": "s1"});
        let err = bind_instance_id(&mut mismatch, "local-1", true).unwrap_err();
        assert_eq!(err.code, "INSTANCE_MISMATCH");
        assert_eq!(err.effect, EffectStatus::NotApplied);

        let mut undeclared = json!({"methodId": "run.submit"});
        assert!(bind_instance_id(&mut undeclared, "local-1", false).is_ok());
        assert!(undeclared.get("instanceId").is_none());
    }

    #[test]
    fn canonical_digest_is_order_independent() {
        let a = json!({"b": 2, "a": {"y": 1, "x": 2}});
        let b = json!({"a": {"x": 2, "y": 1}, "b": 2});
        assert_eq!(
            compute_input_digest("run.submit", &a),
            compute_input_digest("run.submit", &b)
        );
        assert_ne!(
            compute_input_digest("run.submit", &a),
            compute_input_digest("run.cancel", &a)
        );
    }

    #[test]
    fn canonical_digest_handles_deep_nesting_in_linear_time() {
        // 60 layers of nested single-key objects would have triggered 2^60 calls before.
        let mut cur = json!({"leaf": [1, {"k": "v"}]});
        for i in 0..60 {
            let mut m = Map::new();
            m.insert(format!("k{i}"), cur);
            m.insert("z".to_owned(), json!(i));
            cur = Value::Object(m);
        }
        let d1 = compute_input_digest("tools.test", &cur);
        assert_eq!(d1.len(), 64);

        // Exceeding MAX_CANONICAL_JSON_DEPTH terminates iteratively without stack overflow.
        let mut over = json!(1);
        for _ in 0..300 {
            over = json!({"n": over});
        }
        let d2 = compute_input_digest("tools.test", &over);
        assert_eq!(d2.len(), 64);
    }
}
