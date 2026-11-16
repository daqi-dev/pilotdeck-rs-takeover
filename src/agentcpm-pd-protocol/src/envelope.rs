//! Deterministic mapping between `OperationRequest`/`OperationResponse` and `pd-connect/1` JSON-RPC (`007 §6.3`).

use serde_json::{Map, Value, json};
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

fn canonicalize_json(val: &Value) -> String {
    match val {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by_key(|(a, _)| *a);
            let mut out = Map::with_capacity(entries.len());
            for (k, v) in entries {
                // Re-serialize children deterministically below.
                let child_str = canonicalize_json(v);
                out.insert(
                    k.clone(),
                    serde_json::from_str(&child_str).unwrap_or(Value::Null),
                );
            }
            let parts: Vec<String> = out
                .iter()
                .map(|(k, _)| {
                    let key_json = serde_json::to_string(k).unwrap_or_else(|_| "\"\"".into());
                    let val_json = canonicalize_json(&map[k]);
                    format!("{key_json}:{val_json}")
                })
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(arr) => {
            let parts: Vec<String> = arr.iter().map(canonicalize_json).collect();
            format!("[{}]", parts.join(","))
        }
        _ => serde_json::to_string(val).unwrap_or_else(|_| "null".to_owned()),
    }
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
}
