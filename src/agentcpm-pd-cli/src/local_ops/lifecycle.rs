//! `instance.initialize` and `service.*` — the CLI-local lifecycle
//! operations, delegating to the Gateway `public_v1::bootstrap` library when
//! the `embedded-gateway` feature is enabled.

use std::io::Write;

use serde_json::{Value, json};

use agentcpm_pd_client::error::ClientError;

use crate::commands::{local_request_id, local_response, write_envelope_or_fail, write_error};
use crate::local_ops::LocalOpRegistry;
use crate::{BIN_NAME, GlobalOptions, OutputFormat, capability_unavailable};

pub(super) fn register(reg: &mut LocalOpRegistry) {
    reg.op("instance.initialize", |globals, input, format, out| {
        Box::pin(lifecycle_operation(
            "instance.initialize",
            globals,
            input,
            format,
            out,
        ))
    });
    reg.op("service.start", |globals, input, format, out| {
        Box::pin(lifecycle_operation(
            "service.start",
            globals,
            input,
            format,
            out,
        ))
    });
    reg.op("service.status", |globals, input, format, out| {
        Box::pin(lifecycle_operation(
            "service.status",
            globals,
            input,
            format,
            out,
        ))
    });
    reg.op("service.stop", |globals, input, format, out| {
        Box::pin(lifecycle_operation(
            "service.stop",
            globals,
            input,
            format,
            out,
        ))
    });
}

#[cfg(feature = "embedded-gateway")]
async fn lifecycle_operation(
    method_id: &'static str,
    _globals: &GlobalOptions,
    input: &Value,
    format: OutputFormat,
    out: &mut dyn Write,
) -> i32 {
    use pilotdeck_gateway::public_v1::bootstrap as gw;

    let data_root = match resolve_data_root(input) {
        Ok(root) => root,
        Err(err) => return write_error(out, format, &err),
    };

    let embedded_result: Result<Value, ClientError> = match method_id {
        "instance.initialize" => gw::initialize_instance(
            &data_root,
            input.get("instanceId").and_then(Value::as_str),
            input.get("nodeId").and_then(Value::as_str),
            input.get("expectedAbsent").and_then(Value::as_bool),
        )
        .map_err(ClientError::Contract),
        "service.start" => {
            let mode = input
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or("background");
            if mode == "foreground" {
                return serve_foreground(data_root, input, format, out).await;
            }
            background_service_start(&data_root, input).await
        }
        "service.status" => {
            let expected = match expected_instance_id(input, &data_root) {
                Ok(expected) => expected,
                Err(err) => return write_error(out, format, &err),
            };
            gw::inspect_service_status(&data_root, expected.as_deref())
                .map_err(ClientError::Contract)
        }
        "service.stop" => {
            let expected = match expected_instance_id(input, &data_root) {
                Ok(expected) => expected,
                Err(err) => return write_error(out, format, &err),
            };
            let mode = input.get("mode").and_then(Value::as_str).unwrap_or("idle");
            let timeout_ms = input
                .get("timeoutMs")
                .and_then(Value::as_u64)
                .unwrap_or(30_000);
            gw::stop_local_service(&data_root, expected.as_deref(), mode, timeout_ms)
                .map_err(ClientError::Contract)
        }
        other => Err(capability_unavailable(
            other,
            "unknown local lifecycle operation",
        )),
    };

    match embedded_result {
        Ok(result) => {
            write_envelope_or_fail(out, format, &local_response(local_request_id(), result))
        }
        Err(err) => write_error(out, format, &err),
    }
}

#[cfg(not(feature = "embedded-gateway"))]
async fn lifecycle_operation(
    method_id: &'static str,
    _globals: &GlobalOptions,
    _input: &Value,
    format: OutputFormat,
    out: &mut dyn Write,
) -> i32 {
    let err = capability_unavailable(
        method_id,
        "local lifecycle commands require the `embedded-gateway` feature; rebuild with `--features embedded-gateway`",
    );
    write_error(out, format, &err)
}

#[cfg(feature = "embedded-gateway")]
fn resolve_data_root(input: &Value) -> Result<std::path::PathBuf, ClientError> {
    match input.get("dataRoot").and_then(Value::as_str) {
        Some(raw) => agentcpm_pd_client::config::expand_tilde_with_env(raw),
        None => agentcpm_pd_client::config::resolve_pd_home(None),
    }
}

