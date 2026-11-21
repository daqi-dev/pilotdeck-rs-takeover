//! `target.*`, `connection.*`, and `instance.inspect_initialization` — the
//! CLI-local target-domain operations (S3 `cli.task.t005`, Phase B).
//!
//! 合同正本是 `state/cli-v1-operations.json` 对应条目;数据面全部来自
//! `agentcpm-pd-client::targets`(Phase A 交付的 `TargetStore` /
//! `ConnectionRegistry`)。语义要点:
//! - `target.*`:本机 `<home>/targets.json` 档案 CRUD,单用户本机文件即
//!   边界,从不触碰服务端或凭据值(只存 `credentialRef` 引用)。
//! - `connection.open`:读档案→按 kind 建立真实 `/v1/ws` 连接(本机目标
//!   走 auto-start,https 目标用凭据引用解析的 Bearer),initialize
//!   握手核对 `expectedInstanceId`(不符则零业务请求断开),再经一次
//!   只读 `system.capabilities` 内省取回完整 identity(ownerId 只在
//!   capabilities 里;内省不是业务提交,postcondition「仅建立认证连
//!   接,不提交业务」仍成立)。`connectionId` 用握手返回的服务端 id。
//! - `connection.close`:进程内簿记移除+断开传输;重复关闭/未知 id 返回
//!   `closed:false`,不撤销 key、不取消 run。argv 一次性进程下连接随
//!   open 进程结束自然关闭,close 对跨进程 id 诚实返回 false。
//! - `instance.inspect_initialization`:对 `dataRoot` 做只读四态判定
//!   (absent/initialized/incompatible/corrupt),坏配置返回 corrupt 而
//!   非新实例(合同 acceptance),检查后不创建任何东西。

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use serde_json::{Value, json};

use agentcpm_pd_client::autostart::{ProcessGatewaySpawner, ensure_local_gateway_running};
use agentcpm_pd_client::config::{self, load_local_metadata, normalize_and_validate_ws_url};
use agentcpm_pd_client::error::ClientError;
use agentcpm_pd_client::targets::{
    ConnectionIdentity, ConnectionRegistry, TargetKind, TargetRecord, TargetStore, TargetWriteInput,
};
use agentcpm_pd_client::ws_client::{PdWsClient, PdWsConnectOptions, unwrap_rpc_outcome};
use agentcpm_pd_protocol::contract::OPERATION_VERSION;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_UNAUTHENTICATED, ContractError, EffectStatus,
    RetrySpec,
};
use agentcpm_pd_protocol::id::{IdGenerator, NodeId};
use agentcpm_pd_protocol::registry::{OperationRegistry, validation_errors_to_contract_error};

use crate::commands::{local_request_id, local_response, write_envelope_or_fail, write_error};
use crate::local_ops::LocalOpRegistry;
use crate::{GlobalOptions, OutputFormat, usage_contract_error};

/// Process-wide connection bookkeeping for `connection.open`/`close`.
static CONNECTIONS: OnceLock<ConnectionRegistry> = OnceLock::new();

fn connections() -> &'static ConnectionRegistry {
    CONNECTIONS.get_or_init(ConnectionRegistry::new)
}

