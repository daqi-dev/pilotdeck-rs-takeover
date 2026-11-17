//! `api connect`: the pd-connect/1 stdio engine (`WP3`, `007 §6`).
//!
//! Three tasks (`007 §6.11`): a bounded stdin reader (1 MiB frame cap with
//! recovery), a stdout writer with `Control > Normal > Event` biased queues, and
//! the upstream `/v1/ws` task that forwards business calls, multiplexes
//! subscriptions, tracks heartbeats, and reconnects with backoff.
//!
//! Host-visible behavior: mandatory `initialize` handshake with CLI process
//! info, local read-only `schema.*`, rejection of local lifecycle operations
//! (`service.*` / `instance.*`), per-class pending limits (16 control / 64
//! normal), 5 s EOF drain that never cancels runs, SIGINT exit 130, and
//! `conn.state` notifications on upstream loss (exit 1 on `instance_changed`).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use agentcpm_pd_client::error::ClientError;
use agentcpm_pd_client::ws_client::{PdWsClient, PdWsSubscription, SubStreamItem};
use agentcpm_pd_client::{PdClient, PdClientOptions};
use agentcpm_pd_protocol::contract::{OPERATION_VERSION, ResponseOutcome};
use agentcpm_pd_protocol::envelope::{bind_instance_id, is_control_method, unary_timeout_ms};
use agentcpm_pd_protocol::errors::{
    CODE_CAPABILITY_UNAVAILABLE, CODE_CONNECTION_FAILED, ContractError, EXIT_FAILED,
    EXIT_INTERRUPTED, EXIT_OK, EXIT_USAGE, EffectStatus, RetrySpec,
};
use agentcpm_pd_protocol::framing::{decode_ndjson_line, encode_ndjson_line};
use agentcpm_pd_protocol::id::{IdGenerator, NodeId, now_ms};
use agentcpm_pd_protocol::registry::{ExecutionMode, OperationRegistry, RetryKind};
use agentcpm_pd_protocol::wire::{
    ABI_VERSION, CliBuildInfo, ConnAbandonParams, ConnHeartbeatNotification, ConnStateKind,
    ConnStateNotification, DEFAULT_CREDIT, DEFAULT_HEARTBEAT_MS, EOF_DRAIN_MS, InitializeParams,
    InitializeResult, JSONRPC_VERSION, JsonRpcFrame, JsonRpcNotification, JsonRpcRequest,
    JsonRpcResponse, MAX_CONTROL_PENDING, MAX_FRAME_BYTES, MAX_PENDING, MAX_SUBSCRIPTIONS,
    OperationRpcParams, OperationRpcResult, ProtocolLimits, SubCloseParams, SubCreditParams,
    SubEndNotification, SubEventNotification, SubOpenParams, SubOpenResult, SubTerminal,
    TargetHandshakeInfo, WIRE_CONN_ABANDON, WIRE_CONN_HEARTBEAT, WIRE_CONN_STATE, WIRE_INITIALIZE,
    WIRE_SUB_CLOSE, WIRE_SUB_CREDIT, WIRE_SUB_END, WIRE_SUB_EVENT, WIRE_SUB_OPEN,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::commands::LogFormat;
use crate::{BIN_NAME, GlobalOptions};

const STARTUP_CONNECT_ATTEMPTS: usize = 3;
const RECONNECT_BACKOFF_MS: &[u64] = &[500, 1_000, 2_000, 4_000, 8_000, 10_000];
const WATCHDOG_INTERVAL: Duration = Duration::from_millis(400);
const WRITER_HEARTBEAT_TICK: Duration = Duration::from_millis(500);
const IDLE_QUEUE_POLL: Duration = Duration::from_millis(50);

// ---------------------------------------------------------------------------
// Entry
// ---------------------------------------------------------------------------

/// Runs `agentcpm-pd api connect` and returns the process exit code
/// (`007 §6.8`: 0 normal, 1 runtime fatal, 2 usage/config, 130 SIGINT).
pub async fn run_connect(globals: GlobalOptions, log_format: LogFormat) -> i32 {
    let client_opts = PdClientOptions {
        home_override: None,
        target: globals.target.clone(),
        no_start: globals.no_start,
        spawner: None,
    };
    let diag = |message: String| match log_format {
        LogFormat::Json => eprintln!("{}", json!({"level": "error", "message": message})),
        LogFormat::Text => eprintln!("{BIN_NAME}: {message}"),
    };

    let (ctrl_tx, ctrl_rx) = mpsc::channel::<String>(256);
    let (norm_tx, norm_rx) = mpsc::channel::<String>(256);
    let (evt_tx, evt_rx) = mpsc::channel::<String>(MAX_SUBSCRIPTIONS * 16);
    let (writer_kill_tx, writer_kill_rx) = watch::channel(false);
    let (writer_died_tx, writer_died_rx) = oneshot::channel::<()>();
    let (reader_tx, reader_rx) = mpsc::channel::<ReaderMsg>(256);
    let (done_tx, done_rx) = mpsc::channel::<DoneMsg>(256);
    let (sub_kill_tx, sub_kill_rx) = watch::channel(0u64);
    let (sub_announce_tx, sub_announce_rx) = mpsc::channel::<SubAnnouncement>(64);
    let (reconnect_tx, reconnect_rx) = mpsc::channel::<Result<PdClient, ClientError>>(1);

    let initialized = Arc::new(AtomicBool::new(false));
    let shutting_down = Arc::new(AtomicBool::new(false));

    let writer_handle = tokio::spawn(writer_task(
        ctrl_rx,
        norm_rx,
        evt_rx,
        writer_kill_rx,
        initialized.clone(),
        shutting_down.clone(),
        writer_died_tx,
    ));
    let reader_handle = tokio::spawn(reader_task(reader_tx));

    let mut engine =
        match connect_upstream_with_retries(&client_opts, STARTUP_CONNECT_ATTEMPTS).await {
            Ok(client) => Engine {
                out: OutQueues {
                    ctrl: ctrl_tx.clone(),
                    norm: norm_tx.clone(),
                    evt: evt_tx,
                },
                done_tx,
                sub_kill: sub_kill_tx,
                sub_kill_rx,
                sub_announce_tx,
                initialized: initialized.clone(),
                shutting_down,
                ws: client.ws().clone(),
                bound_instance_id: client.ws().bound_instance_id().to_owned(),
                client_opts,
                pending: HashMap::new(),
                pending_control: 0,
                pending_normal: 0,
                subscriptions: HashMap::new(),
                sub_generation: 0,
                reconnecting: false,
                reconnect_in_flight: false,
                reconnect_attempt: 0,
                next_reconnect_at: None,
                reconnect_tx,
                log_format,
            },
            Err(err) => {
                let code = match &err {
                    ClientError::Usage(_)
                    | ClientError::Config(_)
                    | ClientError::NotInitialized(_) => EXIT_USAGE,
                    _ => EXIT_FAILED,
                };
                let contract = err.into_contract_error();
                diag(format!("gateway connect failed: {err}"));
                let _ = ctrl_tx.send(conn_fatal_frame(&contract)).await;
                let _ = writer_kill_tx.send(true);
                let _ = writer_handle.await;
                reader_handle.abort();
                return code;
            }
        };

    let exit_code = engine
        .run(
            reader_rx,
            done_rx,
            sub_announce_rx,
            reconnect_rx,
            writer_died_rx,
            reader_handle,
        )
        .await;

    // Final flush and teardown.
    let _ = writer_kill_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(1), writer_handle).await;
    exit_code
}

// ---------------------------------------------------------------------------
// Bounded stdin reading (`007 §6.1`)
// ---------------------------------------------------------------------------

enum ReaderMsg {
    Frame(JsonRpcFrame),
    ErrorResponse(JsonRpcResponse),
    Eof,
}

/// Reads one line with a byte cap: lines larger than `cap` are truncated and
/// reported as `(truncated, total_bytes)` so the caller can answer
/// `-32600 frame_too_large` and keep reading the next line (`T2` recovery).
async fn read_line_capped<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    cap: usize,
) -> std::io::Result<Option<(Vec<u8>, bool, usize)>> {
    let mut out: Vec<u8> = Vec::new();
    let mut truncated = false;
    let mut total = 0usize;
    loop {
        let (used, found_newline) = {
            let buf = reader.fill_buf().await?;
            if buf.is_empty() {
                if total == 0 {
                    return Ok(None);
                }
                return Ok(Some((out, truncated, total)));
            }
            let (used, found) = match buf.iter().position(|&b| b == b'\n') {
                Some(pos) => (pos + 1, true),
                None => (buf.len(), false),
            };
            if !truncated {
                out.extend_from_slice(&buf[..used]);
            }
            (used, found)
        };
        total += used;
        reader.consume(used);
        if !truncated && out.len() > cap {
            truncated = true;
            out.clear();
        }
        if found_newline {
            return Ok(Some((out, truncated, total)));
        }
    }
}