#[cfg(feature = "embedded-gateway")]
fn expected_instance_id(
    input: &Value,
    data_root: &std::path::Path,
) -> Result<Option<String>, ClientError> {
    if let Some(expected) = input.get("expectedInstanceId").and_then(Value::as_str) {
        return Ok(Some(expected.to_owned()));
    }
    let metadata = agentcpm_pd_client::config::load_local_metadata(data_root)?;
    Ok(Some(metadata.instance_id))
}

#[cfg(feature = "embedded-gateway")]
async fn background_service_start(
    data_root: &std::path::Path,
    input: &Value,
) -> Result<Value, ClientError> {
    use std::sync::Arc;

    use agentcpm_pd_client::autostart::{ProcessGatewaySpawner, ensure_local_gateway_running};

    let metadata = agentcpm_pd_client::config::load_local_metadata(data_root)?;
    let expected = match input.get("expectedInstanceId").and_then(Value::as_str) {
        Some(expected) => expected.to_owned(),
        None => metadata.instance_id.clone(),
    };
    let spawner: Arc<dyn agentcpm_pd_client::autostart::GatewaySpawner> =
        Arc::new(ProcessGatewaySpawner::current_exe()?);
    // An explicit `server start` is not the implicit auto-start path; the
    // global --no-start flag governs implicit start only.
    let ready = ensure_local_gateway_running(data_root, &expected, false, spawner).await?;
    Ok(json!({
        "identity": {
            "instanceId": metadata.instance_id,
            "ownerId": metadata.owner_id,
            "product": metadata.product,
            "protocolVersion": metadata.protocol_version,
        },
        "pid": ready.pid,
        "endpoint": ready.ws_url,
        "generation": ready.generation,
        "ready": true,
    }))
}

/// Foreground `server serve` (recipe: `service.start` with `mode=foreground`):
/// runs the embedded gateway until SIGINT/SIGTERM, then reports the
/// `service.start` result envelope and exits 0.
#[cfg(feature = "embedded-gateway")]
async fn serve_foreground(
    data_root: std::path::PathBuf,
    input: &Value,
    format: OutputFormat,
    out: &mut dyn Write,
) -> i32 {
    use pilotdeck_gateway::public_v1::bootstrap as gw;

    let expected = match expected_instance_id(input, &data_root) {
        Ok(expected) => expected,
        Err(err) => return write_error(out, format, &err),
    };

    let ready_watcher_root = data_root.clone();
    let watcher = tokio::spawn(async move {
        let ready_path = ready_watcher_root.join("run").join("ready.json");
        for _ in 0..600 {
            if let Ok(raw) = std::fs::read_to_string(&ready_path)
                && let Ok(value) = serde_json::from_str::<Value>(&raw)
            {
                eprintln!(
                    "{BIN_NAME}: gateway listening on {} (instance {})",
                    value.get("wsUrl").and_then(Value::as_str).unwrap_or("?"),
                    value
                        .get("instanceId")
                        .and_then(Value::as_str)
                        .unwrap_or("?"),
                );
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    });

    let stop = async {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut term =
                signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    };

    let served = gw::serve_v1_gateway(
        data_root.clone(),
        expected.clone(),
        gw::default_loopback_addr(),
        stop,
    )
    .await;
    watcher.abort();

    match served {
        Ok(ready) => {
            let result = match agentcpm_pd_client::config::load_local_metadata(&data_root) {
                Ok(metadata) => json!({
                    "identity": {
                        "instanceId": metadata.instance_id,
                        "ownerId": metadata.owner_id,
                        "product": metadata.product,
                        "protocolVersion": metadata.protocol_version,
                    },
                    "pid": ready.pid,
                    "endpoint": ready.ws_url,
                    "generation": ready.generation,
                    "ready": true,
                }),
                Err(err) => return write_error(out, format, &err),
            };
            write_envelope_or_fail(out, format, &local_response(local_request_id(), result))
        }
        Err(err) => write_error(out, format, &ClientError::Contract(err)),
    }
}