pub(super) fn register(reg: &mut LocalOpRegistry) {
    reg.op(
        "instance.inspect_initialization",
        |globals, input, format, out| {
            Box::pin(op_inspect_initialization(globals, input, format, out))
        },
    );
    reg.op("target.list", |globals, input, format, out| {
        Box::pin(op_target_list(globals, input, format, out))
    });
    reg.op("target.get", |globals, input, format, out| {
        Box::pin(op_target_get(globals, input, format, out))
    });
    reg.op("target.write", |globals, input, format, out| {
        Box::pin(op_target_write(globals, input, format, out))
    });
    reg.op("target.select", |globals, input, format, out| {
        Box::pin(op_target_select(globals, input, format, out))
    });
    reg.op("target.remove", |globals, input, format, out| {
        Box::pin(op_target_remove(globals, input, format, out))
    });
    reg.op("connection.open", |globals, input, format, out| {
        Box::pin(op_connection_open(globals, input, format, out))
    });
    reg.op("connection.close", |globals, input, format, out| {
        Box::pin(op_connection_close(globals, input, format, out))
    });
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn finish(out: &mut dyn Write, format: OutputFormat, result: Result<Value, ClientError>) -> i32 {
    match result {
        Ok(value) => {
            write_envelope_or_fail(out, format, &local_response(local_request_id(), value))
        }
        Err(err) => write_error(out, format, &err),
    }
}

/// Validates the input against the embedded operation schema (local ops skip
/// the gateway dispatch gate, so the domain re-applies it — `local_ops/
/// schema.rs` precedent).
fn validate_input(method_id: &str, input: &Value) -> Result<(), ClientError> {
    OperationRegistry::global()
        .validate_input(method_id, input)
        .map(|_| ())
        .map_err(|errs| ClientError::Contract(validation_errors_to_contract_error(errs)))
}

fn pd_home() -> Result<PathBuf, ClientError> {
    config::resolve_pd_home(None)
}

/// Resolves the CLI home used by the target domain: `--data-root`-like
/// explicit roots exist only for the instance ops; targets always live in
/// the resolved pd home.
fn target_store() -> Result<TargetStore, ClientError> {
    Ok(TargetStore::open(&pd_home()?))
}

// ---------------------------------------------------------------------------
// instance.inspect_initialization
// ---------------------------------------------------------------------------

async fn op_inspect_initialization(
    _globals: &GlobalOptions,
    input: &Value,
    format: OutputFormat,
    out: &mut dyn Write,
) -> i32 {
    let result = validate_input("instance.inspect_initialization", input).and_then(|_| {
        let raw = input
            .get("dataRoot")
            .and_then(Value::as_str)
            .ok_or_else(|| ClientError::Contract(usage_contract_error("dataRoot is required")))?;
        let data_root = config::expand_tilde_with_env(raw)?;
        inspect_initialization(&data_root)
    });
    finish(out, format, result)
}

/// Read-only four-state inspection of one data root (contract: 不存在目录
/// 检查后仍不存在;坏配置返回 corrupt 而非新实例). `local.json` is the
/// initialization marker `bootstrap::initialize_instance` writes.
fn inspect_initialization(data_root: &std::path::Path) -> Result<Value, ClientError> {
    let meta_path = config::local_metadata_path(data_root);
    if !meta_path.exists() {
        return Ok(json!({
            "identity": Value::Null,
            "issues": [],
            "initializationState": "absent",
        }));
    }

    let raw = std::fs::read_to_string(&meta_path)
        .map_err(|e| ClientError::config(format!("failed to read {}: {e}", meta_path.display())))?;
    let issue = |code: &str, message: String| json!({"code": code, "message": message});
    let Ok(meta) = serde_json::from_str::<config::LocalInstanceMetadata>(&raw) else {
        return Ok(json!({
            "identity": Value::Null,
            "issues": [issue("LOCAL_METADATA_MALFORMED", format!("{} is not valid JSON for the instance metadata", meta_path.display()))],
            "initializationState": "corrupt",
        }));
    };

    // Product / protocol compatibility gates.
    if meta.product != "agentcpm-pd" {
        return Ok(json!({
            "identity": Value::Null,
            "issues": [issue("PRODUCT_MISMATCH", format!("data root belongs to product `{}`, not `agentcpm-pd`", meta.product))],
            "initializationState": "incompatible",
        }));
    }
    if protocol_major(&meta.protocol_version) != protocol_major(OPERATION_VERSION) {
        return Ok(json!({
            "identity": Value::Null,
            "issues": [issue("PROTOCOL_UNSUPPORTED", format!("data root protocol `{}` is incompatible with the CLI protocol `{OPERATION_VERSION}`", meta.protocol_version))],
            "initializationState": "incompatible",
        }));
    }

    Ok(json!({
        "identity": {
            "instanceId": meta.instance_id,
            "ownerId": meta.owner_id,
            "product": meta.product,
            "protocolVersion": meta.protocol_version,
        },
        "issues": [],
        "initializationState": "initialized",
    }))
}

/// The major number of an `agentcpm-pd.operation/<N>` version string.
fn protocol_major(version: &str) -> u64 {
    version
        .rsplit('/')
        .next()
        .and_then(|major| major.parse::<u64>().ok())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// target.*
// ---------------------------------------------------------------------------

async fn target_list(input: &Value) -> Result<Value, ClientError> {
    validate_input("target.list", input)?;
    let store = target_store()?;
    let page = input.get("page");
    let limit = page.and_then(|p| p.get("limit")).and_then(Value::as_u64);
    let cursor = page.and_then(|p| p.get("cursor")).and_then(Value::as_str);
    let (records, snapshot, next, complete, selected) = store.list_async(limit, cursor).await?;
    Ok(json!({
        "items": records
            .iter()
            .map(TargetRecord::to_contract_json)
            .collect::<Vec<_>>(),
        "pageInfo": {
            "snapshotRevision": snapshot,
            "nextCursor": next,
            "complete": complete,
        },
        "selectedTargetId": selected,
    }))
}

async fn op_target_list(
    _globals: &GlobalOptions,
    input: &Value,
    format: OutputFormat,
    out: &mut dyn Write,
) -> i32 {
    finish(out, format, target_list(input).await)
}

async fn target_get(input: &Value) -> Result<Value, ClientError> {
    validate_input("target.get", input)?;
    let store = target_store()?;
    let target_id = input
        .get("targetId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let record = store.get_async(target_id).await?;
    Ok(json!({"target": record.to_contract_json()}))
}

async fn op_target_get(
    _globals: &GlobalOptions,
    input: &Value,
    format: OutputFormat,
    out: &mut dyn Write,
) -> i32 {
    finish(out, format, target_get(input).await)
}

async fn target_write(input: &Value) -> Result<Value, ClientError> {
    validate_input("target.write", input)?;
    let store = target_store()?;
    let kind = TargetKind::parse(
        input
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    )?;
    let write_input = TargetWriteInput {
        target_id: input
            .get("targetId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        expected_revision: input
            .get("expectedRevision")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        name: input
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        kind,
        endpoint: input
            .get("endpoint")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        expected_instance_id: input
            .get("expectedInstanceId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        credential_ref: input
            .get("credentialRef")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    };
    let record = store.write_async(write_input).await?;
    Ok(json!({"target": record.to_contract_json()}))
}

async fn op_target_write(
    _globals: &GlobalOptions,
    input: &Value,
    format: OutputFormat,
    out: &mut dyn Write,
) -> i32 {
    finish(out, format, target_write(input).await)
}

async fn target_select(input: &Value) -> Result<Value, ClientError> {
    validate_input("target.select", input)?;
    let store = target_store()?;
    let target_id = input
        .get("targetId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let expected = input
        .get("expectedRevision")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (selected, revision) = store.select_async(target_id, expected).await?;
    Ok(json!({"selectedTargetId": selected, "revision": revision}))
}

async fn op_target_select(
    _globals: &GlobalOptions,
    input: &Value,
    format: OutputFormat,
    out: &mut dyn Write,
) -> i32 {
    finish(out, format, target_select(input).await)
}

async fn target_remove(input: &Value) -> Result<Value, ClientError> {
    validate_input("target.remove", input)?;
    let store = target_store()?;
    let target_id = input
        .get("targetId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let expected = input
        .get("expectedRevision")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (removed, revision) = store.remove_async(target_id, expected).await?;
    Ok(json!({"removedTargetId": removed, "revision": revision}))
}

async fn op_target_remove(
    _globals: &GlobalOptions,
    input: &Value,
    format: OutputFormat,
    out: &mut dyn Write,
) -> i32 {
    finish(out, format, target_remove(input).await)
}

// ---------------------------------------------------------------------------
// connection.open / connection.close
// ---------------------------------------------------------------------------

async fn load_connection_open_target(
    input: &Value,
) -> Result<(TargetRecord, Option<String>), ClientError> {
    validate_input("connection.open", input)?;
    let home = pd_home()?;
    let store = TargetStore::open(&home);
    let target_id = input
        .get("targetId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let record = store.get_async(target_id).await.map_err(|err| {
        if matches!(&err, ClientError::Contract(c) if c.code == CODE_NOT_FOUND) {
            ClientError::Contract(ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!("target `{target_id}` is not configured in this home; write it first"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ))
        } else {
            err
        }
    })?;
    let credential_ref = input
        .get("credentialRef")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| record.credential_ref.clone());
    Ok((record, credential_ref))
}

async fn op_connection_open(
    globals: &GlobalOptions,
    input: &Value,
    format: OutputFormat,
    out: &mut dyn Write,
) -> i32 {
    let (record, credential_ref) = match load_connection_open_target(input).await {
        Ok(pair) => pair,
        Err(err) => return write_error(out, format, &err),
    };

    let established = connect_target(globals, &record, credential_ref.as_deref()).await;
    let (client, identity, protocol_version, connection_id) = match established {
        Ok(quadruple) => quadruple,
        Err(err) => return write_error(out, format, &map_connection_error(err)),
    };

    // Bookkeep under the server-assigned handshake connectionId; the live
    // client handle rides along so `connection.close` detaches the transport.
    connections().register_established(
        &connection_id,
        &record.target_id,
        identity.clone(),
        Some(client),
    );

    finish(
        out,
        format,
        Ok(json!({
            "identity": {
                "instanceId": identity.instance_id,
                "ownerId": identity.owner_id,
                "product": identity.product,
                "protocolVersion": identity.protocol_version,
            },
            "connectionId": connection_id,
            "protocolVersion": protocol_version,
        })),
    )
}

/// Establishes the real connection for one target record. Local targets
/// auto-start the embedded gateway (unless `--no-start`); https targets
/// resolve their credential reference into `credentials.json`; ssh targets
/// answer the contract's `UNSUPPORTED_OPERATION` (tunnel support is out of
/// this round's scope — the remote acceptance profile is the ws:// URL
/// profile).
async fn connect_target(
    globals: &GlobalOptions,
    record: &TargetRecord,
    credential_ref: Option<&str>,
) -> Result<(PdWsClient, ConnectionIdentity, String, String), ClientError> {
    let home = pd_home()?;
    let (ws_url, token, expected_instance_id) = match record.kind {
        TargetKind::Local => {
            let target_home = if record.endpoint.trim().is_empty() {
                home
            } else {
                config::expand_tilde_with_env(&record.endpoint)?
            };
            let metadata = load_local_metadata(&target_home)?;
            if record.expected_instance_id != metadata.instance_id {
                return Err(ClientError::InstanceChanged {
                    expected: record.expected_instance_id.clone(),
                    actual: metadata.instance_id,
                });
            }
            let token = config::load_local_token(&target_home)?;
            let spawner: Arc<dyn agentcpm_pd_client::autostart::GatewaySpawner> =
                Arc::new(ProcessGatewaySpawner::current_exe()?);
            let ready = ensure_local_gateway_running(
                &target_home,
                &metadata.instance_id,
                globals.no_start,
                spawner,
            )
            .await?;
            (ready.ws_url, Some(token), Some(metadata.instance_id))
        }
        TargetKind::Https => {
            let ws_url = normalize_and_validate_ws_url(&record.endpoint)?;
            let token = match credential_ref {
                Some(reference) => {
                    let credentials = config::load_credentials(&home)?;
                    let token = credentials.tokens.get(reference).cloned();
                    if token.is_none() {
                        return Err(ClientError::Contract(ContractError::new(
                            "AUTH_REQUIRED",
                            format!(
                                "credential `{reference}` referenced by target `{}` is not present in credentials store",
                                record.target_id
                            ),
                            RetrySpec::never(),
                            EffectStatus::NotApplied,
                        )));
                    }
                    token
                }
                None => None,
            };
            (ws_url, token, Some(record.expected_instance_id.clone()))
        }
        TargetKind::Ssh => {
            return Err(ClientError::Contract(ContractError::new(
                "UNSUPPORTED_OPERATION",
                "ssh target connections are not implemented in this round; use an https (ws:// URL) target",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )));
        }
    };

    let client = PdWsClient::connect(PdWsConnectOptions {
        ws_url,
        token,
        expected_instance_id,
        client_name: "agentcpm-pd".to_owned(),
        client_version: env!("CARGO_PKG_VERSION").to_owned(),
        id_gen: Arc::new(IdGenerator::new(NodeId::local())),
    })
    .await?;

    // The handshake already verified the expected instanceId (mismatch would
    // have failed above with zero business requests). Read the full identity
    // through the read-only `system.capabilities` introspection — ownerId is
    // only reported there, and introspection is not a business submission
    // (postcondition: 仅建立认证连接,不提交业务).
    let caps = client
        .call_operation("system.capabilities", json!({}), None, None, None)
        .await?;
    let caps_result = unwrap_rpc_outcome(&caps);
    let identity = ConnectionIdentity {
        instance_id: caps_result["identity"]["instanceId"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        owner_id: caps_result["identity"]["ownerId"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        product: caps_result["identity"]["product"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        protocol_version: client.initialize_result().api_version.clone(),
    };
    let connection_id = client.initialize_result().connection_id.clone();
    let api_version = client.initialize_result().api_version.clone();
    Ok((client, identity, api_version, connection_id))
}

/// Maps client-layer failures onto the contract's `connection.open` error
/// codes: 401/403 → AUTH_REQUIRED, instance mismatch → INSTANCE_MISMATCH,
/// handshake version incompatibility → PROTOCOL_UNSUPPORTED, TLS-shaped
/// transport failures → TLS_FAILED, everything else connection-shaped →
/// CONNECTION_FAILED.
fn map_connection_error(err: ClientError) -> ClientError {
    let contract = |code: &str, message: String| {
        ClientError::Contract(ContractError::new(
            code,
            message,
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ))
    };
    match &err {
        ClientError::Contract(c) if c.code == CODE_UNAUTHENTICATED => {
            contract("AUTH_REQUIRED", c.message.clone())
        }
        ClientError::InstanceChanged { expected, actual } => contract(
            "INSTANCE_MISMATCH",
            format!("instanceId mismatch: expected `{expected}`, got `{actual}`"),
        ),
        ClientError::ProtocolError { code, message }
            if *code == agentcpm_pd_protocol::errors::ERR_UNSUPPORTED_VERSION =>
        {
            contract("PROTOCOL_UNSUPPORTED", message.clone())
        }
        ClientError::ConnectionFailed(message) => {
            let lower = message.to_ascii_lowercase();
            if lower.contains("tls") || lower.contains("certificate") || lower.contains("cert") {
                contract("TLS_FAILED", message.clone())
            } else {
                contract("CONNECTION_FAILED", message.clone())
            }
        }
        other => other.clone(),
    }
}

async fn op_connection_close(
    _globals: &GlobalOptions,
    input: &Value,
    format: OutputFormat,
    out: &mut dyn Write,
) -> i32 {
    let taken = validate_input("connection.close", input).map(|_| {
        let connection_id = input
            .get("connectionId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        connections().take(connection_id)
    });
    match taken {
        Ok(Some(entry)) => {
            // Detach the transport after the bookkeeping removal; the server
            // side keeps its keys and runs untouched (contract: 不撤销key、
            // 不取消run).
            if let Some(client) = entry.client {
                let _ = client.close().await;
            }
            finish(out, format, Ok(json!({"closed": true})))
        }
        Ok(None) => finish(out, format, Ok(json!({"closed": false}))),
        Err(err) => write_error(out, format, &err),
    }
}
