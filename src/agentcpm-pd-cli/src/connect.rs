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
use std::pin::Pin;
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
/// Bounded wait when the engine routes a frame to the writer queues (P2-6).
const ENQUEUE_TIMEOUT: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// Entry
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Upstream session seam (production /v1/ws + engine test mocks)
// ---------------------------------------------------------------------------

type RawCallFuture = Pin<Box<dyn Future<Output = Result<Value, ClientError>> + Send>>;
type OpenSubFuture = Pin<Box<dyn Future<Output = Result<Box<dyn SubSource>, ClientError>> + Send>>;
type CloseSubFuture = Pin<Box<dyn Future<Output = Result<(), ClientError>> + Send>>;
type SubRecvFuture<'a> = Pin<Box<dyn Future<Output = Option<SubStreamItem>> + Send + 'a>>;
type SubCreditFuture<'a> = Pin<Box<dyn Future<Output = Result<(), ClientError>> + Send + 'a>>;
type SubCloseFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// The upstream `/v1/ws` session as seen by the engine. Narrow seam so the
/// engine can be driven end-to-end by tests against an in-memory mock.
pub trait UpstreamSession: Send + Sync {
    fn is_closed(&self) -> bool;
    fn bound_instance_id(&self) -> String;
    fn target_info(&self) -> TargetHandshakeInfo;
    fn send_raw(
        &self,
        request_id: String,
        method: String,
        params: Value,
        timeout: Duration,
        is_write: bool,
    ) -> RawCallFuture;
    fn open_subscription(&self, method: String, input: Value, credit: u32) -> OpenSubFuture;
    fn close_subscription(&self, subscription_id: String) -> CloseSubFuture;
    fn shutdown(&self) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

/// One live upstream subscription as seen by the engine's forwarder.
pub trait SubSource: Send {
    fn subscription_id(&self) -> String;
    fn recv_item(&mut self) -> SubRecvFuture<'_>;
    /// Non-blocking receive used to flush queued events on host close.
    fn try_recv_item(&mut self) -> Option<SubStreamItem>;
    fn grant_credit(&mut self, add: u32) -> SubCreditFuture<'_>;
    fn close_upstream(&mut self) -> SubCloseFuture<'_>;
}

/// Production [`UpstreamSession`] over `agentcpm-pd-client`'s `PdWsClient`.
struct PdUpstreamSession {
    ws: PdWsClient,
}

impl UpstreamSession for PdUpstreamSession {
    fn is_closed(&self) -> bool {
        self.ws.is_closed()
    }

    fn bound_instance_id(&self) -> String {
        self.ws.bound_instance_id().to_owned()
    }

    fn target_info(&self) -> TargetHandshakeInfo {
        self.ws.target_info().clone()
    }

    fn send_raw(
        &self,
        request_id: String,
        method: String,
        params: Value,
        timeout: Duration,
        is_write: bool,
    ) -> RawCallFuture {
        let ws = self.ws.clone();
        Box::pin(async move {
            ws.send_raw_rpc(request_id, &method, params, timeout, is_write)
                .await
        })
    }

    fn open_subscription(&self, method: String, input: Value, credit: u32) -> OpenSubFuture {
        let ws = self.ws.clone();
        Box::pin(async move {
            let sub = ws
                .open_subscription(&method, input, Some(credit), false)
                .await?;
            Ok(Box::new(PdSubSource { sub: Some(sub) }) as Box<dyn SubSource>)
        })
    }

    fn close_subscription(&self, subscription_id: String) -> CloseSubFuture {
        let ws = self.ws.clone();
        Box::pin(async move { ws.send_sub_close(&subscription_id).await })
    }

    fn shutdown(&self) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let ws = self.ws.clone();
        Box::pin(async move {
            ws.close().await;
        })
    }
}

/// Production [`SubSource`] over `PdWsSubscription`.
struct PdSubSource {
    sub: Option<PdWsSubscription>,
}

impl SubSource for PdSubSource {
    fn subscription_id(&self) -> String {
        self.sub
            .as_ref()
            .map(|sub| sub.subscription_id.clone())
            .unwrap_or_default()
    }

    fn recv_item(&mut self) -> SubRecvFuture<'_> {
        Box::pin(async move {
            match self.sub.as_mut() {
                Some(sub) => sub.recv().await,
                None => None,
            }
        })
    }

    fn try_recv_item(&mut self) -> Option<SubStreamItem> {
        self.sub.as_mut().and_then(|sub| sub.try_recv())
    }

    fn grant_credit(&mut self, add: u32) -> SubCreditFuture<'_> {
        Box::pin(async move {
            match self.sub.as_mut() {
                Some(sub) => sub.grant_credit(add).await,
                None => Ok(()),
            }
        })
    }

    fn close_upstream(&mut self) -> SubCloseFuture<'_> {
        Box::pin(async move {
            if let Some(sub) = self.sub.take() {
                let _ = sub.close().await;
            }
        })
    }
}

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
    match connect_upstream_with_retries(&client_opts, STARTUP_CONNECT_ATTEMPTS).await {
        Ok(client) => {
            let session: Arc<dyn UpstreamSession> = Arc::new(PdUpstreamSession {
                ws: client.ws().clone(),
            });
            let bound_instance_id = client.ws().bound_instance_id().to_owned();
            run_engine(
                tokio::io::stdin(),
                tokio::io::stdout(),
                session,
                bound_instance_id,
                client_opts,
                log_format,
            )
            .await
        }
        Err(err) => {
            let code = match &err {
                ClientError::Usage(_) | ClientError::Config(_) | ClientError::NotInitialized(_) => {
                    EXIT_USAGE
                }
                _ => EXIT_FAILED,
            };
            let contract = err.into_contract_error();
            match log_format {
                LogFormat::Json => eprintln!(
                    "{}",
                    json!({"level": "error", "message": format!("gateway connect failed: {err}")})
                ),
                LogFormat::Text => eprintln!("{BIN_NAME}: gateway connect failed: {err}"),
            }
            // Emit conn.fatal with a bounded write; nothing else was queued.
            let mut out = tokio::io::stdout();
            let _ = tokio::time::timeout(
                Duration::from_secs(1),
                write_line(&mut out, &conn_fatal_frame(&contract)),
            )
            .await;
            code
        }
    }
}

