//! `schema.*` — read-only introspection served from the embedded registry
//! (identical answers over the CLI and `/v1/ws`).

use std::io::Write;

use serde_json::Value;
use sha2::{Digest, Sha256};

use agentcpm_pd_protocol::contract::OPERATION_VERSION;
use agentcpm_pd_protocol::errors::ContractError;
use agentcpm_pd_protocol::registry::{OperationRegistry, validation_errors_to_contract_error};

use crate::commands::{local_request_id, local_response, write_envelope_or_fail, write_error};
use crate::local_ops::LocalOpRegistry;
use crate::{OutputFormat, capability_unavailable, usage_contract_error};

pub(super) fn register(reg: &mut LocalOpRegistry) {
    reg.op("schema.list", |_globals, input, format, out| {
        Box::pin(async move { schema_operation("schema.list", input, format, out) })
    });
    reg.op("schema.get", |_globals, input, format, out| {
        Box::pin(async move { schema_operation("schema.get", input, format, out) })
    });
    reg.op("schema.export", |_globals, input, format, out| {
        Box::pin(async move { schema_operation("schema.export", input, format, out) })
    });
}

fn schema_operation(
    method_id: &str,
    input: &Value,
    format: OutputFormat,
    out: &mut dyn Write,
) -> i32 {
    let reg = OperationRegistry::global();
    let result = match method_id {
        "schema.list" => reg.handle_schema_list(input),
        "schema.get" => reg.handle_schema_get(input),
        "schema.export" => local_schema_export(reg, input),
        other => {
            let err = capability_unavailable(other, "unknown local schema operation");
            return write_error(out, format, &err);
        }
    };
    match result {
        Ok(value) => {
            write_envelope_or_fail(out, format, &local_response(local_request_id(), value))
        }
        Err(err) => {
            let client_err = agentcpm_pd_client::error::ClientError::Contract(err);
            write_error(out, format, &client_err)
        }
    }
}

/// `schema.export` served from the embedded public contract.
fn local_schema_export(reg: &OperationRegistry, input: &Value) -> Result<Value, ContractError> {
    reg.validate_input("schema.export", input)
        .map_err(validation_errors_to_contract_error)?;
    if let Some(version) = input.get("schemaVersion").and_then(Value::as_str)
        && version != OPERATION_VERSION
    {
        return Err(usage_contract_error(&format!(
            "unsupported schemaVersion `{version}`"
        )));
    }
    let document = reg.public_contract();
    let canonical = serde_json::to_string(document).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    Ok(serde_json::json!({
        "schemaVersion": OPERATION_VERSION,
        "canonicalSha256": format!("{:x}", hasher.finalize()),
        "document": document,
    }))
}