async fn reader_task(tx: mpsc::Sender<ReaderMsg>) {
    let stdin = tokio::io::stdin();
    let mut reader = tokio::io::BufReader::new(stdin);
    loop {
        match read_line_capped(&mut reader, MAX_FRAME_BYTES).await {
            Ok(Some((line, truncated, total))) => {
                if truncated {
                    let resp = JsonRpcResponse::err(
                        None,
                        agentcpm_pd_protocol::errors::JSONRPC_INVALID_REQUEST,
                        "frame too large",
                        Some(json!({
                            "reason": "frame_too_large",
                            "bytes": total,
                            "maxFrameBytes": MAX_FRAME_BYTES,
                        })),
                    );
                    if tx.send(ReaderMsg::ErrorResponse(resp)).await.is_err() {
                        break;
                    }
                    continue;
                }
                match decode_ndjson_line(&line) {
                    Ok(frame) => {
                        if tx.send(ReaderMsg::Frame(frame)).await.is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        if let Some(resp) = err.to_error_response()
                            && tx.send(ReaderMsg::ErrorResponse(resp)).await.is_err()
                        {
                            break;
                        }
                    }
                }
            }
            Ok(None) => {
                let _ = tx.send(ReaderMsg::Eof).await;
                break;
            }
            Err(e) => {
                eprintln!("{BIN_NAME}: stdin read failed: {e}");
                let _ = tx.send(ReaderMsg::Eof).await;
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Writer with Control > Normal > Event priority and idle heartbeats
// ---------------------------------------------------------------------------

struct OutQueues {
    ctrl: mpsc::Sender<String>,
    norm: mpsc::Sender<String>,
    evt: mpsc::Sender<String>,
}

#[allow(clippy::too_many_arguments)]
async fn write_line(out: &mut tokio::io::Stdout, line: &str) -> bool {
    out.write_all(line.as_bytes()).await.is_ok() && out.flush().await.is_ok()
}

async fn writer_task(
    mut ctrl: mpsc::Receiver<String>,
    mut norm: mpsc::Receiver<String>,
    mut evt: mpsc::Receiver<String>,
    mut kill: watch::Receiver<bool>,
    initialized: Arc<AtomicBool>,
    shutting_down: Arc<AtomicBool>,
    writer_died: oneshot::Sender<()>,
) {
    let mut out = tokio::io::stdout();
    let mut last_write = tokio::time::Instant::now();
    let mut tick = tokio::time::interval(WRITER_HEARTBEAT_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut alive = true;

    while alive {
        tokio::select! {
            biased;
            _ = kill.changed() => {
                // Shutdown: flush everything already accepted before exiting so
                // buffered events are not lost (`007 §6.6` drain semantics).
                drain_writer_queues(&mut ctrl, &mut norm, &mut evt, &mut out).await;
                break;
            }
            frame = ctrl.recv() => match frame {
                Some(line) => {
                    if !write_line(&mut out, &line).await {
                        alive = false;
                    }
                    last_write = tokio::time::Instant::now();
                }
                None => tokio::time::sleep(IDLE_QUEUE_POLL).await,
            },
            frame = norm.recv() => match frame {
                Some(line) => {
                    if !write_line(&mut out, &line).await {
                        alive = false;
                    }
                    last_write = tokio::time::Instant::now();
                }
                None => tokio::time::sleep(IDLE_QUEUE_POLL).await,
            },
            frame = evt.recv() => match frame {
                Some(line) => {
                    if !write_line(&mut out, &line).await {
                        alive = false;
                    }
                    last_write = tokio::time::Instant::now();
                }
                None => tokio::time::sleep(IDLE_QUEUE_POLL).await,
            },
            _ = tick.tick() => {
                if initialized.load(Ordering::SeqCst)
                    && !shutting_down.load(Ordering::SeqCst)
                    && last_write.elapsed() >= Duration::from_millis(DEFAULT_HEARTBEAT_MS)
                {
                    let hb = JsonRpcNotification {
                        jsonrpc: JSONRPC_VERSION.to_owned(),
                        method: WIRE_CONN_HEARTBEAT.to_owned(),
                        params: serde_json::to_value(ConnHeartbeatNotification { ts_ms: now_ms() })
                            .unwrap_or(Value::Null),
                    };
                    if let Ok(line) = encode_ndjson_line(&hb) {
                        if !write_line(&mut out, &line).await {
                            alive = false;
                        }
                        last_write = tokio::time::Instant::now();
                    }
                }
            }
        }
    }
    let _ = writer_died.send(());
}

async fn drain_writer_queues(
    ctrl: &mut mpsc::Receiver<String>,
    norm: &mut mpsc::Receiver<String>,
    evt: &mut mpsc::Receiver<String>,
    out: &mut tokio::io::Stdout,
) {
    for queue in [ctrl, norm, evt] {
        while let Ok(line) = queue.try_recv() {
            if !write_line(out, &line).await {
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pure request preparation (unit-testable without the engine)
// ---------------------------------------------------------------------------

/// Snapshot of the gate state consulted while preparing one inbound request.
#[derive(Debug)]
pub struct RequestGate {
    pub initialized: bool,
    pub bound_instance_id: String,
    pub pending_ids: HashSet<String>,
    pub pending_control: usize,
    pub pending_normal: usize,
    pub subscriptions: usize,
}

/// The action decided for one inbound business request.
#[derive(Debug)]
pub enum PreparedRequest {
    /// Serve read-only `schema.list` / `schema.get` locally from the registry.
    LocalSchema { method: String, input: Value },
    /// Forward to the gateway.
    Forward {
        method: String,
        params: OperationRpcParams,
        timeout: Duration,
        is_control: bool,
        is_write: bool,
    },
    /// Reply with this error frame instead.
    Rejected(JsonRpcResponse),
}

fn invalid_params(id: &str, message: impl Into<String>, data: Option<Value>) -> JsonRpcResponse {
    JsonRpcResponse::err(
        Some(id.to_owned()),
        agentcpm_pd_protocol::errors::JSONRPC_INVALID_PARAMS,
        message,
        data,
    )
}

fn method_not_found(id: &str, method: &str, contract_code: Option<&str>) -> JsonRpcResponse {
    let data = contract_code.map(|code| {
        serde_json::to_value(ContractError::new(
            code,
            format!("method `{method}` is not available on this connection"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ))
        .unwrap_or(Value::Null)
    });
    JsonRpcResponse::err(
        Some(id.to_owned()),
        agentcpm_pd_protocol::errors::JSONRPC_METHOD_NOT_FOUND,
        format!("method not found: {method}"),
        data,
    )
}

fn pending_limit_error(id: &str, limit: usize, class: &str) -> JsonRpcResponse {
    JsonRpcResponse::err(
        Some(id.to_owned()),
        agentcpm_pd_protocol::errors::JSONRPC_INVALID_REQUEST,
        format!("too many pending {class} requests (limit {limit})"),
        None,
    )
}

#[allow(clippy::result_large_err)] // JsonRpcResponse is fixed by the frozen protocol crate
fn parse_rpc_params(req: &JsonRpcRequest) -> Result<OperationRpcParams, JsonRpcResponse> {
    serde_json::from_value::<OperationRpcParams>(req.params.clone()).map_err(|_| {
        invalid_params(
            &req.id,
            "params must be {input, idempotencyKey?, expectedRevision?}",
            None,
        )
    })
}

fn schema_violation(
    id: &str,
    errors: &[agentcpm_pd_protocol::schema_validator::ValidationError],
) -> JsonRpcResponse {
    let first = errors.first();
    let data = json!({
        "details": {
            "path": first.map(|e| e.path.clone()).unwrap_or_default(),
            "message": first.map(|e| e.message.clone()).unwrap_or_default(),
        },
        "validationErrors": serde_json::to_value(errors).unwrap_or(Value::Null),
    });
    invalid_params(
        id,
        format!(
            "input does not match schema at {}",
            first.map(|e| e.path.clone()).unwrap_or_default()
        ),
        Some(data),
    )
}

/// Validates and classifies one inbound business request (`007 §6.3`):
/// duplicate ids and pending limits (-32600), unknown methods (-32601),
/// malformed params and schema violations (-32602 with `details.path`),
/// missing idempotency keys (-32086), and instance binding (-32000).
pub fn prepare_request(req: &JsonRpcRequest, gate: &RequestGate) -> PreparedRequest {
    let reg = OperationRegistry::global();
    let id = req.id.as_str();

    if gate.pending_ids.contains(id) {
        return PreparedRequest::Rejected(JsonRpcResponse::err(
            Some(id.to_owned()),
            agentcpm_pd_protocol::errors::JSONRPC_INVALID_REQUEST,
            format!("request id `{id}` is already in flight"),
            None,
        ));
    }

    let Some(op) = reg.get(&req.method) else {
        return PreparedRequest::Rejected(method_not_found(
            id,
            &req.method,
            Some(agentcpm_pd_protocol::errors::CODE_UNSUPPORTED_OPERATION),
        ));
    };

    let is_control = is_control_method(&req.method);
    if is_control && gate.pending_control >= MAX_CONTROL_PENDING {
        return PreparedRequest::Rejected(pending_limit_error(id, MAX_CONTROL_PENDING, "control"));
    }
    if !is_control && gate.pending_normal >= MAX_PENDING {
        return PreparedRequest::Rejected(pending_limit_error(id, MAX_PENDING, "normal"));
    }

    let Ok(params) = parse_rpc_params(req) else {
        return PreparedRequest::Rejected(invalid_params(
            id,
            "params must be {input, idempotencyKey?, expectedRevision?}",
            None,
        ));
    };
    if !params.input.is_object() {
        return PreparedRequest::Rejected(invalid_params(
            id,
            "params.input must be a JSON object",
            None,
        ));
    }

    if op.execution == ExecutionMode::Local {
        // `007 §5`: on the long connection only read-only schema.list/schema.get
        // run locally; local lifecycle commands are top-level-only and are
        // answered with -32601 / CAPABILITY_UNAVAILABLE.
        if req.method == "schema.list" || req.method == "schema.get" {
            let input = params.input;
            if let Err(errors) = reg.validate_input(&req.method, &input) {
                return PreparedRequest::Rejected(schema_violation(id, &errors));
            }
            return PreparedRequest::LocalSchema {
                method: req.method.clone(),
                input,
            };
        }
        return PreparedRequest::Rejected(method_not_found(
            id,
            &req.method,
            Some(CODE_CAPABILITY_UNAVAILABLE),
        ));
    }

    let mut input = params.input.clone();
    if let Err(err) = bind_instance_id(&mut input, &gate.bound_instance_id, op.declares_instance_id)
    {
        return PreparedRequest::Rejected(JsonRpcResponse::application_error(id, &err));
    }

    if let Err(errors) = reg.validate_input(&req.method, &input) {
        return PreparedRequest::Rejected(schema_violation(id, &errors));
    }

    if op.retry == RetryKind::IdempotentSubmit
        && params.idempotency_key.as_deref().is_none_or(str::is_empty)
    {
        return PreparedRequest::Rejected(JsonRpcResponse::err(
            Some(id.to_owned()),
            agentcpm_pd_protocol::errors::ERR_MISSING_IDEMPOTENCY_KEY,
            format!(
                "operation `{}` requires an idempotencyKey provided by the caller",
                req.method
            ),
            None,
        ));
    }

    let timeout = Duration::from_millis(unary_timeout_ms(&input));
    let is_write = op.retry != RetryKind::ReadOnly;
    PreparedRequest::Forward {
        method: req.method.clone(),
        params: OperationRpcParams {
            input,
            idempotency_key: params.idempotency_key,
            expected_revision: params.expected_revision,
        },
        timeout,
        is_control,
        is_write,
    }
}

/// Result of validating one `sub.open` (`007 §6.4`).
#[derive(Debug)]
pub struct PreparedSubOpen {
    pub method: String,
    pub input: Value,
    pub credit: u32,
}

/// Validates a `sub.open`: stream methods only, `page` rejected, `follow`
/// forced to `true`, schema-validated input.
#[allow(clippy::result_large_err)] // JsonRpcResponse is fixed by the frozen protocol crate
pub fn prepare_sub_open(
    req: &JsonRpcRequest,
    gate: &RequestGate,
) -> Result<PreparedSubOpen, JsonRpcResponse> {
    let reg = OperationRegistry::global();
    let id = req.id.as_str();

    if gate.subscriptions >= MAX_SUBSCRIPTIONS {
        return Err(JsonRpcResponse::err(
            Some(id.to_owned()),
            agentcpm_pd_protocol::errors::JSONRPC_INVALID_REQUEST,
            format!("subscription limit reached ({MAX_SUBSCRIPTIONS})"),
            None,
        ));
    }

    let Ok(params) = serde_json::from_value::<SubOpenParams>(req.params.clone()) else {
        return Err(invalid_params(
            id,
            "params must be {stream, input, credit?}",
            None,
        ));
    };

    let Some(op) = reg.get(&params.stream) else {
        return Err(method_not_found(
            id,
            &params.stream,
            Some(agentcpm_pd_protocol::errors::CODE_UNSUPPORTED_OPERATION),
        ));
    };
    if op.execution != ExecutionMode::Stream {
        return Err(invalid_params(
            id,
            format!("`{}` is not a stream operation", params.stream),
            None,
        ));
    }

    let Some(mut obj) = params.input.as_object().cloned() else {
        return Err(invalid_params(id, "input must be a JSON object", None));
    };
    if obj.contains_key("page") {
        return Err(invalid_params(
            id,
            "`page` cannot be used with subscriptions; paginate with a unary follow=false call",
            None,
        ));
    }
    obj.insert("follow".to_owned(), Value::Bool(true));
    let mut input = Value::Object(obj);

    if let Err(err) = bind_instance_id(&mut input, &gate.bound_instance_id, op.declares_instance_id)
    {
        return Err(JsonRpcResponse::application_error(id, &err));
    }
    if let Err(errors) = reg.validate_input(&params.stream, &input) {
        return Err(schema_violation(id, &errors));
    }

    Ok(PreparedSubOpen {
        method: params.stream,
        input,
        credit: params.credit.unwrap_or(DEFAULT_CREDIT),
    })
}

/// Builds the `initialize` response (`007 §6.2`) or the matching error frame:
/// `-32006` with `data.supported` on version mismatch, `-32600` on repeats.
pub fn build_initialize_response(
    req: &JsonRpcRequest,
    already_initialized: bool,
    target: &TargetHandshakeInfo,
    connection_id: &str,
) -> JsonRpcResponse {
    let id = req.id.as_str();
    if already_initialized {
        return JsonRpcResponse::err(
            Some(id.to_owned()),
            agentcpm_pd_protocol::errors::JSONRPC_INVALID_REQUEST,
            "initialize must be the first and only handshake request",
            None,
        );
    }
    let Ok(params) = serde_json::from_value::<InitializeParams>(req.params.clone()) else {
        return JsonRpcResponse::err(
            Some(id.to_owned()),
            agentcpm_pd_protocol::errors::JSONRPC_INVALID_PARAMS,
            "initialize params must be {abi_version, api_version, features[], client{name,version}}",
            None,
        );
    };
    if params.abi_version != ABI_VERSION {
        return JsonRpcResponse::err(
            Some(id.to_owned()),
            agentcpm_pd_protocol::errors::ERR_UNSUPPORTED_VERSION,
            format!("unsupported abi_version {}", params.abi_version),
            Some(json!({"supported": [ABI_VERSION]})),
        );
    }
    if params.api_version != OPERATION_VERSION {
        return JsonRpcResponse::err(
            Some(id.to_owned()),
            agentcpm_pd_protocol::errors::ERR_UNSUPPORTED_VERSION,
            format!("unsupported api_version `{}`", params.api_version),
            Some(json!({"supported": [OPERATION_VERSION]})),
        );
    }

    let supported_features: Vec<String> = ["subscriptions", "credit", "heartbeat"]
        .into_iter()
        .filter(|feature| params.features.iter().any(|requested| requested == feature))
        .map(ToOwned::to_owned)
        .collect();

    let result = InitializeResult {
        abi_version: ABI_VERSION,
        supported: true,
        api_version: OPERATION_VERSION.to_owned(),
        supported_features,
        connection_id: connection_id.to_owned(),
        cli: Some(cli_build_info()),
        target: target.clone(),
        limits: ProtocolLimits::default(),
    };
    JsonRpcResponse::ok(id, serde_json::to_value(result).unwrap_or(Value::Null))
}

fn cli_build_info() -> CliBuildInfo {
    let digest = OperationRegistry::global().registry_digest();
    CliBuildInfo {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        build: format!(
            "{}+registry-{}",
            option_env!("AGENTCPM_PD_BUILD_GIT_SHA").unwrap_or("dev"),
            &digest[..digest.len().min(12)],
        ),
        pid: std::process::id(),
        executable: std::env::current_exe()
            .map(|exe| exe.display().to_string())
            .unwrap_or_default(),
    }
}

/// Maps a forwarded-call failure onto the JSON-RPC error frame
/// (`-32000` with the contract error in `data`, or the raw protocol code).
pub fn error_response_for(id: &str, err: &ClientError) -> JsonRpcResponse {
    match err {
        ClientError::Contract(contract) => JsonRpcResponse::application_error(id, contract),
        ClientError::ProtocolError { code, message } => {
            JsonRpcResponse::err(Some(id.to_owned()), *code, message.clone(), None)
        }
        other => JsonRpcResponse::application_error(id, &other.into_contract_error()),
    }
}

fn conn_fatal_frame(contract: &ContractError) -> String {
    let notif = JsonRpcNotification {
        jsonrpc: JSONRPC_VERSION.to_owned(),
        method: "conn.fatal".to_owned(),
        params: json!({
            "code": contract.code,
            "message": contract.message,
        }),
    };
    encode_ndjson_line(&notif).unwrap_or_default()
}

/// The reply sent to the original id of an abandoned request (`007 §6.5`):
/// reads are `not_applied`, already-sent writes are `unknown` with the key.
fn abandon_reply(id: &str, is_write: bool, idempotency_key: Option<&str>) -> JsonRpcResponse {
    let contract = if is_write {
        ContractError::result_unknown(
            idempotency_key,
            "request abandoned by the caller after dispatch; the business operation was not cancelled",
        )
    } else {
        ContractError::new(
            CODE_CONNECTION_FAILED,
            "request abandoned by the caller before completion",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    };
    JsonRpcResponse::err(
        Some(id.to_owned()),
        agentcpm_pd_protocol::errors::ERR_REQUEST_ABANDONED,
        "request abandoned",
        Some(serde_json::to_value(&contract).unwrap_or(Value::Null)),
    )
}

fn shutdown_pending_reply(
    id: &str,
    is_write: bool,
    idempotency_key: Option<&str>,
) -> JsonRpcResponse {
    let contract = if is_write {
        ContractError::result_unknown(
            idempotency_key,
            "CLI shut down while the request was still in flight",
        )
    } else {
        ContractError::new(
            CODE_CONNECTION_FAILED,
            "CLI shut down before the read completed",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    };
    JsonRpcResponse::application_error(id, &contract)
}

fn conn_state_frame(
    state: ConnStateKind,
    reason: Option<&str>,
    instance_id: Option<&str>,
) -> String {
    let notif = JsonRpcNotification {
        jsonrpc: JSONRPC_VERSION.to_owned(),
        method: WIRE_CONN_STATE.to_owned(),
        params: serde_json::to_value(ConnStateNotification {
            state,
            reason: reason.map(ToOwned::to_owned),
            instance_id: instance_id.map(ToOwned::to_owned),
        })
        .unwrap_or(Value::Null),
    };
    encode_ndjson_line(&notif).unwrap_or_default()
}

fn sub_end_frame(
    subscription_id: &str,
    terminal: SubTerminal,
    resume_cursor: Option<&str>,
    error: Option<&ContractError>,
) -> String {
    let notif = JsonRpcNotification {
        jsonrpc: JSONRPC_VERSION.to_owned(),
        method: WIRE_SUB_END.to_owned(),
        params: serde_json::to_value(SubEndNotification {
            subscription_id: subscription_id.to_owned(),
            terminal,
            resume_cursor: resume_cursor.map(ToOwned::to_owned),
            gap: None,
            error: error.cloned(),
        })
        .unwrap_or(Value::Null),
    };
    encode_ndjson_line(&notif).unwrap_or_default()
}

/// Resolves on the first shutdown signal: `(exit_code, reason)`.
async fn shutdown_signal_future() -> (i32, &'static str) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => (EXIT_INTERRUPTED, "SIGINT"),
            _ = term.recv() => (EXIT_OK, "SIGTERM"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        (EXIT_INTERRUPTED, "SIGINT")
    }
}

fn backoff_with_jitter(attempt: usize) -> Duration {
    let base =
        RECONNECT_BACKOFF_MS[(attempt.saturating_sub(1)).min(RECONNECT_BACKOFF_MS.len() - 1)];
    let jitter = now_ms() % 250;
    Duration::from_millis(base + jitter)
}

async fn connect_upstream_with_retries(
    opts: &PdClientOptions,
    max_attempts: usize,
) -> Result<PdClient, ClientError> {
    let mut last = ClientError::connection_failed("no attempts executed");
    for attempt in 0..max_attempts.max(1) {
        match PdClient::connect(opts.clone()).await {
            Ok(client) => return Ok(client),
            Err(err) => {
                let fatal = matches!(
                    &err,
                    ClientError::Usage(_)
                        | ClientError::Config(_)
                        | ClientError::NotInitialized(_)
                        | ClientError::InstanceChanged { .. }
                );
                if fatal {
                    return Err(err);
                }
                last = err;
                if attempt + 1 < max_attempts {
                    let wait = 500u64.saturating_mul(1u64 << attempt.min(3));
                    tokio::time::sleep(Duration::from_millis(wait.min(2_000))).await;
                }
            }
        }
    }
    Err(last)
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

struct PendingReq {
    is_control: bool,
    is_write: bool,
    idempotency_key: Option<String>,
    handle: JoinHandle<()>,
}

/// Engine bookkeeping events from spawned request/subscription tasks.
enum DoneMsg {
    Request { id: String, is_control: bool },
    SubscriptionEnded { id: String },
}

struct SubAnnouncement {
    subscription_id: String,
    last_cursor: Arc<std::sync::Mutex<Option<String>>>,
    closed_by_host: Arc<AtomicBool>,
    credit_tx: mpsc::Sender<u32>,
    forwarder: JoinHandle<()>,
}

struct Engine {
    out: OutQueues,
    done_tx: mpsc::Sender<DoneMsg>,
    sub_kill: watch::Sender<u64>,
    sub_kill_rx: watch::Receiver<u64>,
    sub_announce_tx: mpsc::Sender<SubAnnouncement>,
    initialized: Arc<AtomicBool>,
    shutting_down: Arc<AtomicBool>,
    ws: PdWsClient,
    bound_instance_id: String,
    client_opts: PdClientOptions,
    pending: HashMap<String, PendingReq>,
    pending_control: usize,
    pending_normal: usize,
    subscriptions: HashMap<String, SubAnnouncement>,
    sub_generation: u64,
    reconnecting: bool,
    reconnect_in_flight: bool,
    reconnect_attempt: usize,
    next_reconnect_at: Option<tokio::time::Instant>,
    reconnect_tx: mpsc::Sender<Result<PdClient, ClientError>>,
    log_format: LogFormat,
}

struct ShutdownState {
    exit_code: i32,
    deadline: tokio::time::Instant,
}

impl Engine {
    fn diag(&self, message: String) {
        match self.log_format {
            LogFormat::Json => eprintln!("{}", json!({"level": "error", "message": message})),
            LogFormat::Text => eprintln!("{BIN_NAME}: {message}"),
        }
    }

    fn gate(&self) -> RequestGate {
        RequestGate {
            initialized: self.initialized.load(Ordering::SeqCst),
            bound_instance_id: self.bound_instance_id.clone(),
            pending_ids: self.pending.keys().cloned().collect(),
            pending_control: self.pending_control,
            pending_normal: self.pending_normal,
            subscriptions: self.subscriptions.len(),
        }
    }

    async fn push_ctrl(&self, frame: String) {
        let _ = self.out.ctrl.send(frame).await;
    }

    async fn send_response(&self, resp: &JsonRpcResponse) {
        if let Ok(line) = encode_ndjson_line(resp) {
            self.push_ctrl(line).await;
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run(
        &mut self,
        mut reader_rx: mpsc::Receiver<ReaderMsg>,
        mut done_rx: mpsc::Receiver<DoneMsg>,
        mut sub_announce_rx: mpsc::Receiver<SubAnnouncement>,
        mut reconnect_rx: mpsc::Receiver<Result<PdClient, ClientError>>,
        mut writer_died_rx: oneshot::Receiver<()>,
        reader_handle: JoinHandle<()>,
    ) -> i32 {
        let mut shutdown_signal = std::pin::pin!(shutdown_signal_future());
        let mut watchdog = tokio::time::interval(WATCHDOG_INTERVAL);
        watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut shutdown: Option<ShutdownState> = None;
        let mut reader_alive = true;

        loop {
            tokio::select! {
                biased;
                sig = &mut shutdown_signal, if shutdown.is_none() => {
                    self.begin_shutdown(
                        &mut shutdown,
                        &reader_handle,
                        sig.0,
                        sig.1,
                    );
                }
                _ = &mut writer_died_rx => {
                    self.diag("stdout writer failed; exiting".to_owned());
                    self.finish_shutdown().await;
                    return EXIT_FAILED;
                }
                msg = reader_rx.recv(), if reader_alive => match msg {
                    Some(ReaderMsg::Frame(frame)) => {
                        self.handle_frame(frame, &shutdown).await;
                    }
                    Some(ReaderMsg::ErrorResponse(resp)) => {
                        self.send_response(&resp).await;
                    }
                    Some(ReaderMsg::Eof) | None => {
                        reader_alive = false;
                        self.begin_shutdown(&mut shutdown, &reader_handle, EXIT_OK, "stdin EOF");
                    }
                },
                done = done_rx.recv() => match done {
                    Some(DoneMsg::Request { id, is_control }) => {
                        if self.pending.remove(&id).is_some() {
                            if is_control {
                                self.pending_control =
                                    self.pending_control.saturating_sub(1);
                            } else {
                                self.pending_normal =
                                    self.pending_normal.saturating_sub(1);
                            }
                        }
                    }
                    Some(DoneMsg::SubscriptionEnded { id }) => {
                        self.subscriptions.remove(&id);
                    }
                    None => {}
                },
                ann = sub_announce_rx.recv() => {
                    if let Some(ann) = ann {
                        self.subscriptions
                            .insert(ann.subscription_id.clone(), ann);
                    }
                }
                res = reconnect_rx.recv() => {
                    if let Some(res) = res {
                        self.handle_reconnect_result(res, &mut shutdown).await;
                    }
                }
                _ = watchdog.tick() => {
                    self.watchdog_tick().await;
                }
            }

            if let Some(state) = &shutdown
                && (self.pending.is_empty() || tokio::time::Instant::now() >= state.deadline)
            {
                break;
            }
        }

        let exit_code = shutdown
            .as_ref()
            .map(|state| state.exit_code)
            .unwrap_or(EXIT_OK);
        self.finish_shutdown().await;
        exit_code
    }

    fn begin_shutdown(
        &mut self,
        shutdown: &mut Option<ShutdownState>,
        reader_handle: &JoinHandle<()>,
        exit_code: i32,
        reason: &str,
    ) {
        if shutdown.is_none() {
            self.diag(format!(
                "draining after {reason}; in-flight requests get up to {EOF_DRAIN_MS}ms"
            ));
            *shutdown = Some(ShutdownState {
                exit_code,
                deadline: tokio::time::Instant::now() + Duration::from_millis(EOF_DRAIN_MS),
            });
            self.shutting_down.store(true, Ordering::SeqCst);
            // Stop receiving new requests. Runs are never cancelled here (D4).
            reader_handle.abort();
        }
    }

    async fn finish_shutdown(&mut self) {
        // Answer requests still in flight after the drain window.
        let pending = std::mem::take(&mut self.pending);
        self.pending_control = 0;
        self.pending_normal = 0;
        for (id, req) in pending {
            req.handle.abort();
            let frame = shutdown_pending_reply(&id, req.is_write, req.idempotency_key.as_deref());
            if let Ok(line) = encode_ndjson_line(&frame) {
                let _ = self.out.ctrl.try_send(line);
            }
        }
        // End all subscriptions with `shutdown` and their resume cursors.
        let subscriptions = std::mem::take(&mut self.subscriptions);
        self.sub_generation = self.sub_generation.wrapping_add(1);
        let _ = self.sub_kill.send_replace(self.sub_generation);
        for (sub_id, sub) in subscriptions {
            sub.forwarder.abort();
            let cursor = sub.last_cursor.lock().unwrap().clone();
            let frame = sub_end_frame(&sub_id, SubTerminal::Shutdown, cursor.as_deref(), None);
            let _ = self.out.ctrl.try_send(frame);
        }
        self.shutting_down.store(true, Ordering::SeqCst);
    }

    async fn watchdog_tick(&mut self) {
        if !self.reconnecting && self.ws.is_closed() {
            self.begin_reconnect().await;
            return;
        }
        if self.reconnecting
            && !self.reconnect_in_flight
            && self
                .next_reconnect_at
                .is_some_and(|at| tokio::time::Instant::now() >= at)
        {
            self.reconnect_in_flight = true;
            let opts = self.client_opts.clone();
            let tx = self.reconnect_tx.clone();
            tokio::spawn(async move {
                let result = PdClient::connect(opts).await;
                let _ = tx.send(result).await;
            });
        }
    }

    async fn begin_reconnect(&mut self) {
        self.reconnecting = true;
        self.reconnect_attempt = 1;
        self.reconnect_in_flight = false;
        self.next_reconnect_at =
            Some(tokio::time::Instant::now() + backoff_with_jitter(self.reconnect_attempt));

        // End every subscription with `disconnected` plus its resume cursor.
        let subscriptions = std::mem::take(&mut self.subscriptions);
        self.sub_generation = self.sub_generation.wrapping_add(1);
        let _ = self.sub_kill.send_replace(self.sub_generation);
        for (sub_id, sub) in subscriptions {
            sub.forwarder.abort();
            let cursor = sub.last_cursor.lock().unwrap().clone();
            let frame = sub_end_frame(&sub_id, SubTerminal::Disconnected, cursor.as_deref(), None);
            let _ = self.out.ctrl.try_send(frame);
        }
        // In-flight requests were already failed by the ws client internals
        // (reads CONNECTION_FAILED / not_applied, writes RESULT_UNKNOWN / unknown).

        self.push_ctrl(conn_state_frame(
            ConnStateKind::Reconnecting,
            None,
            Some(&self.bound_instance_id),
        ))
        .await;
    }

    async fn handle_reconnect_result(
        &mut self,
        result: Result<PdClient, ClientError>,
        shutdown: &mut Option<ShutdownState>,
    ) {
        self.reconnect_in_flight = false;
        match result {
            Ok(client) => {
                let new_instance = client.ws().bound_instance_id().to_owned();
                if new_instance != self.bound_instance_id {
                    self.instance_changed_fatal(&new_instance, shutdown).await;
                    return;
                }
                let old = std::mem::replace(&mut self.ws, client.ws().clone());
                old.close().await;
                self.reconnecting = false;
                self.reconnect_attempt = 0;
                self.next_reconnect_at = None;
                self.push_ctrl(conn_state_frame(
                    ConnStateKind::Connected,
                    None,
                    Some(&self.bound_instance_id),
                ))
                .await;
            }
            Err(ClientError::InstanceChanged { expected, actual }) => {
                let _ = expected;
                self.instance_changed_fatal(&actual, shutdown).await;
            }
            Err(err) => {
                self.reconnect_attempt += 1;
                self.next_reconnect_at =
                    Some(tokio::time::Instant::now() + backoff_with_jitter(self.reconnect_attempt));
                self.diag(format!(
                    "reconnect attempt {} failed: {err}",
                    self.reconnect_attempt
                ));
                self.push_ctrl(conn_state_frame(
                    ConnStateKind::Reconnecting,
                    None,
                    Some(&self.bound_instance_id),
                ))
                .await;
            }
        }
    }

    async fn instance_changed_fatal(&mut self, actual: &str, shutdown: &mut Option<ShutdownState>) {
        self.push_ctrl(conn_state_frame(
            ConnStateKind::Failed,
            Some("instance_changed"),
            Some(actual),
        ))
        .await;
        self.diag(format!(
            "gateway instance changed from `{}` to `{actual}`; refusing to continue",
            self.bound_instance_id
        ));
        *shutdown = Some(ShutdownState {
            exit_code: EXIT_FAILED,
            deadline: tokio::time::Instant::now(),
        });
    }

    async fn handle_frame(&mut self, frame: JsonRpcFrame, shutdown: &Option<ShutdownState>) {
        match frame {
            JsonRpcFrame::Response(resp) => {
                let error = JsonRpcResponse::err(
                    resp.id.clone(),
                    agentcpm_pd_protocol::errors::JSONRPC_INVALID_REQUEST,
                    "unexpected response frame: this CLI does not issue host requests",
                    None,
                );
                self.send_response(&error).await;
            }
            JsonRpcFrame::Notification(notif) => match notif.method.as_str() {
                WIRE_SUB_CREDIT => {
                    if let Ok(params) = serde_json::from_value::<SubCreditParams>(notif.params) {
                        self.handle_sub_credit(None, params).await;
                    }
                }
                WIRE_SUB_CLOSE => {
                    if let Ok(params) = serde_json::from_value::<SubCloseParams>(notif.params) {
                        self.handle_sub_close(None, params).await;
                    }
                }
                _ => {}
            },
            JsonRpcFrame::Request(req) => {
                self.handle_request(req, shutdown).await;
            }
        }
    }

    async fn handle_request(&mut self, req: JsonRpcRequest, shutdown: &Option<ShutdownState>) {
        let initialized = self.initialized.load(Ordering::SeqCst);

        if !initialized && req.method != WIRE_INITIALIZE {
            let resp = JsonRpcResponse::err(
                Some(req.id.clone()),
                agentcpm_pd_protocol::errors::ERR_NOT_INITIALIZED,
                "initialize must be the first request",
                None,
            );
            self.send_response(&resp).await;
            return;
        }

        if shutdown.is_some() {
            let resp = JsonRpcResponse::application_error(
                &req.id,
                &ContractError::new(
                    CODE_CONNECTION_FAILED,
                    "CLI is shutting down; new requests are not accepted",
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ),
            );
            self.send_response(&resp).await;
            return;
        }

        match req.method.as_str() {
            WIRE_INITIALIZE => {
                let connection_id = IdGenerator::new(NodeId::local()).next_id();
                let resp = build_initialize_response(
                    &req,
                    initialized,
                    self.ws.target_info(),
                    &connection_id,
                );
                if resp.error.is_none() {
                    self.initialized.store(true, Ordering::SeqCst);
                }
                self.send_response(&resp).await;
            }
            WIRE_SUB_OPEN => {
                let result = prepare_sub_open(&req, &self.gate());
                match result {
                    Ok(prepared) => {
                        self.forward_sub_open(req, prepared).await;
                    }
                    Err(resp) => {
                        self.send_response(&resp).await;
                    }
                }
            }
            WIRE_SUB_CREDIT => {
                match serde_json::from_value::<SubCreditParams>(req.params.clone()) {
                    Ok(params) => self.handle_sub_credit(Some(req.id.clone()), params).await,
                    Err(e) => {
                        let resp = invalid_params(&req.id, format!("sub.credit params: {e}"), None);
                        self.send_response(&resp).await;
                    }
                }
            }
            WIRE_SUB_CLOSE => match serde_json::from_value::<SubCloseParams>(req.params.clone()) {
                Ok(params) => self.handle_sub_close(Some(req.id.clone()), params).await,
                Err(e) => {
                    let resp = invalid_params(&req.id, format!("sub.close params: {e}"), None);
                    self.send_response(&resp).await;
                }
            },
            WIRE_CONN_ABANDON => {
                match serde_json::from_value::<ConnAbandonParams>(req.params.clone()) {
                    Ok(params) => self.handle_abandon(req, params).await,
                    Err(e) => {
                        let resp =
                            invalid_params(&req.id, format!("conn.abandon params: {e}"), None);
                        self.send_response(&resp).await;
                    }
                }
            }
            WIRE_CONN_HEARTBEAT => {
                // Host heartbeat: acknowledged implicitly.
                let resp = JsonRpcResponse::ok(req.id, json!({}));
                self.send_response(&resp).await;
            }
            _ => {
                let prepared = if self.reconnecting {
                    PreparedRequest::Rejected(JsonRpcResponse::application_error(
                        &req.id,
                        &ContractError::new(
                            CODE_CONNECTION_FAILED,
                            "gateway connection is reconnecting; retry after conn.state ready",
                            RetrySpec::backoff_read(Some(500)),
                            EffectStatus::NotApplied,
                        ),
                    ))
                } else {
                    prepare_request(&req, &self.gate())
                };
                match prepared {
                    PreparedRequest::LocalSchema { method, input } => {
                        self.handle_local_schema(req, &method, &input).await;
                    }
                    PreparedRequest::Forward {
                        method,
                        params,
                        timeout,
                        is_control,
                        is_write,
                    } => {
                        self.spawn_forwarder(req, method, params, timeout, is_control, is_write)
                            .await;
                    }
                    PreparedRequest::Rejected(resp) => {
                        self.send_response(&resp).await;
                    }
                }
            }
        }
    }

    async fn handle_local_schema(&mut self, req: JsonRpcRequest, method: &str, input: &Value) {
        let reg = OperationRegistry::global();
        let result = match method {
            "schema.list" => reg.handle_schema_list(input),
            "schema.get" => reg.handle_schema_get(input),
            other => Err(ContractError::capability_unavailable(
                other,
                "only schema.list and schema.get are local on this connection",
            )),
        };
        let resp = match result {
            Ok(value) => {
                let rpc_result = OperationRpcResult {
                    outcome: ResponseOutcome::Succeeded,
                    result: value,
                };
                JsonRpcResponse::ok(
                    req.id,
                    serde_json::to_value(rpc_result).unwrap_or(Value::Null),
                )
            }
            Err(err) => JsonRpcResponse::application_error(&req.id, &err),
        };
        self.send_response(&resp).await;
    }

    async fn spawn_forwarder(
        &mut self,
        req: JsonRpcRequest,
        method: String,
        params: OperationRpcParams,
        timeout: Duration,
        is_control: bool,
        is_write: bool,
    ) {
        let ws = self.ws.clone();
        let queue = if is_control {
            self.out.ctrl.clone()
        } else {
            self.out.norm.clone()
        };
        let done = self.done_tx.clone();
        let id = req.id.clone();
        let idempotency_key = params.idempotency_key.clone();
        let rpc_params = serde_json::to_value(&params).unwrap_or(Value::Null);
        let forward_method = method;

        let handle = tokio::spawn(async move {
            let result = ws
                .send_raw_rpc(id.clone(), &forward_method, rpc_params, timeout, is_write)
                .await;
            let frame = match result {
                Ok(value) => JsonRpcResponse::ok(id.clone(), value),
                Err(err) => error_response_for(&id, &err),
            };
            if let Ok(line) = encode_ndjson_line(&frame) {
                let _ = queue.send(line).await;
            }
            let _ = done.send(DoneMsg::Request { id, is_control }).await;
        });

        self.pending.insert(
            req.id.clone(),
            PendingReq {
                is_control,
                is_write,
                idempotency_key,
                handle,
            },
        );
        if is_control {
            self.pending_control += 1;
        } else {
            self.pending_normal += 1;
        }
    }

    async fn forward_sub_open(&mut self, req: JsonRpcRequest, prepared: PreparedSubOpen) {
        let ws = self.ws.clone();
        let ctrl = self.out.ctrl.clone();
        let evt = self.out.evt.clone();
        let sub_kill = self.sub_kill_rx.clone();
        let announce = self.sub_announce_tx.clone();
        let done = self.done_tx.clone();
        let method = prepared.method;
        let input = prepared.input;
        let credit = prepared.credit;
        let request_id = req.id.clone();
        let log_format = self.log_format;
        let handle = tokio::spawn(async move {
            match ws
                .open_subscription(&method, input, Some(credit), false)
                .await
            {
                Ok(subscription) => {
                    let subscription_id = subscription.subscription_id.clone();
                    let result = SubOpenResult {
                        subscription_id: subscription_id.clone(),
                    };
                    let resp = JsonRpcResponse::ok(
                        request_id.clone(),
                        serde_json::to_value(result).unwrap_or(Value::Null),
                    );
                    if let Ok(line) = encode_ndjson_line(&resp) {
                        let _ = ctrl.send(line).await;
                    }
                    let (credit_tx, credit_rx) = mpsc::channel::<u32>(64);
                    let last_cursor: Arc<std::sync::Mutex<Option<String>>> =
                        Arc::new(std::sync::Mutex::new(None));
                    let closed_by_host = Arc::new(AtomicBool::new(false));
                    let forwarder = tokio::spawn(sub_forwarder(
                        subscription_id.clone(),
                        subscription,
                        evt,
                        ctrl.clone(),
                        credit_rx,
                        sub_kill,
                        last_cursor.clone(),
                        closed_by_host.clone(),
                        done.clone(),
                        log_format,
                    ));
                    let _ = announce
                        .send(SubAnnouncement {
                            subscription_id,
                            last_cursor,
                            closed_by_host,
                            credit_tx,
                            forwarder,
                        })
                        .await;
                }
                Err(err) => {
                    let resp = error_response_for(&request_id, &err);
                    if let Ok(line) = encode_ndjson_line(&resp) {
                        let _ = ctrl.send(line).await;
                    }
                }
            }
            let _ = done
                .send(DoneMsg::Request {
                    id: request_id,
                    is_control: true,
                })
                .await;
        });
        // The sub.open counts as an in-flight control request so the EOF drain
        // waits for its response before exiting.
        self.pending.insert(
            req.id,
            PendingReq {
                is_control: true,
                is_write: false,
                idempotency_key: None,
                handle,
            },
        );
        self.pending_control += 1;
    }

    async fn handle_sub_credit(&mut self, reply_to: Option<String>, params: SubCreditParams) {
        let Some(sub) = self.subscriptions.get(&params.subscription_id) else {
            if let Some(id) = reply_to {
                let resp = invalid_params(
                    &id,
                    format!("unknown subscriptionId `{}`", params.subscription_id),
                    None,
                );
                self.send_response(&resp).await;
            }
            return;
        };
        if params.add == 0 {
            if let Some(id) = reply_to {
                let resp = invalid_params(&id, "credit `add` must be greater than zero", None);
                self.send_response(&resp).await;
            }
            return;
        }
        let credit_tx = sub.credit_tx.clone();
        let sent = credit_tx.send(params.add).await.is_ok();
        if !sent {
            if let Some(id) = reply_to {
                let resp = error_response_for(
                    &id,
                    &ClientError::connection_failed("subscription forwarder is gone"),
                );
                self.send_response(&resp).await;
            }
            return;
        }
        if let Some(id) = reply_to {
            let resp = JsonRpcResponse::ok(id, json!({}));
            self.send_response(&resp).await;
        }
    }

    async fn handle_sub_close(&mut self, reply_to: Option<String>, params: SubCloseParams) {
        let Some(sub) = self.subscriptions.remove(&params.subscription_id) else {
            if let Some(id) = reply_to {
                let resp = invalid_params(
                    &id,
                    format!("unknown subscriptionId `{}`", params.subscription_id),
                    None,
                );
                self.send_response(&resp).await;
            }
            return;
        };
        sub.closed_by_host.store(true, Ordering::SeqCst);
        let ws = self.ws.clone();
        let subscription_id = params.subscription_id;
        tokio::spawn(async move {
            let _ = ws.send_sub_close(&subscription_id).await;
        });
        if let Some(id) = reply_to {
            let resp = JsonRpcResponse::ok(id, json!({}));
            self.send_response(&resp).await;
        }
    }

    async fn handle_abandon(&mut self, req: JsonRpcRequest, params: ConnAbandonParams) {
        match self.pending.remove(&params.id) {
            Some(pending) => {
                let is_write = pending.is_write;
                let key = pending.idempotency_key.clone();
                let is_control = pending.is_control;
                pending.handle.abort();
                // Reply to the ORIGINAL request id with -32800.
                let original = abandon_reply(&params.id, is_write, key.as_deref());
                if let Ok(line) = encode_ndjson_line(&original) {
                    self.push_ctrl(line).await;
                }
                if is_control {
                    self.pending_control = self.pending_control.saturating_sub(1);
                } else {
                    self.pending_normal = self.pending_normal.saturating_sub(1);
                }
                let resp = JsonRpcResponse::ok(req.id, json!({}));
                self.send_response(&resp).await;
            }
            None => {
                let resp = JsonRpcResponse::err(
                    Some(req.id),
                    agentcpm_pd_protocol::errors::JSONRPC_INVALID_REQUEST,
                    format!("unknown request id `{}` to abandon", params.id),
                    None,
                );
                self.send_response(&resp).await;
            }
        }
    }
}

/// Forwards upstream subscription events to the host with per-subscription
/// cursor tracking, slow-consumer termination, and oversized-event handling.
#[allow(clippy::too_many_arguments)]
async fn sub_forwarder(
    subscription_id: String,
    mut subscription: PdWsSubscription,
    evt: mpsc::Sender<String>,
    ctrl: mpsc::Sender<String>,
    mut credit_rx: mpsc::Receiver<u32>,
    mut kill: watch::Receiver<u64>,
    last_cursor: Arc<std::sync::Mutex<Option<String>>>,
    closed_by_host: Arc<AtomicBool>,
    done: mpsc::Sender<DoneMsg>,
    _log_format: LogFormat,
) {
    loop {
        tokio::select! {
            biased;
            _ = kill.changed() => break,
            grant = credit_rx.recv() => {
                if let Some(add) = grant {
                    let _ = subscription.grant_credit(add).await;
                }
            }
            item = subscription.recv() => match item {
                Some(SubStreamItem::Event(ev)) => {
                    *last_cursor.lock().unwrap() = Some(ev.event.cursor.clone());
                    let notif = JsonRpcNotification {
                        jsonrpc: JSONRPC_VERSION.to_owned(),
                        method: WIRE_SUB_EVENT.to_owned(),
                        params: serde_json::to_value(SubEventNotification {
                            subscription_id: subscription_id.clone(),
                            event: ev.event,
                        })
                        .unwrap_or(Value::Null),
                    };
                    let Ok(line) = encode_ndjson_line(&notif) else {
                        continue;
                    };
                    if line.len() > MAX_FRAME_BYTES {
                        let error = ContractError::new(
                            "INPUT_TOO_LARGE",
                            "event exceeded the 1 MiB frame cap",
                            RetrySpec::never(),
                            EffectStatus::NotApplied,
                        );
                        let end = sub_end_frame(
                            &subscription_id,
                            SubTerminal::Error,
                            last_cursor.lock().unwrap().clone().as_deref(),
                            Some(&error),
                        );
                        let _ = ctrl.send(end).await;
                        break;
                    }
                    if evt.try_send(line).is_err() {
                        let end = sub_end_frame(
                            &subscription_id,
                            SubTerminal::SlowConsumer,
                            last_cursor.lock().unwrap().clone().as_deref(),
                            None,
                        );
                        let _ = ctrl.send(end).await;
                        break;
                    }
                }
                Some(SubStreamItem::End(end)) => {
                    // Relay the upstream terminal through the event lane: it
                    // must not overtake this subscription's queued events.
                    // Synthesized ends below stay on the control lane (the
                    // design's slow-consumer jump).
                    let frame = sub_end_frame(
                        &subscription_id,
                        end.terminal,
                        end.resume_cursor.as_deref(),
                        end.error.as_ref(),
                    );
                    if evt.send(frame).await.is_err() {
                        let cursor = last_cursor.lock().unwrap().clone();
                        let _ = ctrl
                            .send(sub_end_frame(
                                &subscription_id,
                                SubTerminal::Error,
                                cursor.as_deref(),
                                None,
                            ))
                            .await;
                    }
                    break;
                }
                None => {
                    let terminal = if closed_by_host.load(Ordering::SeqCst) {
                        SubTerminal::Closed
                    } else {
                        SubTerminal::Disconnected
                    };
                    let frame = sub_end_frame(
                        &subscription_id,
                        terminal,
                        last_cursor.lock().unwrap().clone().as_deref(),
                        None,
                    );
                    let _ = ctrl.send(frame).await;
                    break;
                }
            },
        }
    }
    let _ = done
        .send(DoneMsg::SubscriptionEnded {
            id: subscription_id,
        })
        .await;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use agentcpm_pd_protocol::wire::ClientDescriptor;

    fn gate() -> RequestGate {
        RequestGate {
            initialized: true,
            bound_instance_id: "local-019a0000-0000-7000-8000-000000000001".to_owned(),
            pending_ids: HashSet::new(),
            pending_control: 0,
            pending_normal: 0,
            subscriptions: 0,
        }
    }

    fn rpc_request(id: &str, method: &str, params: Value) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: id.to_owned(),
            method: method.to_owned(),
            params,
        }
    }

    fn error_code(resp: &JsonRpcResponse) -> i64 {
        resp.error.as_ref().map(|e| e.code).unwrap_or(0)
    }

    #[tokio::test]
    async fn read_line_capped_recovers_after_oversized_lines() {
        let normal = b"{\"jsonrpc\":\"2.0\"}\n".to_vec();
        let huge = vec![b'a'; MAX_FRAME_BYTES + 10];
        let mut data = Vec::new();
        data.extend_from_slice(&huge);
        data.push(b'\n');
        data.extend_from_slice(&normal);
        let mut reader = tokio::io::BufReader::new(data.as_slice());

        let (line, truncated, total) = read_line_capped(&mut reader, MAX_FRAME_BYTES)
            .await
            .unwrap()
            .unwrap();
        assert!(truncated);
        assert!(line.is_empty());
        assert_eq!(total, MAX_FRAME_BYTES + 11);

        // The connection stays usable: the next line parses.
        let (line, truncated, total) = read_line_capped(&mut reader, MAX_FRAME_BYTES)
            .await
            .unwrap()
            .unwrap();
        assert!(!truncated);
        assert_eq!(total, normal.len());
        assert!(decode_ndjson_line(&line).is_ok());

        // EOF afterwards.
        assert!(
            read_line_capped(&mut reader, MAX_FRAME_BYTES)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn read_line_capped_partial_last_line_without_newline() {
        let mut reader = tokio::io::BufReader::new(&b"no-newline"[..]);
        let (line, truncated, total) = read_line_capped(&mut reader, MAX_FRAME_BYTES)
            .await
            .unwrap()
            .unwrap();
        assert!(!truncated);
        assert_eq!(total, 10);
        assert_eq!(line, b"no-newline");
    }

    #[test]
    fn prepare_request_rejects_pre_init_duplicate_unknown_and_local_lifecycle() {
        // pre-initialize rejection (-32002) lives in the engine loop, which
        // consults the same gate before dispatch.

        let mut g = gate();
        g.pending_ids.insert("r1".to_owned());
        match prepare_request(
            &rpc_request("r1", "system.health", json!({"input": {}})),
            &g,
        ) {
            PreparedRequest::Rejected(resp) => {
                assert_eq!(error_code(&resp), -32600);
                assert!(resp.error.unwrap().message.contains("already in flight"));
            }
            other => panic!("expected duplicate rejection, got {other:?}"),
        }

        // unknown method -> -32601 with UNSUPPORTED_OPERATION data
        match prepare_request(
            &rpc_request("r1", "no.such.method", json!({"input": {}})),
            &gate(),
        ) {
            PreparedRequest::Rejected(resp) => {
                assert_eq!(error_code(&resp), -32601);
                assert_eq!(
                    resp.error.unwrap().data.unwrap()["code"],
                    "UNSUPPORTED_OPERATION"
                );
            }
            other => panic!("expected unknown method rejection, got {other:?}"),
        }

        // local lifecycle (service.status) -> -32601 with CAPABILITY_UNAVAILABLE
        match prepare_request(
            &rpc_request("r1", "service.status", json!({"input": {}})),
            &gate(),
        ) {
            PreparedRequest::Rejected(resp) => {
                assert_eq!(error_code(&resp), -32601);
                assert_eq!(
                    resp.error.unwrap().data.unwrap()["code"],
                    "CAPABILITY_UNAVAILABLE"
                );
            }
            other => panic!("expected local rejection, got {other:?}"),
        }

        // instance.initialize and schema.export (local, not read-only schema) rejected too
        for method in ["instance.initialize", "schema.export"] {
            match prepare_request(&rpc_request("r1", method, json!({"input": {}})), &gate()) {
                PreparedRequest::Rejected(resp) => assert_eq!(error_code(&resp), -32601),
                other => panic!("expected rejection for {method}, got {other:?}"),
            }
        }
    }

    #[test]
    fn prepare_request_serves_local_schema_and_validates_input() {
        match prepare_request(
            &rpc_request(
                "r1",
                "schema.list",
                json!({"input": {"page": {"limit": 2}}}),
            ),
            &gate(),
        ) {
            PreparedRequest::LocalSchema { method, input } => {
                assert_eq!(method, "schema.list");
                assert_eq!(input["page"]["limit"], json!(2));
            }
            other => panic!("expected local schema, got {other:?}"),
        }

        // schema violation -> -32602 with details.path
        match prepare_request(
            &rpc_request("r1", "schema.list", json!({"input": {"page": 5}})),
            &gate(),
        ) {
            PreparedRequest::Rejected(resp) => {
                assert_eq!(error_code(&resp), -32602);
                let error = resp.error.unwrap();
                let data = error.data.unwrap();
                assert!(data["details"]["path"].as_str().unwrap().contains("page"));
            }
            other => panic!("expected schema rejection, got {other:?}"),
        }
    }

    #[test]
    fn prepare_request_enforces_idempotency_and_instance_binding() {
        // run.submit without a key -> -32086 (T3)
        match prepare_request(
            &rpc_request(
                "r1",
                "run.submit",
                json!({"input": {"sessionId": "s1", "input": [{"kind": "text", "text": "hi"}]}}),
            ),
            &gate(),
        ) {
            PreparedRequest::Rejected(resp) => assert_eq!(error_code(&resp), -32086),
            other => panic!("expected missing-key rejection, got {other:?}"),
        }

        // with a key -> forwarded with control=false, write=true
        match prepare_request(
            &rpc_request(
                "r1",
                "run.submit",
                json!({
                    "input": {"sessionId": "s1", "input": [{"kind": "text", "text": "hi"}]},
                    "idempotencyKey": "key-1"
                }),
            ),
            &gate(),
        ) {
            PreparedRequest::Forward {
                method,
                params,
                is_control,
                is_write,
                ..
            } => {
                assert_eq!(method, "run.submit");
                assert!(is_write);
                assert!(!is_control);
                assert_eq!(params.idempotency_key.as_deref(), Some("key-1"));
                // instanceId auto-injected because run.submit declares it
                assert_eq!(
                    params.input["instanceId"],
                    json!("local-019a0000-0000-7000-8000-000000000001")
                );
            }
            other => panic!("expected forward, got {other:?}"),
        }

        // control class + instance mismatch: run.get with wrong instanceId
        match prepare_request(
            &rpc_request(
                "r1",
                "run.get",
                json!({"input": {"runId": "r", "instanceId": "other-1"}}),
            ),
            &gate(),
        ) {
            PreparedRequest::Rejected(resp) => {
                assert_eq!(error_code(&resp), -32000);
                assert_eq!(
                    resp.error.unwrap().data.unwrap()["code"],
                    "INSTANCE_MISMATCH"
                );
            }
            other => panic!("expected mismatch rejection, got {other:?}"),
        }
    }

    #[test]
    fn prepare_request_enforces_class_limits() {
        let mut g = gate();
        g.pending_control = MAX_CONTROL_PENDING;
        match prepare_request(
            &rpc_request("r1", "run.get", json!({"input": {"runId": "r"}})),
            &g,
        ) {
            PreparedRequest::Rejected(resp) => {
                assert_eq!(error_code(&resp), -32600);
                assert!(resp.error.unwrap().message.contains("control"));
            }
            other => panic!("expected control limit rejection, got {other:?}"),
        }

        let mut g = gate();
        g.pending_normal = MAX_PENDING;
        match prepare_request(
            &rpc_request("r1", "system.health", json!({"input": {}})),
            &g,
        ) {
            PreparedRequest::Rejected(resp) => {
                assert_eq!(error_code(&resp), -32600);
                assert!(resp.error.unwrap().message.contains("normal"));
            }
            other => panic!("expected normal limit rejection, got {other:?}"),
        }
    }

    #[test]
    fn prepare_sub_open_forces_follow_and_rejects_page_and_non_stream() {
        // non-stream method -> -32602
        let err = prepare_sub_open(
            &rpc_request(
                "r1",
                WIRE_SUB_OPEN,
                json!({"stream": "system.health", "input": {}}),
            ),
            &gate(),
        )
        .unwrap_err();
        assert_eq!(error_code(&err), -32602);

        // page rejected
        let err = prepare_sub_open(
            &rpc_request(
                "r1",
                WIRE_SUB_OPEN,
                json!({
                    "stream": "run.events",
                    "input": {"runId": "r", "page": {"limit": 10}}
                }),
            ),
            &gate(),
        )
        .unwrap_err();
        assert_eq!(error_code(&err), -32602);
        assert!(err.error.unwrap().message.contains("page"));

        // missing required runId -> -32602 with path
        let err = prepare_sub_open(
            &rpc_request(
                "r1",
                WIRE_SUB_OPEN,
                json!({"stream": "run.events", "input": {}}),
            ),
            &gate(),
        )
        .unwrap_err();
        assert_eq!(error_code(&err), -32602);

        // valid: follow forced to true, instanceId injected, default credit
        let prepared = prepare_sub_open(
            &rpc_request(
                "r1",
                WIRE_SUB_OPEN,
                json!({"stream": "run.events", "input": {"runId": "r"}, "credit": 7}),
            ),
            &gate(),
        )
        .unwrap();
        assert_eq!(prepared.method, "run.events");
        assert_eq!(prepared.credit, 7);
        assert_eq!(prepared.input["follow"], json!(true));
        assert_eq!(
            prepared.input["instanceId"],
            json!("local-019a0000-0000-7000-8000-000000000001")
        );

        // default credit when omitted
        let prepared = prepare_sub_open(
            &rpc_request(
                "r1",
                WIRE_SUB_OPEN,
                json!({"stream": "run.events", "input": {"runId": "r"}}),
            ),
            &gate(),
        )
        .unwrap();
        assert_eq!(prepared.credit, DEFAULT_CREDIT);

        // subscription limit
        let mut g = gate();
        g.subscriptions = MAX_SUBSCRIPTIONS;
        let err = prepare_sub_open(
            &rpc_request(
                "r1",
                WIRE_SUB_OPEN,
                json!({"stream": "run.events", "input": {"runId": "r"}}),
            ),
            &g,
        )
        .unwrap_err();
        assert_eq!(error_code(&err), -32600);
    }

    #[test]
    fn build_initialize_response_reports_cli_info_and_version_errors() {
        let target = TargetHandshakeInfo {
            name: "local".to_owned(),
            instance_id: "local-1".to_owned(),
            node_id: "local".to_owned(),
            gateway_version: "0.1.0".to_owned(),
            schema_digest: "deadbeef".to_owned(),
            transport: "ws".to_owned(),
        };
        let req = rpc_request(
            "r1",
            WIRE_INITIALIZE,
            json!({
                "abi_version": 1,
                "api_version": OPERATION_VERSION,
                "features": ["subscriptions", "credit", "mystery"],
                "client": {"name": "host", "version": "1.0"}
            }),
        );
        let resp = build_initialize_response(&req, false, &target, "conn-1");
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(result["abi_version"], json!(1));
        assert_eq!(result["supported"], json!(true));
        assert_eq!(result["connectionId"], "conn-1");
        let cli = result["cli"].as_object().unwrap();
        assert_eq!(cli["pid"], json!(std::process::id()));
        assert!(cli["version"].as_str().is_some_and(|v| !v.is_empty()));
        assert!(cli["executable"].as_str().is_some_and(|v| !v.is_empty()));
        assert_eq!(result["target"]["instanceId"], "local-1");
        assert_eq!(
            result["supported_features"],
            json!(["subscriptions", "credit"])
        );
        assert_eq!(result["limits"]["maxFrameBytes"], json!(MAX_FRAME_BYTES));

        // unsupported abi -> -32006 with supported list
        let bad_abi = rpc_request(
            "r2",
            WIRE_INITIALIZE,
            json!({
                "abi_version": 2,
                "api_version": OPERATION_VERSION,
                "features": [],
                "client": {"name": "host", "version": "1.0"}
            }),
        );
        let resp = build_initialize_response(&bad_abi, false, &target, "conn-2");
        assert_eq!(error_code(&resp), -32006);
        assert_eq!(resp.error.unwrap().data.unwrap()["supported"], json!([1]));

        // unsupported api version -> -32006 with supported list
        let bad_api = rpc_request(
            "r3",
            WIRE_INITIALIZE,
            json!({
                "abi_version": 1,
                "api_version": "agentcpm-pd.operation/999",
                "features": [],
                "client": {"name": "host", "version": "1.0"}
            }),
        );
        let resp = build_initialize_response(&bad_api, false, &target, "conn-3");
        assert_eq!(error_code(&resp), -32006);
        assert_eq!(
            resp.error.unwrap().data.unwrap()["supported"],
            json!([OPERATION_VERSION])
        );

        // duplicate initialize -> -32600
        let resp = build_initialize_response(&req, true, &target, "conn-4");
        assert_eq!(error_code(&resp), -32600);

        // malformed params -> -32602
        let malformed = rpc_request("r5", WIRE_INITIALIZE, json!({"bogus": 1}));
        let resp = build_initialize_response(&malformed, false, &target, "conn-5");
        assert_eq!(error_code(&resp), -32602);

        // ClientDescriptor round-trips through the params parser.
        let descriptor: ClientDescriptor =
            serde_json::from_value(json!({"name": "host", "version": "1.0"})).unwrap();
        assert_eq!(descriptor.name, "host");
    }

    #[test]
    fn error_and_abandon_frames_carry_contract_data() {
        // contract error -> -32000 with full contract object
        let contract = ContractError::result_unknown(Some("key-9"), "connection lost");
        let resp = error_response_for("r1", &ClientError::Contract(contract));
        assert_eq!(error_code(&resp), -32000);
        let data = resp.error.unwrap().data.unwrap();
        assert_eq!(data["code"], "RESULT_UNKNOWN");
        assert_eq!(data["effect"], "unknown");
        assert_eq!(data["details"]["idempotencyKey"], "key-9");

        // protocol error passes the code through (e.g. -32004)
        let resp = error_response_for(
            "r2",
            &ClientError::ProtocolError {
                code: -32004,
                message: "capability unavailable".to_owned(),
            },
        );
        assert_eq!(error_code(&resp), -32004);

        // abandon: read -> not_applied; write -> unknown + key
        let read = abandon_reply("r3", false, None);
        assert_eq!(read.error.as_ref().unwrap().code, -32800);
        let read_data = read.error.unwrap().data.unwrap();
        assert_eq!(read_data["code"], "CONNECTION_FAILED");
        assert_eq!(read_data["effect"], "not_applied");

        let write = abandon_reply("r4", true, Some("key-4"));
        let write_data = write.error.unwrap().data.unwrap();
        assert_eq!(write_data["code"], "RESULT_UNKNOWN");
        assert_eq!(write_data["effect"], "unknown");
        assert_eq!(write_data["details"]["idempotencyKey"], "key-4");
    }

    #[test]
    fn conn_state_and_fatal_frames_serialize_camel_case() {
        let frame = conn_state_frame(
            ConnStateKind::Failed,
            Some("instance_changed"),
            Some("local-2"),
        );
        let value: Value = serde_json::from_str(frame.trim()).unwrap();
        assert_eq!(value["method"], "conn.state");
        assert_eq!(value["params"]["state"], "failed");
        assert_eq!(value["params"]["reason"], "instance_changed");
        assert_eq!(value["params"]["instanceId"], "local-2");

        let fatal = conn_fatal_frame(&ContractError::capability_unavailable("x", "y"));
        let value: Value = serde_json::from_str(fatal.trim()).unwrap();
        assert_eq!(value["method"], "conn.fatal");
        assert_eq!(value["params"]["code"], "CAPABILITY_UNAVAILABLE");
    }

    #[test]
    fn sub_end_frames_serialize_terminals_and_cursors() {
        let frame = sub_end_frame("sub-1", SubTerminal::SlowConsumer, Some("cur-9"), None);
        let value: Value = serde_json::from_str(frame.trim()).unwrap();
        assert_eq!(value["method"], "sub.end");
        assert_eq!(value["params"]["subscriptionId"], "sub-1");
        assert_eq!(value["params"]["terminal"], "slow_consumer");
        assert_eq!(value["params"]["resumeCursor"], "cur-9");

        let frame = sub_end_frame("sub-1", SubTerminal::Shutdown, None, None);
        let value: Value = serde_json::from_str(frame.trim()).unwrap();
        assert_eq!(value["params"]["terminal"], "shutdown");
        assert!(value["params"]["resumeCursor"].is_null());
    }

    #[test]
    fn backoff_stays_within_the_documented_window() {
        for attempt in 1..=12 {
            let d = backoff_with_jitter(attempt);
            let base = RECONNECT_BACKOFF_MS[(attempt - 1).min(RECONNECT_BACKOFF_MS.len() - 1)];
            assert!(d.as_millis() as u64 >= base);
            assert!((d.as_millis() as u64) < base + 250);
        }
    }
}