/// Engine entry over generic IO so tests can drive it with duplex streams.
#[allow(clippy::too_many_arguments)]
async fn run_engine<R, W>(
    reader: R,
    writer: W,
    session: Arc<dyn UpstreamSession>,
    bound_instance_id: String,
    client_opts: PdClientOptions,
    log_format: LogFormat,
) -> i32
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
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
        writer,
        ctrl_rx,
        norm_rx,
        evt_rx,
        writer_kill_rx,
        initialized.clone(),
        shutting_down.clone(),
        writer_died_tx,
    ));
    let reader_handle = tokio::spawn(reader_task(reader, reader_tx));

    let mut engine = Engine {
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
        session,
        bound_instance_id,
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
        next_token: 0,
        dropped_terminal_frames: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        dropped_credit_grants: 0,
        log_format,
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

    // Final flush and teardown (bounded, so a blocked host cannot hang exit).
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

async fn reader_task<R: tokio::io::AsyncRead + Unpin>(reader: R, tx: mpsc::Sender<ReaderMsg>) {
    let mut reader = tokio::io::BufReader::new(reader);
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
async fn write_line<W: tokio::io::AsyncWrite + Unpin>(out: &mut W, line: &str) -> bool {
    out.write_all(line.as_bytes()).await.is_ok() && out.flush().await.is_ok()
}

#[allow(clippy::too_many_arguments)]
async fn writer_task<W: tokio::io::AsyncWrite + Unpin>(
    writer: W,
    mut ctrl: mpsc::Receiver<String>,
    mut norm: mpsc::Receiver<String>,
    mut evt: mpsc::Receiver<String>,
    mut kill: watch::Receiver<bool>,
    initialized: Arc<AtomicBool>,
    shutting_down: Arc<AtomicBool>,
    writer_died: oneshot::Sender<()>,
) {
    let mut out = writer;
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

async fn drain_writer_queues<W: tokio::io::AsyncWrite + Unpin>(
    ctrl: &mut mpsc::Receiver<String>,
    norm: &mut mpsc::Receiver<String>,
    evt: &mut mpsc::Receiver<String>,
    out: &mut W,
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

    // P2-3: an in-flight id cannot be reused while the sub.open is pending.
    if gate.pending_ids.contains(id) {
        return Err(JsonRpcResponse::err(
            Some(id.to_owned()),
            agentcpm_pd_protocol::errors::JSONRPC_INVALID_REQUEST,
            format!("request id `{id}` is already in flight"),
            None,
        ));
    }

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
    /// Monotonic token issued when this request was registered. Ids may be
    /// reused once a request completes or is abandoned (`007 §6.3`: unique
    /// only among in-flight requests), so a stale completion from the OLD
    /// request must never claim the NEW one (review P1-2).
    token: u64,
    is_control: bool,
    is_write: bool,
    idempotency_key: Option<String>,
    handle: JoinHandle<()>,
}

/// Engine bookkeeping events from spawned request/subscription tasks.
///
/// `Request` carries the computed response: only the engine loop routes it to
/// the writer, so an abandoned request can never produce both `-32800` and the
/// real result (single-response invariant, review P2-4).
enum DoneMsg {
    Request {
        id: String,
        /// Token of the pending entry this completion was issued for.
        token: u64,
        is_control: bool,
        response: Option<JsonRpcResponse>,
    },
    SubscriptionEnded {
        id: String,
    },
}

struct SubAnnouncement {
    /// The host request id this subscription answers.
    request_id: String,
    /// Token of the sub.open's pending entry at spawn time.
    token: u64,
    subscription_id: String,
    /// Engine `sub_generation` captured when the open started; a mismatch
    /// means the upstream connection was replaced while opening.
    generation: u64,
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
    session: Arc<dyn UpstreamSession>,
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
    /// Monotonic pending token source (see `PendingReq::token`).
    next_token: u64,
    /// Terminal/control frames dropped after a full writer queue (review P2-12).
    dropped_terminal_frames: Arc<std::sync::atomic::AtomicU32>,
    /// Credit grants dropped on a saturated per-subscription channel (P2-6).
    dropped_credit_grants: u32,
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

    /// Routes a response to the matching lane. Bounded wait: a host that
    /// never reads stdout costs the frame, never a stalled engine loop (P2-6).
    async fn enqueue_response(&self, is_control: bool, resp: &JsonRpcResponse) {
        if let Ok(line) = encode_ndjson_line(resp) {
            self.enqueue_line(is_control, line).await;
        }
    }

    async fn enqueue_line(&self, is_control: bool, line: String) {
        let queue = if is_control {
            &self.out.ctrl
        } else {
            &self.out.norm
        };
        match tokio::time::timeout(ENQUEUE_TIMEOUT, queue.send(line)).await {
            Ok(Ok(())) => {}
            _ => self.note_dropped_terminal(),
        }
    }

    /// Fire-and-forget control frame with an immediate drop accounting.
    fn try_enqueue_control(&self, line: String) {
        if self.out.ctrl.try_send(line).is_err() {
            self.note_dropped_terminal();
        }
    }

    fn note_dropped_terminal(&self) {
        let total = self.dropped_terminal_frames.fetch_add(1, Ordering::SeqCst) + 1;
        self.diag(format!(
            "dropped a control/terminal frame after a full writer queue (total {total})"
        ));
    }

    /// P1-1: accept a freshly opened subscription. The sub.open response is
    /// only enqueued AFTER the subscription is registered (or explicitly
    /// terminated), so the host can never reference an id the engine does not
    /// know yet — no credit can be lost to the registration window.
    async fn accept_announcement(&mut self, ann: SubAnnouncement) {
        // P1-2: the sub.open's pending entry is claimed only when the token
        // matches; an abandoned-then-reused id must not let a stale open
        // answer the newer request.
        let claimed = match self.pending.get(&ann.request_id) {
            Some(entry) if entry.token == ann.token => {
                self.pending.remove(&ann.request_id).is_some()
            }
            _ => false,
        };
        if claimed {
            self.pending_control = self.pending_control.saturating_sub(1);
        } else {
            // Abandoned (its -32800 was already sent) or the id was reused by
            // a newer request: terminate silently (P2-4 single-response).
            ann.forwarder.abort();
            let sid = ann.subscription_id.clone();
            let session = self.session.clone();
            tokio::spawn(async move {
                let _ = session.close_subscription(sid).await;
            });
            return;
        }

        // P2-5: the upstream connection was replaced while the open was in
        // flight — answer, then terminate immediately without registering.
        if ann.generation != self.sub_generation {
            ann.forwarder.abort();
            let sid = ann.subscription_id.clone();
            let session = self.session.clone();
            tokio::spawn(async move {
                let _ = session.close_subscription(sid).await;
            });
            let resp = JsonRpcResponse::ok(
                &ann.request_id,
                serde_json::to_value(SubOpenResult {
                    subscription_id: ann.subscription_id.clone(),
                })
                .unwrap_or(Value::Null),
            );
            self.enqueue_response(true, &resp).await;
            self.try_enqueue_control(sub_end_frame(
                &ann.subscription_id,
                SubTerminal::Disconnected,
                None,
                None,
            ));
            return;
        }

        // P2-5: forwarder already exited before the announcement was
        // processed — do not register a dead entry.
        if ann.forwarder.is_finished() {
            ann.forwarder.abort();
            let resp = JsonRpcResponse::ok(
                &ann.request_id,
                serde_json::to_value(SubOpenResult {
                    subscription_id: ann.subscription_id.clone(),
                })
                .unwrap_or(Value::Null),
            );
            self.enqueue_response(true, &resp).await;
            return;
        }

        // Register FIRST, respond AFTER (review P1-1).
        let resp = JsonRpcResponse::ok(
            &ann.request_id,
            serde_json::to_value(SubOpenResult {
                subscription_id: ann.subscription_id.clone(),
            })
            .unwrap_or(Value::Null),
        );
        self.subscriptions.insert(ann.subscription_id.clone(), ann);
        self.enqueue_response(true, &resp).await;
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
                        self.enqueue_response(true, &resp).await;
                    }
                    Some(ReaderMsg::Eof) | None => {
                        reader_alive = false;
                        self.begin_shutdown(&mut shutdown, &reader_handle, EXIT_OK, "stdin EOF");
                    }
                },
                done = done_rx.recv() => match done {
                    Some(DoneMsg::Request { id, token, is_control, response }) => {
                        // P1-2: claim only if BOTH the id is pending AND the
                        // token matches the entry that issued this completion;
                        // a reused id carrying a different token must never be
                        // answered with a stale response.
                        let claimed = match self.pending.get(&id) {
                            Some(entry) if entry.token == token => {
                                self.pending.remove(&id).is_some()
                            }
                            _ => false,
                        };
                        if claimed {
                            if is_control {
                                self.pending_control =
                                    self.pending_control.saturating_sub(1);
                            } else {
                                self.pending_normal =
                                    self.pending_normal.saturating_sub(1);
                            }
                            if let Some(resp) = response {
                                self.enqueue_response(is_control, &resp).await;
                            }
                        }
                        // Not claimed: the request was abandoned (its -32800
                        // was already sent), already answered, or the id was
                        // reused by a newer request — the late response is
                        // dropped, never delivered on the wrong request (P2-4).
                    }
                    Some(DoneMsg::SubscriptionEnded { id }) => {
                        self.subscriptions.remove(&id);
                    }
                    None => {}
                },
                ann = sub_announce_rx.recv() => {
                    if let Some(ann) = ann {
                        self.accept_announcement(ann).await;
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
                self.try_enqueue_control(line);
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
            self.try_enqueue_control(frame);
        }
        self.shutting_down.store(true, Ordering::SeqCst);
    }

    async fn watchdog_tick(&mut self) {
        if !self.reconnecting && self.session.is_closed() {
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
            self.try_enqueue_control(frame);
        }
        // In-flight requests were already failed by the ws client internals
        // (reads CONNECTION_FAILED / not_applied, writes RESULT_UNKNOWN / unknown).

        self.enqueue_line(
            true,
            conn_state_frame(
                ConnStateKind::Reconnecting,
                None,
                Some(&self.bound_instance_id),
            ),
        )
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
                let old = std::mem::replace(
                    &mut self.session,
                    Arc::new(PdUpstreamSession {
                        ws: client.ws().clone(),
                    }),
                );
                old.shutdown().await;
                self.reconnecting = false;
                self.reconnect_attempt = 0;
                self.next_reconnect_at = None;
                self.enqueue_line(
                    true,
                    conn_state_frame(
                        ConnStateKind::Connected,
                        None,
                        Some(&self.bound_instance_id),
                    ),
                )
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
                self.enqueue_line(
                    true,
                    conn_state_frame(
                        ConnStateKind::Reconnecting,
                        None,
                        Some(&self.bound_instance_id),
                    ),
                )
                .await;
            }
        }
    }

    async fn instance_changed_fatal(&mut self, actual: &str, shutdown: &mut Option<ShutdownState>) {
        self.enqueue_line(
            true,
            conn_state_frame(
                ConnStateKind::Failed,
                Some("instance_changed"),
                Some(actual),
            ),
        )
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
                self.enqueue_response(true, &error).await;
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
            self.enqueue_response(true, &resp).await;
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
            self.enqueue_response(true, &resp).await;
            return;
        }

        match req.method.as_str() {
            WIRE_INITIALIZE => {
                let connection_id = IdGenerator::new(NodeId::local()).next_id();
                let resp = build_initialize_response(
                    &req,
                    initialized,
                    &self.session.target_info(),
                    &connection_id,
                );
                if resp.error.is_none() {
                    self.initialized.store(true, Ordering::SeqCst);
                }
                self.enqueue_response(true, &resp).await;
            }
            WIRE_SUB_OPEN => {
                let result = prepare_sub_open(&req, &self.gate());
                match result {
                    Ok(prepared) => {
                        self.forward_sub_open(req, prepared).await;
                    }
                    Err(resp) => {
                        self.enqueue_response(true, &resp).await;
                    }
                }
            }
            WIRE_SUB_CREDIT => {
                match serde_json::from_value::<SubCreditParams>(req.params.clone()) {
                    Ok(params) => self.handle_sub_credit(Some(req.id.clone()), params).await,
                    Err(e) => {
                        let resp = invalid_params(&req.id, format!("sub.credit params: {e}"), None);
                        self.enqueue_response(true, &resp).await;
                    }
                }
            }
            WIRE_SUB_CLOSE => match serde_json::from_value::<SubCloseParams>(req.params.clone()) {
                Ok(params) => self.handle_sub_close(Some(req.id.clone()), params).await,
                Err(e) => {
                    let resp = invalid_params(&req.id, format!("sub.close params: {e}"), None);
                    self.enqueue_response(true, &resp).await;
                }
            },
            WIRE_CONN_ABANDON => {
                match serde_json::from_value::<ConnAbandonParams>(req.params.clone()) {
                    Ok(params) => self.handle_abandon(req, params).await,
                    Err(e) => {
                        let resp =
                            invalid_params(&req.id, format!("conn.abandon params: {e}"), None);
                        self.enqueue_response(true, &resp).await;
                    }
                }
            }
            WIRE_CONN_HEARTBEAT => {
                // Host heartbeat: acknowledged implicitly.
                let resp = JsonRpcResponse::ok(req.id, json!({}));
                self.enqueue_response(true, &resp).await;
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
                        self.enqueue_response(true, &resp).await;
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
        self.enqueue_response(true, &resp).await;
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
        let session = self.session.clone();
        let done = self.done_tx.clone();
        let id = req.id.clone();
        let idempotency_key = params.idempotency_key.clone();
        let rpc_params = serde_json::to_value(&params).unwrap_or(Value::Null);
        let forward_method = method;

        self.next_token += 1;
        let token = self.next_token;

        let handle = tokio::spawn(async move {
            let result = session
                .send_raw(id.clone(), forward_method, rpc_params, timeout, is_write)
                .await;
            // Only the engine routes responses to the writer (P2-4).
            let response = match result {
                Ok(value) => JsonRpcResponse::ok(id.clone(), value),
                Err(err) => error_response_for(&id, &err),
            };
            let _ = done
                .send(DoneMsg::Request {
                    id,
                    token,
                    is_control,
                    response: Some(response),
                })
                .await;
        });

        self.pending.insert(
            req.id.clone(),
            PendingReq {
                token,
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
        let session = self.session.clone();
        let evt = self.out.evt.clone();
        let ctrl = self.out.ctrl.clone();
        // T9 (WP5 e2e): the engine's own `sub_kill_rx` is never polled, so it
        // (and every clone made from it) keeps the watch version from engine
        // start. After a reconnect bumps the generation, a clone handed to a
        // NEW forwarder resolved `changed()` immediately — every
        // post-reconnect subscription died silently before relaying a single
        // frame. Mark the current value as seen so the forwarder only reacts
        // to kill signals raised after it starts.
        let mut sub_kill = self.sub_kill_rx.clone();
        let _ = sub_kill.borrow_and_update();
        let announce = self.sub_announce_tx.clone();
        let done = self.done_tx.clone();
        let dropped = self.dropped_terminal_frames.clone();
        let generation = self.sub_generation;
        let method = prepared.method;
        let input = prepared.input;
        let credit = prepared.credit;
        let request_id = req.id.clone();
        let log_format = self.log_format;

        self.next_token += 1;
        let token = self.next_token;

        let handle = tokio::spawn(async move {
            match session.open_subscription(method, input, credit).await {
                Ok(source) => {
                    let subscription_id = source.subscription_id();
                    let (credit_tx, credit_rx) = mpsc::channel::<u32>(64);
                    let last_cursor: Arc<std::sync::Mutex<Option<String>>> =
                        Arc::new(std::sync::Mutex::new(None));
                    let closed_by_host = Arc::new(AtomicBool::new(false));
                    let forwarder = tokio::spawn(sub_forwarder(
                        subscription_id.clone(),
                        source,
                        evt,
                        ctrl.clone(),
                        credit_rx,
                        sub_kill,
                        last_cursor.clone(),
                        closed_by_host.clone(),
                        done.clone(),
                        dropped,
                        log_format,
                    ));
                    // The ENGINE answers the sub.open after registering (P1-1).
                    let _ = announce
                        .send(SubAnnouncement {
                            request_id,
                            token,
                            subscription_id,
                            generation,
                            last_cursor,
                            closed_by_host,
                            credit_tx,
                            forwarder,
                        })
                        .await;
                }
                Err(err) => {
                    // Route the failure through the same single-response path.
                    let resp = error_response_for(&request_id, &err);
                    let _ = done
                        .send(DoneMsg::Request {
                            id: request_id,
                            token,
                            is_control: true,
                            response: Some(resp),
                        })
                        .await;
                }
            }
        });
        // The sub.open counts as an in-flight control request so the EOF drain
        // waits for its response before exiting.
        // P2-3: a duplicate that slipped past prepare_sub_open aborts the old
        // opener instead of leaking its pending slot.
        if let Some(old) = self.pending.insert(
            req.id,
            PendingReq {
                token,
                is_control: true,
                is_write: false,
                idempotency_key: None,
                handle,
            },
        ) {
            self.pending_control = self.pending_control.saturating_sub(1);
            old.handle.abort();
        } else {
            self.pending_control += 1;
        }
    }

    async fn handle_sub_credit(&mut self, reply_to: Option<String>, params: SubCreditParams) {
        let Some(sub) = self.subscriptions.get(&params.subscription_id) else {
            if let Some(id) = reply_to {
                let resp = invalid_params(
                    &id,
                    format!("unknown subscriptionId `{}`", params.subscription_id),
                    None,
                );
                self.enqueue_response(true, &resp).await;
            }
            return;
        };
        if params.add == 0 {
            if let Some(id) = reply_to {
                let resp = invalid_params(&id, "credit `add` must be greater than zero", None);
                self.enqueue_response(true, &resp).await;
            }
            return;
        }
        // P2-6: never block the engine loop on the forwarder's credit channel.
        match sub.credit_tx.try_send(params.add) {
            Ok(()) => {
                if let Some(id) = reply_to {
                    let resp = JsonRpcResponse::ok(id, json!({}));
                    self.enqueue_response(true, &resp).await;
                }
            }
            Err(_) => {
                self.dropped_credit_grants = self.dropped_credit_grants.saturating_add(1);
                self.diag(format!(
                    "dropped a sub.credit grant (subscription consumer saturated; total {})",
                    self.dropped_credit_grants
                ));
                if let Some(id) = reply_to {
                    let resp = error_response_for(
                        &id,
                        &ClientError::Contract(ContractError::new(
                            CODE_CONNECTION_FAILED,
                            "credit channel for this subscription is saturated; retry after draining events",
                            RetrySpec::backoff_read(Some(200)),
                            EffectStatus::NotApplied,
                        )),
                    );
                    self.enqueue_response(true, &resp).await;
                }
            }
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
                self.enqueue_response(true, &resp).await;
            }
            return;
        };
        // P1-2: the entry (and its credit channel) is dropped immediately,
        // which makes the forwarder flush its queued events and then
        // synthesize sub.end(closed) itself — no detached spinning task, no
        // lost events, no double terminal frame.
        sub.closed_by_host.store(true, Ordering::SeqCst);
        drop(sub);
        let session = self.session.clone();
        let subscription_id = params.subscription_id.clone();
        tokio::spawn(async move {
            let _ = session.close_subscription(subscription_id).await;
        });
        if let Some(id) = reply_to {
            let resp = JsonRpcResponse::ok(id, json!({}));
            self.enqueue_response(true, &resp).await;
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
                self.enqueue_response(true, &original).await;
                if is_control {
                    self.pending_control = self.pending_control.saturating_sub(1);
                } else {
                    self.pending_normal = self.pending_normal.saturating_sub(1);
                }
                let resp = JsonRpcResponse::ok(req.id, json!({}));
                self.enqueue_response(true, &resp).await;
            }
            None => {
                let resp = JsonRpcResponse::err(
                    Some(req.id),
                    agentcpm_pd_protocol::errors::JSONRPC_INVALID_REQUEST,
                    format!("unknown request id `{}` to abandon", params.id),
                    None,
                );
                self.enqueue_response(true, &resp).await;
            }
        }
    }
}

/// Forwards upstream subscription events to the host with per-subscription
/// cursor tracking, slow-consumer termination, and oversized-event handling.
///
/// Never blocks on the engine's queues: terminal frames degrade from the event
/// lane to the control lane, and drops are counted (review P2-6/P2-12).
#[allow(clippy::too_many_arguments)]
async fn sub_forwarder(
    subscription_id: String,
    mut source: Box<dyn SubSource>,
    evt: mpsc::Sender<String>,
    ctrl: mpsc::Sender<String>,
    mut credit_rx: mpsc::Receiver<u32>,
    mut kill: watch::Receiver<u64>,
    last_cursor: Arc<std::sync::Mutex<Option<String>>>,
    closed_by_host: Arc<AtomicBool>,
    done: mpsc::Sender<DoneMsg>,
    dropped: Arc<std::sync::atomic::AtomicU32>,
    _log_format: LogFormat,
) {
    let note_drop = |dropped: &Arc<std::sync::atomic::AtomicU32>| {
        dropped.fetch_add(1, Ordering::SeqCst);
    };
    loop {
        tokio::select! {
            biased;
            _ = kill.changed() => break,
            // P1-2: a closed credit channel means the engine dropped this
            // subscription (host close or shutdown). Flush everything already
            // queued, then terminate closed — never spin on the closed channel.
            grant = credit_rx.recv() => match grant {
                Some(add) => {
                    let _ = source.grant_credit(add).await;
                }
                None => {
                    let mut relayed_end = false;
                    while let Some(item) = source.try_recv_item() {
                        match item {
                            SubStreamItem::Event(ev) => {
                                let cursor = ev.event.cursor.clone();
                                let notif = JsonRpcNotification {
                                    jsonrpc: JSONRPC_VERSION.to_owned(),
                                    method: WIRE_SUB_EVENT.to_owned(),
                                    params: serde_json::to_value(SubEventNotification {
                                        subscription_id: subscription_id.clone(),
                                        event: ev.event,
                                    })
                                    .unwrap_or(Value::Null),
                                };
                                let Ok(json) = serde_json::to_string(&notif) else {
                                    continue;
                                };
                                if json.len() >= MAX_FRAME_BYTES {
                                    let last = last_cursor.lock().unwrap().clone();
                                    let end = sub_end_frame(
                                        &subscription_id,
                                        SubTerminal::Error,
                                        last.as_deref(),
                                        Some(&ContractError::new(
                                            "INPUT_TOO_LARGE",
                                            "event exceeded the 1 MiB frame cap",
                                            RetrySpec::never(),
                                            EffectStatus::NotApplied,
                                        )),
                                    );
                                    if ctrl.try_send(end).is_err() {
                                        note_drop(&dropped);
                                    }
                                    relayed_end = true;
                                    break;
                                }
                                let mut line = json;
                                line.push('\n');
                                if evt.try_send(line).is_err() {
                                    let last = last_cursor.lock().unwrap().clone();
                                    let end = sub_end_frame(
                                        &subscription_id,
                                        SubTerminal::SlowConsumer,
                                        last.as_deref(),
                                        None,
                                    );
                                    if ctrl.try_send(end).is_err() {
                                        note_drop(&dropped);
                                    }
                                    relayed_end = true;
                                    break;
                                }
                                *last_cursor.lock().unwrap() = Some(cursor);
                            }
                            SubStreamItem::End(end) => {
                                let frame = sub_end_frame(
                                    &subscription_id,
                                    end.terminal,
                                    end.resume_cursor.as_deref(),
                                    end.error.as_ref(),
                                );
                                if ctrl.try_send(frame).is_err() {
                                    note_drop(&dropped);
                                }
                                relayed_end = true;
                                break;
                            }
                        }
                    }
                    if !relayed_end {
                        let cursor = last_cursor.lock().unwrap().clone();
                        let frame = sub_end_frame(
                            &subscription_id,
                            SubTerminal::Closed,
                            cursor.as_deref(),
                            None,
                        );
                        // Same lane as the flushed events so the terminal
                        // frame cannot overtake them; degrade to ctrl when
                        // the event lane is full.
                        let fallback = frame.clone();
                        if evt.try_send(frame).is_err() && ctrl.try_send(fallback).is_err() {
                            note_drop(&dropped);
                        }
                    }
                    break;
                }
            },
            item = source.recv_item() => match item {
                Some(SubStreamItem::Event(ev)) => {
                    let cursor = ev.event.cursor.clone();
                    let notif = JsonRpcNotification {
                        jsonrpc: JSONRPC_VERSION.to_owned(),
                        method: WIRE_SUB_EVENT.to_owned(),
                        params: serde_json::to_value(SubEventNotification {
                            subscription_id: subscription_id.clone(),
                            event: ev.event,
                        })
                        .unwrap_or(Value::Null),
                    };
                    let Ok(json) = serde_json::to_string(&notif) else {
                        continue;
                    };
                    // P2-7: judge the frame content (without the trailing
                    // newline); a content length >= MAX would produce a line
                    // the peer's inbound cap rejects.
                    if json.len() >= MAX_FRAME_BYTES {
                        let cursor = last_cursor.lock().unwrap().clone();
                        let end = sub_end_frame(
                            &subscription_id,
                            SubTerminal::Error,
                            cursor.as_deref(),
                            Some(&ContractError::new(
                                "INPUT_TOO_LARGE",
                                "event exceeded the 1 MiB frame cap",
                                RetrySpec::never(),
                                EffectStatus::NotApplied,
                            )),
                        );
                        if ctrl.try_send(end).is_err() {
                            note_drop(&dropped);
                        }
                        break;
                    }
                    let mut line = json;
                    line.push('\n');
                    if evt.try_send(line).is_err() {
                        let cursor = last_cursor.lock().unwrap().clone();
                        let end = sub_end_frame(
                            &subscription_id,
                            SubTerminal::SlowConsumer,
                            cursor.as_deref(),
                            None,
                        );
                        if ctrl.try_send(end).is_err() {
                            note_drop(&dropped);
                        }
                        break;
                    }
                    // P2-7: the cursor advances only after a successful enqueue.
                    *last_cursor.lock().unwrap() = Some(cursor);
                }
                Some(SubStreamItem::End(end)) => {
                    // Relay the upstream terminal through the event lane: it
                    // must not overtake this subscription's queued events;
                    // degrade to the control lane when full (P2-6).
                    let frame = sub_end_frame(
                        &subscription_id,
                        end.terminal,
                        end.resume_cursor.as_deref(),
                        end.error.as_ref(),
                    );
                    let fallback = frame.clone();
                    if evt.try_send(frame).is_err() && ctrl.try_send(fallback).is_err() {
                        note_drop(&dropped);
                    }
                    break;
                }
                None => {
                    // Upstream channel vanished without a terminal frame.
                    let terminal = if closed_by_host.load(Ordering::SeqCst) {
                        SubTerminal::Closed
                    } else {
                        SubTerminal::Disconnected
                    };
                    let cursor = last_cursor.lock().unwrap().clone();
                    let frame =
                        sub_end_frame(&subscription_id, terminal, cursor.as_deref(), None);
                    if ctrl.try_send(frame).is_err() {
                        note_drop(&dropped);
                    }
                    break;
                }
            },
        }
    }
    // P2-10: every exit path closes the upstream subscription so long-lived
    // processes do not accumulate live server-side streams.
    source.close_upstream().await;
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

// ---------------------------------------------------------------------------
// Engine timing tests (mock upstream + duplex stdio; review P1/P2 items)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod engine_tests {
    use super::*;
    use std::time::Duration as StdDuration;

    use agentcpm_pd_protocol::wire::SUBSCRIPTION_BUFFER_ITEMS;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    // ---- mock upstream ------------------------------------------------------

    struct MockInner {
        sub_counter: u32,
        sub_txs: Vec<mpsc::Sender<SubStreamItem>>,
        sub_ids: Vec<String>,
        grants: Vec<u32>,
        closed_upstream: Vec<String>,
        close_calls: Vec<String>,
        raw_calls: Vec<(String, String)>,
        held: HashMap<String, oneshot::Sender<Result<Value, ClientError>>>,
        hold_methods: Vec<String>,
        slow_open: bool,
        grant_delay_ms: u64,
    }

    #[derive(Clone)]
    struct MockSession {
        bound: String,
        closed: Arc<AtomicBool>,
        inner: Arc<std::sync::Mutex<MockInner>>,
        open_release: Arc<tokio::sync::Notify>,
    }

    impl MockSession {
        fn new() -> Self {
            Self {
                bound: "local-mock-1".to_owned(),
                closed: Arc::new(AtomicBool::new(false)),
                inner: Arc::new(std::sync::Mutex::new(MockInner {
                    sub_counter: 0,
                    sub_txs: Vec::new(),
                    sub_ids: Vec::new(),
                    grants: Vec::new(),
                    closed_upstream: Vec::new(),
                    close_calls: Vec::new(),
                    raw_calls: Vec::new(),
                    held: HashMap::new(),
                    hold_methods: Vec::new(),
                    slow_open: false,
                    grant_delay_ms: 0,
                })),
                open_release: Arc::new(tokio::sync::Notify::new()),
            }
        }

        fn hold(&self, method: &str) {
            self.inner
                .lock()
                .unwrap()
                .hold_methods
                .push(method.to_owned());
        }

        fn resolve(&self, id: &str, value: Value) {
            if let Some(tx) = self.inner.lock().unwrap().held.remove(id) {
                let _ = tx.send(Ok(value));
            }
        }

        fn inject_event(&self, cursor: &str) {
            let tx = self.inner.lock().unwrap().sub_txs[0].clone();
            let _ = tx.try_send(SubStreamItem::Event(SubEventNotification {
                subscription_id: self.inner.lock().unwrap().sub_ids[0].clone(),
                event: agentcpm_pd_protocol::contract::EventEnvelope {
                    version: "agentcpm-pd.event/1".to_owned(),
                    instance_id: "local-mock-1".to_owned(),
                    run_id: Some("run-1".to_owned()),
                    operation_id: None,
                    subscription_id: None,
                    cursor: cursor.to_owned(),
                    event_type: "run.progress".to_owned(),
                    data: serde_json::Map::new(),
                },
            }));
        }

        fn set_slow_open(&self) {
            self.inner.lock().unwrap().slow_open = true;
        }

        fn set_grant_delay(&self, ms: u64) {
            self.inner.lock().unwrap().grant_delay_ms = ms;
        }

        fn grants(&self) -> Vec<u32> {
            self.inner.lock().unwrap().grants.clone()
        }

        fn close_calls(&self) -> Vec<String> {
            self.inner.lock().unwrap().close_calls.clone()
        }
    }

    impl UpstreamSession for MockSession {
        fn is_closed(&self) -> bool {
            self.closed.load(Ordering::SeqCst)
        }

        fn bound_instance_id(&self) -> String {
            self.bound.clone()
        }

        fn target_info(&self) -> TargetHandshakeInfo {
            TargetHandshakeInfo {
                name: "local".to_owned(),
                instance_id: self.bound.clone(),
                node_id: "local".to_owned(),
                gateway_version: "mock".to_owned(),
                schema_digest: "deadbeef".to_owned(),
                transport: "ws".to_owned(),
            }
        }

        fn send_raw(
            &self,
            request_id: String,
            method: String,
            _params: Value,
            _timeout: Duration,
            _is_write: bool,
        ) -> RawCallFuture {
            let inner = self.inner.clone();
            Box::pin(async move {
                let held = {
                    let mut guard = inner.lock().unwrap();
                    guard.raw_calls.push((request_id.clone(), method.clone()));
                    if guard.hold_methods.contains(&method) {
                        let (tx, rx) = oneshot::channel();
                        guard.held.insert(request_id.clone(), tx);
                        Some(rx)
                    } else {
                        None
                    }
                };
                if let Some(rx) = held {
                    return match rx.await {
                        Ok(result) => result,
                        Err(_) => Err(ClientError::connection_failed("mock dropped")),
                    };
                }
                Ok(json!({"outcome": "succeeded", "result": {"mock": method}}))
            })
        }

        fn open_subscription(&self, _method: String, _input: Value, _credit: u32) -> OpenSubFuture {
            let inner = self.inner.clone();
            let release = self.open_release.clone();
            Box::pin(async move {
                if inner.lock().unwrap().slow_open {
                    release.notified().await;
                }
                let (tx, rx) = mpsc::channel(SUBSCRIPTION_BUFFER_ITEMS);
                let mut guard = inner.lock().unwrap();
                guard.sub_counter += 1;
                let id = format!("sub-mock-{}", guard.sub_counter);
                guard.sub_txs.push(tx);
                guard.sub_ids.push(id.clone());
                let delay = guard.grant_delay_ms;
                drop(guard);
                Ok(Box::new(MockSubSource {
                    id,
                    rx,
                    inner: inner.clone(),
                    delay,
                }) as Box<dyn SubSource>)
            })
        }

        fn close_subscription(&self, subscription_id: String) -> CloseSubFuture {
            let inner = self.inner.clone();
            Box::pin(async move {
                inner.lock().unwrap().close_calls.push(subscription_id);
                Ok(())
            })
        }

        fn shutdown(&self) -> Pin<Box<dyn Future<Output = ()> + Send>> {
            Box::pin(async {})
        }
    }

    struct MockSubSource {
        id: String,
        rx: mpsc::Receiver<SubStreamItem>,
        inner: Arc<std::sync::Mutex<MockInner>>,
        delay: u64,
    }

    impl SubSource for MockSubSource {
        fn subscription_id(&self) -> String {
            self.id.clone()
        }

        fn recv_item(&mut self) -> SubRecvFuture<'_> {
            Box::pin(async move { self.rx.recv().await })
        }

        fn try_recv_item(&mut self) -> Option<SubStreamItem> {
            self.rx.try_recv().ok()
        }

        fn grant_credit(&mut self, add: u32) -> SubCreditFuture<'_> {
            Box::pin(async move {
                if self.delay > 0 {
                    tokio::time::sleep(Duration::from_millis(self.delay)).await;
                }
                self.inner.lock().unwrap().grants.push(add);
                Ok(())
            })
        }

        fn close_upstream(&mut self) -> SubCloseFuture<'_> {
            Box::pin(async move {
                self.inner
                    .lock()
                    .unwrap()
                    .closed_upstream
                    .push(self.id.clone());
            })
        }
    }

    // ---- harness -------------------------------------------------------------

    struct Harness {
        stdin: tokio::io::DuplexStream,
        stdout: tokio::io::BufReader<tokio::io::DuplexStream>,
        task: JoinHandle<i32>,
    }

    impl Harness {
        async fn start() -> (Self, MockSession) {
            let session = MockSession::new();
            let (engine_in, mut host_out) = tokio::io::duplex(64 * 1024);
            let (mut host_in, engine_out) = tokio::io::duplex(64 * 1024);
            let opts = PdClientOptions {
                home_override: None,
                target: None,
                no_start: true,
                spawner: None,
            };
            let sess: Arc<dyn UpstreamSession> = Arc::new(session.clone());
            let task = tokio::spawn(run_engine(
                engine_in,
                engine_out,
                sess,
                "local-mock-1".to_owned(),
                opts,
                LogFormat::Text,
            ));
            let _ = host_out.flush().await;
            let _ = &mut host_in;
            (
                Harness {
                    stdin: host_out,
                    stdout: tokio::io::BufReader::new(host_in),
                    task,
                },
                session,
            )
        }

        async fn send(&mut self, line: &str) {
            self.stdin
                .write_all(format!("{line}\n").as_bytes())
                .await
                .unwrap();
            self.stdin.flush().await.unwrap();
        }

        async fn read_line(&mut self) -> String {
            let mut buf = String::new();
            let n =
                tokio::time::timeout(StdDuration::from_secs(5), self.stdout.read_line(&mut buf))
                    .await
                    .expect("timeout waiting for engine output")
                    .expect("engine stdout closed");
            assert!(n > 0, "engine stdout closed unexpectedly");
            buf.trim_end().to_owned()
        }

        /// Reads lines until one matches `id`, collecting skipped frames.
        async fn read_response_collect(&mut self, id: &str, skipped: &mut Vec<Value>) -> Value {
            loop {
                let line = self.read_line().await;
                let value: Value = serde_json::from_str(&line).expect("valid frame");
                if value.get("id").and_then(Value::as_str) == Some(id) {
                    return value;
                }
                skipped.push(value);
            }
        }

        /// Reads lines until one matches `id`, skipping notifications.
        async fn read_response(&mut self, id: &str) -> Value {
            let mut skipped = Vec::new();
            self.read_response_collect(id, &mut skipped).await
        }

        async fn initialize(&mut self) {
            self.send(
                r#"{"jsonrpc":"2.0","id":"init","method":"initialize","params":{"abi_version":1,"api_version":"agentcpm-pd.operation/1","features":["subscriptions","credit"],"client":{"name":"test","version":"0"}}}"#,
            )
            .await;
            let resp = self.read_response("init").await;
            assert!(resp.get("result").is_some(), "initialize failed: {resp}");
        }

        async fn finish(self) -> i32 {
            drop(self.stdin);
            tokio::time::timeout(StdDuration::from_secs(10), self.task)
                .await
                .expect("engine did not exit")
                .expect("engine task panicked")
        }
    }

    // ---- P1-1: credit racing the sub.open response must reach the gateway ----

    #[tokio::test]
    async fn sub_open_response_is_sent_only_after_registration() {
        let (mut h, session) = Harness::start().await;
        h.initialize().await;
        h.send(r#"{"jsonrpc":"2.0","id":"so","method":"sub.open","params":{"stream":"run.events","input":{"runId":"run-1"},"credit":64}}"#)
            .await;
        let resp = h.read_response("so").await;
        let sub_id = resp["result"]["subscriptionId"]
            .as_str()
            .unwrap()
            .to_owned();

        // The host sends credit the instant it sees the subscription id.
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":"cr","method":"sub.credit","params":{{"subscriptionId":"{sub_id}","add":5}}}}"#
        ))
        .await;
        let credit_resp = h.read_response("cr").await;
        assert!(
            credit_resp.get("result").is_some(),
            "credit must be accepted, not rejected as unknown: {credit_resp}"
        );

        // The grant must reach the forwarder (and the upstream) — the stream
        // must not stall silently.
        let deadline = tokio::time::Instant::now() + StdDuration::from_secs(5);
        while session.grants().is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(StdDuration::from_millis(20)).await;
        }
        assert_eq!(session.grants(), vec![5], "credit must reach the gateway");
        assert_eq!(h.finish().await, 0);
    }

    // ---- P1-2: sub.close terminates the forwarder and synthesizes end(closed) ----

    #[tokio::test]
    async fn sub_close_stops_the_forwarder_and_ends_closed() {
        let (mut h, session) = Harness::start().await;
        h.initialize().await;
        h.send(r#"{"jsonrpc":"2.0","id":"so","method":"sub.open","params":{"stream":"run.events","input":{"runId":"run-1"},"credit":64}}"#)
            .await;
        let resp = h.read_response("so").await;
        let sub_id = resp["result"]["subscriptionId"]
            .as_str()
            .unwrap()
            .to_owned();

        session.inject_event("cur-1");
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":"sc","method":"sub.close","params":{{"subscriptionId":"{sub_id}"}}}}"#
        ))
        .await;
        // The flushed event may legally precede the close ack; collect it.
        let mut skipped = Vec::new();
        let close_resp = h.read_response_collect("sc", &mut skipped).await;
        assert!(
            close_resp.get("result").is_some(),
            "sub.close ack: {close_resp}"
        );
        let mut saw_event = skipped.iter().any(|value| value["method"] == "sub.event");

        // The forwarder must flush the queued event, then end closed with the
        // last delivered cursor.
        let mut saw_end = false;
        let deadline = tokio::time::Instant::now() + StdDuration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            let line = h.read_line().await;
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["method"] == "sub.event" {
                saw_event = true;
                assert_eq!(value["params"]["event"]["cursor"], "cur-1");
            }
            if value["method"] == "sub.end" {
                assert_eq!(value["params"]["terminal"], "closed");
                assert_eq!(value["params"]["resumeCursor"], "cur-1");
                saw_end = true;
                break;
            }
        }
        assert!(saw_event, "queued event must be flushed before sub.end");
        assert!(saw_end, "engine must synthesize sub.end closed");

        // The upstream close was requested.
        let deadline = tokio::time::Instant::now() + StdDuration::from_secs(5);
        while session.close_calls().is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(StdDuration::from_millis(20)).await;
        }
        assert_eq!(session.close_calls(), vec![sub_id]);
        assert_eq!(h.finish().await, 0);
    }

    // ---- P2-3: duplicate in-flight sub.open ids are rejected -----------------

    #[tokio::test]
    async fn duplicate_sub_open_request_id_is_rejected() {
        let (mut h, session) = Harness::start().await;
        h.initialize().await;
        session.set_slow_open();
        let frame = r#"{"jsonrpc":"2.0","id":"dup","method":"sub.open","params":{"stream":"run.events","input":{"runId":"run-1"},"credit":16}}"#;
        h.send(frame).await;
        // The first sub.open is still in flight (slow open), so the duplicate
        // id must be rejected before any state is touched.
        h.send(frame).await;
        let second = h.read_response("dup").await;
        assert_eq!(second["error"]["code"], -32600);
        assert!(
            second["error"]["message"]
                .as_str()
                .unwrap()
                .contains("already in flight")
        );
        session.open_release.notify_one();
        assert_eq!(h.finish().await, 0);
    }

    // ---- P2-4: abandon yields exactly one response -----------------------------

    #[tokio::test]
    async fn abandoned_request_never_produces_a_second_response() {
        let (mut h, session) = Harness::start().await;
        h.initialize().await;
        session.hold("run.get");
        h.send(r#"{"jsonrpc":"2.0","id":"r-9","method":"run.get","params":{"input":{"runId":"run-1"}}}"#)
            .await;
        h.send(r#"{"jsonrpc":"2.0","id":"ab","method":"conn.abandon","params":{"id":"r-9"}}"#)
            .await;

        // The original id gets exactly -32800...
        let abandoned = h.read_response("r-9").await;
        assert_eq!(abandoned["error"]["code"], -32800);
        assert!(h.read_response("ab").await.get("result").is_some());

        // ...and the late real result is dropped, not delivered.
        session.resolve(
            "r-9",
            json!({"outcome": "succeeded", "result": {"runId": "run-1"}}),
        );
        h.send(r#"{"jsonrpc":"2.0","id":"ping","method":"system.health","params":{"input":{}}}"#)
            .await;
        assert!(h.read_response("ping").await.get("result").is_some());

        let mut total_r9 = 0;
        // The ping response proves the done message was processed before it.
        // Drain any remaining lines for a bounded time and count r-9 frames.
        let deadline = tokio::time::Instant::now() + StdDuration::from_millis(300);
        while tokio::time::Instant::now() < deadline {
            let mut buf = String::new();
            match tokio::time::timeout(StdDuration::from_millis(50), h.stdout.read_line(&mut buf))
                .await
            {
                Ok(Ok(0)) => break,
                Ok(Ok(_)) => {
                    if buf.contains("\"id\":\"r-9\"") {
                        total_r9 += 1;
                    }
                }
                Ok(Err(_)) | Err(_) => continue,
            }
        }
        assert_eq!(total_r9, 0, "the late response must be dropped");
        assert_eq!(h.finish().await, 0);
    }

    // ---- T9 (WP5 e2e): a forwarder cloned after a generation bump must not
    // die immediately ------------------------------------------------------------

    /// The engine's own `sub_kill_rx` is never polled, so clones made after a
    /// reconnect keep the stale watch version and `changed()` fires at once —
    /// every post-reconnect subscription relayed nothing (found by the WP5
    /// T9 e2e against the real gateway). `forward_sub_open` marks the clone
    /// as seen; this test pins the watch semantics that fix relies on.
    #[tokio::test]
    async fn sub_kill_clone_after_generation_bump_is_quiet_when_marked() {
        let (tx, rx) = watch::channel(0u64);
        tx.send_replace(1);

        // Unmarked clone: fires immediately (the bug's mechanism).
        {
            let mut stale = rx.clone();
            let fired = stale.changed().await.is_ok();
            assert!(fired, "an unobserved clone must observe the bump");
        }

        // Marked clone (what forward_sub_open does now): stays quiet until a
        // NEW signal arrives.
        {
            let mut marked = rx.clone();
            marked.borrow_and_update();
            assert!(
                tokio::time::timeout(Duration::from_millis(100), marked.changed())
                    .await
                    .is_err(),
                "a marked clone must not fire without a new signal"
            );
            tx.send_replace(2);
            assert!(marked.changed().await.is_ok());
        }
        drop(tx);
    }

    // ---- P2-5: stale-generation announcements terminate immediately -------------

    #[tokio::test]
    async fn stale_generation_sub_open_is_terminated_disconnected() {
        let (mut h, session) = Harness::start().await;
        h.initialize().await;
        session.set_slow_open();
        h.send(r#"{"jsonrpc":"2.0","id":"so","method":"sub.open","params":{"stream":"run.events","input":{"runId":"run-1"},"credit":64}}"#)
            .await;
        tokio::time::sleep(StdDuration::from_millis(100)).await;

        // Upstream is lost while the open is in flight.
        session.closed.store(true, Ordering::SeqCst);
        let deadline = tokio::time::Instant::now() + StdDuration::from_secs(5);
        let mut saw_reconnecting = false;
        while tokio::time::Instant::now() < deadline {
            let line = h.read_line().await;
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["method"] == "conn.state" && value["params"]["state"] == "reconnecting" {
                saw_reconnecting = true;
                break;
            }
        }
        assert!(saw_reconnecting, "conn.state reconnecting must be emitted");

        // Now the open completes on the dead session: the announcement is
        // stale; the host gets the id plus an immediate disconnected end.
        session.open_release.notify_one();
        let resp = h.read_response("so").await;
        let sub_id = resp["result"]["subscriptionId"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut saw_end = false;
        let deadline = tokio::time::Instant::now() + StdDuration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            let line = h.read_line().await;
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["method"] == "sub.end" && value["params"]["subscriptionId"] == sub_id.as_str()
            {
                assert_eq!(value["params"]["terminal"], "disconnected");
                saw_end = true;
                break;
            }
        }
        assert!(saw_end, "stale subscription must end disconnected");

        // No registration happened: credit is unknown.
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":"cr","method":"sub.credit","params":{{"subscriptionId":"{sub_id}","add":1}}}}"#
        ))
        .await;
        let credit = h.read_response("cr").await;
        assert_eq!(
            credit["error"]["code"], -32602,
            "stale subscription must not be registered"
        );
        assert_eq!(h.finish().await, 0);
    }

    // ---- P2-6: saturated credit channel cannot stall the engine ----------------

    #[tokio::test]
    async fn saturated_credit_channel_does_not_stall_the_engine() {
        let (mut h, session) = Harness::start().await;
        h.initialize().await;
        session.set_grant_delay(200);
        h.send(r#"{"jsonrpc":"2.0","id":"so","method":"sub.open","params":{"stream":"run.events","input":{"runId":"run-1"},"credit":64}}"#)
            .await;
        let resp = h.read_response("so").await;
        let sub_id = resp["result"]["subscriptionId"]
            .as_str()
            .unwrap()
            .to_owned();

        // Flood far beyond the 64-slot forwarder credit channel.
        let mut saw_saturation = false;
        for i in 0..80u32 {
            h.send(&format!(
                r#"{{"jsonrpc":"2.0","id":"c{i}","method":"sub.credit","params":{{"subscriptionId":"{sub_id}","add":1}}}}"#
            ))
            .await;
        }
        for _ in 0..80u32 {
            let line = h.read_line().await;
            let value: Value = serde_json::from_str(&line).unwrap();
            if value.get("id").is_some() && value.get("error").is_some() {
                let msg = value["error"]["message"].as_str().unwrap_or_default();
                if msg.contains("saturated") {
                    saw_saturation = true;
                }
            }
        }
        assert!(
            saw_saturation,
            "saturated credits must be answered with an error, not dropped silently"
        );

        // The engine loop is still live.
        h.send(r#"{"jsonrpc":"2.0","id":"ping","method":"run.get","params":{"input":{"runId":"run-1"}}}"#)
            .await;
        assert!(h.read_response("ping").await.get("result").is_some());
        assert_eq!(h.finish().await, 0);
    }

    // ---- P1-2: reused ids must never be claimed by stale completions ----------

    #[tokio::test]
    async fn reused_request_id_is_not_claimed_by_stale_announcement() {
        let (mut h, session) = Harness::start().await;
        h.initialize().await;
        session.hold("run.get");
        session.set_slow_open();
        h.send(r#"{"jsonrpc":"2.0","id":"r-9","method":"sub.open","params":{"stream":"run.events","input":{"runId":"run-1"},"credit":64}}"#)
            .await;
        tokio::time::sleep(StdDuration::from_millis(100)).await;

        // Abandon the in-flight sub.open, then reuse its id for a business
        // request that stays held (so its pending entry is live).
        h.send(r#"{"jsonrpc":"2.0","id":"ab","method":"conn.abandon","params":{"id":"r-9"}}"#)
            .await;
        let abandoned = h.read_response("r-9").await;
        assert_eq!(abandoned["error"]["code"], -32800);
        h.send(r#"{"jsonrpc":"2.0","id":"r-9","method":"run.get","params":{"input":{"runId":"run-1"}}}"#)
            .await;
        tokio::time::sleep(StdDuration::from_millis(50)).await;

        // Now the stale sub.open completes: its announcement must NOT claim
        // the business request's pending entry and must not answer r-9.
        session.open_release.notify_one();
        tokio::time::sleep(StdDuration::from_millis(200)).await;

        // The business request answers with its own response.
        session.resolve(
            "r-9",
            json!({"outcome": "succeeded", "result": {"marker": "biz"}}),
        );
        let deadline = tokio::time::Instant::now() + StdDuration::from_secs(5);
        let mut saw_biz = false;
        let mut saw_subscription_answer = false;
        while tokio::time::Instant::now() < deadline {
            let line = h.read_line().await;
            let value: Value = serde_json::from_str(&line).unwrap();
            if value.get("id").and_then(Value::as_str) != Some("r-9") {
                continue;
            }
            if value["result"]["result"]["marker"] == "biz" {
                saw_biz = true;
                break;
            }
            if value["result"].get("subscriptionId").is_some() {
                saw_subscription_answer = true;
            }
        }
        assert!(
            saw_biz,
            "the business request must be answered with its own response"
        );
        assert!(
            !saw_subscription_answer,
            "the stale sub.open announcement must not answer a reused id"
        );
        assert_eq!(h.finish().await, 0);
    }

    #[tokio::test]
    async fn reused_request_id_is_not_claimed_by_stale_completion() {
        let (mut h, session) = Harness::start().await;
        h.initialize().await;
        session.hold("run.get");
        h.send(r#"{"jsonrpc":"2.0","id":"r-9","method":"run.get","params":{"input":{"runId":"run-1"}}}"#)
            .await;
        tokio::time::sleep(StdDuration::from_millis(50)).await;

        // Complete the old request, then immediately abandon the id and reuse
        // it; the stale completion message races the reuse.
        session.resolve(
            "r-9",
            json!({"outcome": "succeeded", "result": {"marker": "old"}}),
        );
        tokio::time::sleep(StdDuration::from_millis(100)).await;
        h.send(r#"{"jsonrpc":"2.0","id":"ab","method":"conn.abandon","params":{"id":"r-9"}}"#)
            .await;
        h.send(r#"{"jsonrpc":"2.0","id":"r-9","method":"system.health","params":{"input":{}}}"#)
            .await;

        // The new request's own response MUST arrive (a stale completion
        // claiming the reused id would drop it), and at most one old-marker
        // response may ever be delivered.
        let deadline = tokio::time::Instant::now() + StdDuration::from_secs(5);
        let mut saw_health = false;
        let mut old_markers = 0;
        while tokio::time::Instant::now() < deadline {
            let line = h.read_line().await;
            let value: Value = serde_json::from_str(&line).unwrap();
            if value.get("id").and_then(Value::as_str) != Some("r-9") {
                continue;
            }
            if value["result"]["result"]["mock"] == "system.health" {
                saw_health = true;
                break;
            }
            if value["result"]["result"]["marker"] == "old" {
                old_markers += 1;
            }
        }
        assert!(
            saw_health,
            "the reused request must be answered with its own response"
        );
        assert!(
            old_markers <= 1,
            "at most one old-response may be delivered"
        );
        assert_eq!(h.finish().await, 0);
    }

    // ---- P2-7: exact 1 MiB event boundary --------------------------------------

    #[tokio::test]
    async fn oversized_event_at_exact_boundary_terminates_with_resume_cursor() {
        let (mut h, session) = Harness::start().await;
        h.initialize().await;
        h.send(r#"{"jsonrpc":"2.0","id":"so","method":"sub.open","params":{"stream":"run.events","input":{"runId":"run-1"},"credit":64}}"#)
            .await;
        let resp = h.read_response("so").await;
        let sub_id = resp["result"]["subscriptionId"]
            .as_str()
            .unwrap()
            .to_owned();

        // Deliver one small event first (its cursor becomes the resume point).
        session.inject_event("cur-small");
        let line = h.read_line().await;
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["method"], "sub.event");

        // Build an event whose serialized frame content is exactly MAX bytes.
        let tx = session.inner.lock().unwrap().sub_txs[0].clone();
        let probe = SubEventNotification {
            subscription_id: sub_id.clone(),
            event: agentcpm_pd_protocol::contract::EventEnvelope {
                version: "agentcpm-pd.event/1".to_owned(),
                instance_id: "local-mock-1".to_owned(),
                run_id: Some("run-1".to_owned()),
                operation_id: None,
                subscription_id: None,
                cursor: "cur-huge".to_owned(),
                event_type: "run.progress".to_owned(),
                data: serde_json::Map::new(),
            },
        };
        // Measure with a one-char blob: every extra 'a' adds exactly one
        // byte (no escaping), so the exact pad is derivable.
        let mut one = probe.clone();
        one.event
            .data
            .insert("blob".to_owned(), Value::String("a".to_owned()));
        let notif_one = JsonRpcNotification {
            jsonrpc: "2.0".to_owned(),
            method: WIRE_SUB_EVENT.to_owned(),
            params: serde_json::to_value(&one).unwrap(),
        };
        let len_one = serde_json::to_string(&notif_one).unwrap().len();
        assert!(len_one < MAX_FRAME_BYTES);
        let pad = MAX_FRAME_BYTES - len_one + 1;
        let mut huge = probe.clone();
        huge.event
            .data
            .insert("blob".to_owned(), Value::String("a".repeat(pad)));
        let notif_huge = JsonRpcNotification {
            jsonrpc: "2.0".to_owned(),
            method: WIRE_SUB_EVENT.to_owned(),
            params: serde_json::to_value(&huge).unwrap(),
        };
        assert_eq!(
            serde_json::to_string(&notif_huge).unwrap().len(),
            MAX_FRAME_BYTES
        );
        let _ = tx.try_send(SubStreamItem::Event(huge));

        // The engine must end the subscription with INPUT_TOO_LARGE and the
        // last DELIVERED cursor, not the oversized one.
        let mut saw_end = false;
        let deadline = tokio::time::Instant::now() + StdDuration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            let line = h.read_line().await;
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["method"] == "sub.end" {
                assert_eq!(value["params"]["terminal"], "error");
                assert_eq!(value["params"]["error"]["code"], "INPUT_TOO_LARGE");
                assert_eq!(value["params"]["resumeCursor"], "cur-small");
                saw_end = true;
                break;
            }
        }
        assert!(saw_end, "oversized event must terminate the subscription");
        assert_eq!(h.finish().await, 0);
    }
}
