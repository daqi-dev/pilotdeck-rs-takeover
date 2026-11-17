//! Multiplexed `pd-connect/1` over `/v1/ws` client state machine (`007 §6, WP2`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use agentcpm_pd_protocol::contract::{OPERATION_VERSION, OperationResponse, ResponseOutcome};
use agentcpm_pd_protocol::envelope::{
    bind_instance_id, operation_request_to_rpc, unary_timeout_ms,
};
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_RESULT_UNKNOWN, ContractError, EffectStatus, RetrySpec,
};
use agentcpm_pd_protocol::id::{IdGenerator, NodeId};
use agentcpm_pd_protocol::registry::{
    OperationRegistry, RetryKind, validation_errors_to_contract_error,
};
use agentcpm_pd_protocol::wire::{
    ABI_VERSION, ClientDescriptor, ConnAbandonParams, ConnHeartbeatNotification, DEFAULT_CREDIT,
    InitializeParams, InitializeResult, JSONRPC_VERSION, JsonRpcErrorObject, JsonRpcNotification,
    JsonRpcRequest, JsonRpcResponse, MAX_FRAME_BYTES, OperationRpcResult,
    SUBSCRIPTION_BUFFER_ITEMS, SubCloseParams, SubCreditParams, SubEndNotification,
    SubEventNotification, SubOpenParams, SubOpenResult, TargetHandshakeInfo, WIRE_CONN_ABANDON,
    WIRE_CONN_HEARTBEAT, WIRE_INITIALIZE, WIRE_SUB_CLOSE, WIRE_SUB_CREDIT, WIRE_SUB_END,
    WIRE_SUB_EVENT, WIRE_SUB_OPEN,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

use crate::error::ClientError;

const BACKOFF_SCHEDULE_MS: &[u64] = &[500, 1_000, 2_000, 4_000, 8_000, 10_000];

#[derive(Clone, Debug)]
pub struct PdWsConnectOptions {
    pub ws_url: String,
    pub token: Option<String>,
    pub expected_instance_id: Option<String>,
    pub client_name: String,
    pub client_version: String,
    pub id_gen: Arc<IdGenerator>,
}

impl PdWsConnectOptions {
    pub fn for_local(
        ws_url: impl Into<String>,
        token: impl Into<String>,
        expected_instance_id: impl Into<String>,
    ) -> Self {
        Self {
            ws_url: ws_url.into(),
            token: Some(token.into()),
            expected_instance_id: Some(expected_instance_id.into()),
            client_name: "agentcpm-pd".to_owned(),
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
            id_gen: Arc::new(IdGenerator::new(NodeId::local())),
        }
    }
}

#[derive(Clone, Debug)]
pub enum SubStreamItem {
    Event(SubEventNotification),
    End(SubEndNotification),
}

enum RawRpcOutcome {
    Result(Value),
    Error(JsonRpcErrorObject),
    Disconnected(String),
}

struct SubRegistryEntry {
    tx: mpsc::Sender<SubStreamItem>,
}

/// Sub.event/sub.end frames observed for a subscription whose `sub.open` response
/// has resolved but whose registry entry has not been installed yet. The reader
/// task parks them here so the tiny registration window cannot drop events;
/// `open_subscription` flushes them (in arrival order) while installing the
/// entry under the same router lock.
///
/// Bounded by item count and bytes; eviction is insertion-order (FIFO) so
/// behavior is deterministic. Frames for ids that this process has already
/// registered and ended are dropped by the tombstone set below, never parked.
#[derive(Default)]
struct ClientRouterState {
    pending: HashMap<String, oneshot::Sender<RawRpcOutcome>>,
    subscriptions: HashMap<String, SubRegistryEntry>,
    parked_sub_items: HashMap<String, VecDeque<SubStreamItem>>,
    parked_order: VecDeque<String>,
    parked_sub_items_total: usize,
    parked_sub_bytes: usize,
    /// Subscription ids that were registered and have since ended (sub.end or
    /// sub.close). Late frames for them are dropped instead of parked; the
    /// set is a bounded FIFO so long-lived connections cannot grow it.
    ended_ids: HashSet<String>,
    ended_order: VecDeque<String>,
    last_heartbeat_at: Option<Instant>,
    closed_reason: Option<String>,
}

const ENDED_TOMBSTONE_CAP: usize = 4096;

/// Records that a subscription id has ended in this process.
fn mark_ended(state: &mut ClientRouterState, id: &str) {
    if state.ended_ids.insert(id.to_owned()) {
        state.ended_order.push_back(id.to_owned());
        while state.ended_order.len() > ENDED_TOMBSTONE_CAP {
            match state.ended_order.pop_front() {
                Some(oldest) => {
                    state.ended_ids.remove(&oldest);
                }
                None => break,
            }
        }
    }
}

/// Removes a subscription entry and tombstones the id, so late frames are
/// dropped rather than parked.
fn remove_subscription_entry(state: &mut ClientRouterState, id: &str) -> Option<SubRegistryEntry> {
    let entry = state.subscriptions.remove(id);
    if entry.is_some() {
        mark_ended(state, id);
    }
    entry
}

/// Flushes frames parked for `id` during the sub.open registration window, in
/// arrival order, into the freshly created subscription channel.
///
/// A terminal frame parked in the window means the subscription ended before
/// its entry was installed (review round-3 P1-B): after flushing, the entry
/// is removed and the id tombstoned so it cannot linger in the registry
/// forever and late frames are dropped. The terminal frame is the last frame
/// delivered — anything parked after it is discarded, matching the direct
/// (already-registered) path where post-`sub.end` events are dropped by the
/// tombstone (wp3fix3 review P2).
fn flush_parked_sub_items(
    state: &mut ClientRouterState,
    id: &str,
    sub_tx: &mpsc::Sender<SubStreamItem>,
) {
    let Some(parked) = state.parked_sub_items.remove(id) else {
        return;
    };
    state.parked_order.retain(|p| p != id);
    state.parked_sub_items_total = state.parked_sub_items_total.saturating_sub(parked.len());
    state.parked_sub_bytes = state.parked_sub_bytes.saturating_sub(
        parked
            .iter()
            .map(|item| estimate_parked_item_bytes(id, item))
            .sum(),
    );
    let ended_during_window = parked
        .iter()
        .any(|item| matches!(item, SubStreamItem::End(_)));
    let mut seen_terminal = false;
    for item in parked {
        if seen_terminal {
            // Trailing frames after the terminal: undeliverable by contract
            // (consumers stop at `sub.end`), so drop instead of delivering
            // out of order.
            continue;
        }
        if matches!(item, SubStreamItem::End(_)) {
            seen_terminal = true;
        }
        let _ = sub_tx.try_send(item);
    }
    if ended_during_window {
        remove_subscription_entry(state, id);
    }
}

const PARKED_SUB_ITEMS_PER_SUB: usize = SUBSCRIPTION_BUFFER_ITEMS;
const PARKED_SUB_ITEMS_TOTAL: usize = SUBSCRIPTION_BUFFER_ITEMS * 4;
const PARKED_SUB_BYTES_TOTAL: usize = agentcpm_pd_protocol::wire::SUBSCRIPTION_BUFFER_BYTES;

/// Honest per-item footprint for the parked byte budget: the FULL serialized
/// notification (including the upstream-controlled `subscription_id`,
/// `resume_cursor`, `error`, and `gap` fields, each individually bounded only
/// by the 1 MiB frame cap) plus the map-key copy of the id. The old fixed
/// 256-byte charge for `End` let adversarial frames accumulate GiBs of real
/// residency while the ledger read <= 4 MiB (review P1-4).
fn estimate_parked_item_bytes(id: &str, item: &SubStreamItem) -> usize {
    let serialized = match item {
        SubStreamItem::Event(ev) => serde_json::to_string(ev),
        SubStreamItem::End(end) => serde_json::to_string(end),
    };
    serialized
        .map(|s| s.len() + id.len() + 128)
        // Unserializable items are charged as over-budget (rejected below).
        .unwrap_or(PARKED_SUB_BYTES_TOTAL.saturating_add(1))
}

fn drop_oldest_parked(state: &mut ClientRouterState) -> bool {
    while let Some(oldest_id) = state.parked_order.front().cloned() {
        // Defensive: a dangling order entry must never panic the reader task
        // (a panic would skip `closed`/fail_all_pending and zombie the link).
        let Some(queue) = state.parked_sub_items.get_mut(&oldest_id) else {
            state.parked_order.pop_front();
            continue;
        };
        match queue.pop_front() {
            Some(item) => {
                state.parked_sub_items_total = state.parked_sub_items_total.saturating_sub(1);
                state.parked_sub_bytes = state
                    .parked_sub_bytes
                    .saturating_sub(estimate_parked_item_bytes(&oldest_id, &item));
                return true;
            }
            None => {
                state.parked_sub_items.remove(&oldest_id);
                state.parked_order.pop_front();
            }
        }
    }
    false
}

fn park_sub_item(state: &mut ClientRouterState, id: &str, item: SubStreamItem) {
    let bytes = estimate_parked_item_bytes(id, &item);
    // Pre-enqueue rejection: a single item that cannot fit into the byte
    // budget even with everything else evicted is dropped outright instead of
    // parking an over-budget entry (review P1-4).
    if bytes >= PARKED_SUB_BYTES_TOTAL {
        return;
    }
    // Invariant: an id appears in parked_order exactly once, for as long as
    // its key exists in parked_sub_items (even when eviction has emptied the
    // queue). Pushing on `is_empty` instead of key-membership could add a
    // duplicate after an eviction, which later crashed the reader.
    let is_new_key = !state.parked_sub_items.contains_key(id);
    let queue = state.parked_sub_items.entry(id.to_owned()).or_default();
    if is_new_key {
        state.parked_order.push_back(id.to_owned());
    }
    if queue.len() >= PARKED_SUB_ITEMS_PER_SUB
        && let Some(dropped) = queue.pop_front()
    {
        state.parked_sub_items_total = state.parked_sub_items_total.saturating_sub(1);
        state.parked_sub_bytes = state
            .parked_sub_bytes
            .saturating_sub(estimate_parked_item_bytes(id, &dropped));
    }
    queue.push_back(item);
    state.parked_sub_items_total += 1;
    state.parked_sub_bytes += bytes;

    while (state.parked_sub_items_total > PARKED_SUB_ITEMS_TOTAL
        || state.parked_sub_bytes > PARKED_SUB_BYTES_TOTAL)
        && drop_oldest_parked(state)
    {}
}

/// Active multiplexed WebSocket session bound to a verified Gateway instance.
#[derive(Clone)]
pub struct PdWsClient {
    tx: mpsc::Sender<Message>,
    router: Arc<Mutex<ClientRouterState>>,
    closed: Arc<AtomicBool>,
    init_result: InitializeResult,
    id_gen: Arc<IdGenerator>,
    reader_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    writer_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

pub struct PdWsSubscription {
    pub subscription_id: String,
    pub credit_window: u32,
    rx: mpsc::Receiver<SubStreamItem>,
    client: PdWsClient,
    auto_credit: bool,
    unacked_events: u32,
    closed: bool,
}

impl PdWsSubscription {
    pub async fn recv(&mut self) -> Option<SubStreamItem> {
        let item = self.rx.recv().await?;
        if matches!(item, SubStreamItem::Event(_)) && self.auto_credit {
            self.unacked_events = self.unacked_events.saturating_add(1);
            let threshold = (self.credit_window / 2).max(1);
            if self.unacked_events >= threshold {
                let grant = self.unacked_events;
                self.unacked_events = 0;
                let _ = self.client.send_credit(&self.subscription_id, grant).await;
            }
        }
        Some(item)
    }

    /// Non-blocking receive: a buffered item if one is immediately ready.
    pub fn try_recv(&mut self) -> Option<SubStreamItem> {
        self.rx.try_recv().ok()
    }

    pub async fn grant_credit(&mut self, credit: u32) -> Result<(), ClientError> {
        self.client.send_credit(&self.subscription_id, credit).await
    }

    pub async fn close(mut self) -> Result<(), ClientError> {
        self.closed = true;
        self.client.send_sub_close(&self.subscription_id).await
    }
}

impl PdWsClient {
    /// Connects to `/v1/ws`, authenticates with bearer token, and runs mandatory `initialize`.
    pub async fn connect(opts: PdWsConnectOptions) -> Result<Self, ClientError> {
        let mut request = opts.ws_url.as_str().into_client_request().map_err(|e| {
            ClientError::usage(format!("invalid WebSocket URL `{}`: {e}", opts.ws_url))
        })?;

        if let Some(tok) = &opts.token
            && !tok.trim().is_empty()
        {
            let hv = HeaderValue::from_str(&format!("Bearer {}", tok.trim())).map_err(|e| {
                ClientError::config(format!("invalid bearer token header value: {e}"))
            })?;
            request.headers_mut().insert("authorization", hv);
        }

        let (ws_stream, _) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| ClientError::from_ws_auth_or_connect(e.to_string()))?;

        let (mut ws_sink, mut ws_source) = ws_stream.split();

        let (tx, mut out_rx) = mpsc::channel::<Message>(256);
        let router = Arc::new(Mutex::new(ClientRouterState::default()));
        let closed = Arc::new(AtomicBool::new(false));

        let closed_w = closed.clone();
        let router_w = router.clone();
        let writer_handle = tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if ws_sink.send(msg).await.is_err() {
                    closed_w.store(true, Ordering::SeqCst);
                    fail_all_pending(&router_w, "websocket write failed").await;
                    break;
                }
            }
            let _ = ws_sink.close().await;
        });

        let closed_r = closed.clone();
        let router_r = router.clone();
        let reader_handle = tokio::spawn(async move {
            // Panic guard: any exit from this task — including an unwind from
            // a future bug — must set `closed` so callers stop issuing requests
            // and the engine watchdog reconnects instead of zombie-ing the
            // link (fail_all_pending runs on the normal paths below).
            struct ClosedOnDrop(Arc<AtomicBool>);
            impl Drop for ClosedOnDrop {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
            let _guard = ClosedOnDrop(closed_r.clone());
            while let Some(next) = ws_source.next().await {
                match next {
                    Ok(Message::Text(txt)) => {
                        if txt.len() > MAX_FRAME_BYTES {
                            continue;
                        }
                        if let Ok(val) = serde_json::from_str::<Value>(&txt) {
                            dispatch_incoming_frame(&router_r, val).await;
                        }
                    }
                    Ok(Message::Close(_)) => {
                        closed_r.store(true, Ordering::SeqCst);
                        fail_all_pending(&router_r, "server closed websocket connection").await;
                        break;
                    }
                    Ok(Message::Ping(_)) | Ok(Message::Pong(_)) | Ok(Message::Binary(_)) => {}
                    Ok(Message::Frame(_)) => {}
                    Err(e) => {
                        closed_r.store(true, Ordering::SeqCst);
                        fail_all_pending(&router_r, &format!("websocket read error: {e}")).await;
                        break;
                    }
                }
            }
            closed_r.store(true, Ordering::SeqCst);
            fail_all_pending(&router_r, "websocket stream ended").await;
        });

        let client = Self {
            tx,
            router,
            closed,
            init_result: InitializeResult {
                abi_version: ABI_VERSION,
                supported: false,
                api_version: OPERATION_VERSION.to_owned(),
                supported_features: Vec::new(),
                connection_id: String::new(),
                cli: None,
                target: TargetHandshakeInfo {
                    name: String::new(),
                    instance_id: String::new(),
                    node_id: "local".to_owned(),
                    gateway_version: String::new(),
                    schema_digest: String::new(),
                    transport: "ws".to_owned(),
                },
                limits: agentcpm_pd_protocol::wire::ProtocolLimits::default(),
            },
            id_gen: opts.id_gen.clone(),
            reader_task: Arc::new(Mutex::new(Some(reader_handle))),
            writer_task: Arc::new(Mutex::new(Some(writer_handle))),
        };

        let init_params = InitializeParams {
            abi_version: ABI_VERSION,
            api_version: OPERATION_VERSION.to_owned(),
            features: vec!["subscriptions".to_owned(), "control-priority".to_owned()],
            client: ClientDescriptor {
                name: opts.client_name,
                version: opts.client_version,
                pid: Some(std::process::id()),
            },
            credential: opts.token.clone(),
        };
        let params_val = serde_json::to_value(init_params).map_err(|e| {
            ClientError::connection_failed(format!("failed to serialize initialize params: {e}"))
        })?;
        let raw_val = match client
            .send_raw_rpc_inner(WIRE_INITIALIZE, params_val, Duration::from_secs(10), false)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                client.close().await;
                return Err(e);
            }
        };

        let init_result: InitializeResult =
            serde_json::from_value(raw_val).map_err(|e| ClientError::ProtocolError {
                code: -32603,
                message: format!("malformed initialize result: {e}"),
            })?;

        if init_result.abi_version != ABI_VERSION
            || !init_result.supported
            || init_result.api_version != OPERATION_VERSION
        {
            client.close().await;
            return Err(ClientError::ProtocolError {
                code: agentcpm_pd_protocol::errors::ERR_UNSUPPORTED_VERSION,
                message: format!(
                    "incompatible server ABI/api: {} / {} (supported={})",
                    init_result.abi_version, init_result.api_version, init_result.supported
                ),
            });
        }

        if let Some(expected) = &opts.expected_instance_id
            && &init_result.target.instance_id != expected
        {
            let actual = init_result.target.instance_id.clone();
            client.close().await;
            return Err(ClientError::InstanceChanged {
                expected: expected.clone(),
                actual,
            });
        }

        Ok(Self {
            init_result,
            ..client
        })
    }

    /// Reconnects using deterministic exponential backoff (`0.5s ..= 10s`), stopping immediately
    /// on unrecoverable auth/instance-mismatch errors (`007 §6.5, WP2`).
    pub async fn connect_with_backoff(
        opts: PdWsConnectOptions,
        max_attempts: usize,
    ) -> Result<Self, ClientError> {
        let mut last_err = ClientError::connection_failed("no attempts executed");
        let total = max_attempts.max(1);
        for attempt in 0..total {
            match Self::connect(opts.clone()).await {
                Ok(client) => return Ok(client),
                Err(err) => {
                    let unrecoverable = matches!(
                        &err,
                        ClientError::InstanceChanged { .. }
                            | ClientError::NotInitialized(_)
                            | ClientError::Usage(_)
                            | ClientError::Config(_)
                    ) || matches!(&err, ClientError::Contract(c) if matches!(
                        c.code.as_str(),
                        "INSTANCE_MISMATCH" | "UNAUTHENTICATED" | "FORBIDDEN"
                    ));
                    if unrecoverable {
                        return Err(err);
                    }
                    last_err = err;
                    if attempt + 1 < total {
                        let wait_ms =
                            BACKOFF_SCHEDULE_MS[attempt.min(BACKOFF_SCHEDULE_MS.len() - 1)];
                        tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                    }
                }
            }
        }
        Err(last_err)
    }

    pub fn backoff_schedule_ms() -> &'static [u64] {
        BACKOFF_SCHEDULE_MS
    }

    pub fn bound_instance_id(&self) -> &str {
        &self.init_result.target.instance_id
    }

    pub fn target_info(&self) -> &TargetHandshakeInfo {
        &self.init_result.target
    }

    pub fn initialize_result(&self) -> &InitializeResult {
        &self.init_result
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub async fn last_heartbeat_at(&self) -> Option<Instant> {
        self.router.lock().await.last_heartbeat_at
    }

    /// Executes a typed contract operation: conditional `instanceId` injection, input schema check,
    /// unary timeout -> `RESULT_UNKNOWN`, and `OperationResponse` decoding.
    pub async fn call_operation(
        &self,
        method_id: &str,
        mut input: Value,
        idempotency_key: Option<String>,
        expected_revision: Option<String>,
        timeout_override: Option<Duration>,
    ) -> Result<OperationResponse, ClientError> {
        let reg = OperationRegistry::global();
        let spec = reg.get(method_id).ok_or_else(|| {
            ClientError::Contract(ContractError::new(
                agentcpm_pd_protocol::errors::CODE_UNSUPPORTED_OPERATION,
                format!("unknown operation `{method_id}`"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ))
        })?;

        bind_instance_id(
            &mut input,
            &self.init_result.target.instance_id,
            spec.declares_instance_id,
        )?;
        reg.validate_input(method_id, &input)
            .map_err(|errs| ClientError::Contract(validation_errors_to_contract_error(errs)))?;

        if spec.retry == RetryKind::IdempotentSubmit
            && idempotency_key.as_deref().unwrap_or("").is_empty()
        {
            return Err(ClientError::ProtocolError {
                code: agentcpm_pd_protocol::errors::ERR_MISSING_IDEMPOTENCY_KEY,
                message: format!("operation `{method_id}` requires idempotencyKey"),
            });
        }

        let request_id = self.id_gen.next_id();
        let timeout_ms = unary_timeout_ms(&input);
        let timeout = timeout_override.unwrap_or_else(|| Duration::from_millis(timeout_ms));

        let params_map = input.as_object().cloned().unwrap_or_default();
        let op_req = agentcpm_pd_protocol::contract::OperationRequest {
            version: OPERATION_VERSION.to_owned(),
            request_id: request_id.clone(),
            method_id: method_id.to_owned(),
            target: agentcpm_pd_protocol::contract::Target {
                instance_id: self.init_result.target.instance_id.clone(),
                project_id: params_map
                    .get("projectId")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                session_id: params_map
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                run_id: params_map
                    .get("runId")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                interaction_id: params_map
                    .get("interactionId")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                operation_id: params_map
                    .get("operationId")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
            },
            params: params_map,
            idempotency_key,
            expected_revision,
        };
        let rpc = operation_request_to_rpc(&op_req);

        let is_write = spec.retry != RetryKind::ReadOnly;
        let raw_val = self
            .send_raw_rpc_inner(&rpc.method, rpc.params, timeout, is_write)
            .await?;

        let rpc_res: OperationRpcResult =
            serde_json::from_value(raw_val).map_err(|e| ClientError::ProtocolError {
                code: -32603,
                message: format!("malformed operation response for `{method_id}`: {e}"),
            })?;

        Ok(OperationResponse {
            version: OPERATION_VERSION.to_owned(),
            request_id,
            outcome: rpc_res.outcome,
            result: Some(rpc_res.result),
            error: None,
        })
    }

    /// Low-level JSON-RPC call useful for `api connect` forwarding or negative wire testing.
    ///
    /// The caller-supplied id is correlation only: the wire (and the router
    /// pending table) always uses a freshly generated client id, mirroring
    /// `sub.open`. Host ids may be legally reused after completion or abandon
    /// (`007 §6.3`), so keying the router on them let a late response hijack
    /// a reused id's in-flight entry (review round-3 P1-A).
    pub async fn send_raw_rpc(
        &self,
        _request_id: String,
        method: &str,
        params: Value,
        timeout: Duration,
        treat_disconnect_as_result_unknown: bool,
    ) -> Result<Value, ClientError> {
        self.send_raw_rpc_inner(method, params, timeout, treat_disconnect_as_result_unknown)
            .await
    }

    async fn send_raw_rpc_inner(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        is_mutating_after_send: bool,
    ) -> Result<Value, ClientError> {
        if self.is_closed() {
            return Err(ClientError::connection_failed(
                "websocket connection is closed",
            ));
        }

        // Client-generated wire id: unique per call, immune to caller-side
        // id reuse (review round-3 P1-A).
        let request_id = self.id_gen.next_id();

        let idempotency_key = params
            .get("idempotencyKey")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);

        let rpc = JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: request_id.clone(),
            method: method.to_owned(),
            params,
        };
        let serialized = serde_json::to_string(&rpc).map_err(|e| {
            ClientError::usage(format!("failed to serialize request `{method}`: {e}"))
        })?;
        if serialized.len() > MAX_FRAME_BYTES {
            return Err(ClientError::Contract(ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!(
                    "request frame size {} exceeds 1 MiB limit",
                    serialized.len()
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )));
        }

        let (resp_tx, resp_rx) = oneshot::channel();
        {
            let mut guard = self.router.lock().await;
            if let Some(reason) = &guard.closed_reason {
                return Err(ClientError::connection_failed(reason.clone()));
            }
            guard.pending.insert(request_id.clone(), resp_tx);
        }

        if self
            .tx
            .send(Message::Text(serialized.into()))
            .await
            .is_err()
        {
            self.router.lock().await.pending.remove(&request_id);
            return Err(ClientError::connection_failed(
                "failed to enqueue frame before send",
            ));
        }

        match tokio::time::timeout(timeout, resp_rx).await {
            Ok(Ok(RawRpcOutcome::Result(val))) => Ok(val),
            Ok(Ok(RawRpcOutcome::Error(err))) => Err(map_jsonrpc_error(err)),
            Ok(Ok(RawRpcOutcome::Disconnected(reason))) => {
                if is_mutating_after_send {
                    Err(ClientError::Contract(ContractError::result_unknown(
                        idempotency_key.as_deref(),
                        format!(
                            "connection lost while awaiting response for `{method}` (`{request_id}`): {reason}"
                        ),
                    )))
                } else {
                    Err(ClientError::connection_failed(format!(
                        "connection closed while waiting for `{method}`: {reason}"
                    )))
                }
            }
            Ok(Err(_)) => {
                if is_mutating_after_send {
                    Err(ClientError::Contract(ContractError::result_unknown(
                        idempotency_key.as_deref(),
                        format!(
                            "connection dropped while awaiting response for `{method}` (`{request_id}`)"
                        ),
                    )))
                } else {
                    Err(ClientError::connection_failed(format!(
                        "connection dropped while waiting for `{method}`"
                    )))
                }
            }
            Err(_) => {
                self.router.lock().await.pending.remove(&request_id);
                let _ = self.send_abandon(&request_id).await;
                if is_mutating_after_send {
                    Err(ClientError::Contract(ContractError::result_unknown(
                        idempotency_key.as_deref(),
                        format!(
                            "operation `{method}` timed out after {}ms; query requests.lookup or retry with the same idempotencyKey",
                            timeout.as_millis()
                        ),
                    )))
                } else {
                    Err(ClientError::Contract(ContractError {
                        code: CODE_RESULT_UNKNOWN.to_owned(),
                        message: format!(
                            "request `{method}` timed out after {}ms",
                            timeout.as_millis()
                        ),
                        details: None,
                        retry: RetrySpec::backoff_read(Some(500)),
                        effect: EffectStatus::Unknown,
                    }))
                }
            }
        }
    }

    pub async fn send_abandon(&self, request_id: &str) -> Result<(), ClientError> {
        let notif = JsonRpcNotification {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            method: WIRE_CONN_ABANDON.to_owned(),
            params: serde_json::to_value(ConnAbandonParams {
                id: request_id.to_owned(),
            })
            .unwrap_or(Value::Null),
        };
        let text = serde_json::to_string(&notif).unwrap_or_default();
        self.tx
            .send(Message::Text(text.into()))
            .await
            .map_err(|_| ClientError::connection_failed("connection closed sending conn.abandon"))
    }

    pub async fn open_subscription(
        &self,
        method_id: &str,
        mut input: Value,
        initial_credit: Option<u32>,
        auto_credit: bool,
    ) -> Result<PdWsSubscription, ClientError> {
        let reg = OperationRegistry::global();
        let spec = reg.get(method_id).ok_or_else(|| {
            ClientError::Contract(ContractError::new(
                agentcpm_pd_protocol::errors::CODE_UNSUPPORTED_OPERATION,
                format!("unknown stream operation `{method_id}`"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ))
        })?;
        bind_instance_id(
            &mut input,
            &self.init_result.target.instance_id,
            spec.declares_instance_id,
        )?;
        reg.validate_input(method_id, &input)
            .map_err(|errs| ClientError::Contract(validation_errors_to_contract_error(errs)))?;

        let credit = initial_credit.unwrap_or(DEFAULT_CREDIT);
        let open_params = SubOpenParams {
            stream: method_id.to_owned(),
            input,
            credit: Some(credit),
        };
        let raw = self
            .send_raw_rpc_inner(
                WIRE_SUB_OPEN,
                serde_json::to_value(open_params).unwrap_or(Value::Null),
                Duration::from_secs(30),
                false,
            )
            .await?;

        let opened: SubOpenResult =
            serde_json::from_value(raw).map_err(|e| ClientError::ProtocolError {
                code: -32603,
                message: format!("malformed sub.open result: {e}"),
            })?;

        let (sub_tx, sub_rx) = mpsc::channel(SUBSCRIPTION_BUFFER_ITEMS);
        {
            let mut guard = self.router.lock().await;
            guard.subscriptions.insert(
                opened.subscription_id.clone(),
                SubRegistryEntry { tx: sub_tx.clone() },
            );
            // Flush frames parked during the response->registration window.
            flush_parked_sub_items(&mut guard, &opened.subscription_id, &sub_tx);
        }

        Ok(PdWsSubscription {
            subscription_id: opened.subscription_id,
            credit_window: credit,
            rx: sub_rx,
            client: self.clone(),
            auto_credit,
            unacked_events: 0,
            closed: false,
        })
    }

    pub async fn send_credit(&self, subscription_id: &str, add: u32) -> Result<(), ClientError> {
        let params = serde_json::to_value(SubCreditParams {
            subscription_id: subscription_id.to_owned(),
            add,
        })
        .unwrap_or(Value::Null);
        self.send_raw_rpc_inner(WIRE_SUB_CREDIT, params, Duration::from_secs(10), false)
            .await
            .map(|_| ())
    }

    pub async fn send_sub_close(&self, subscription_id: &str) -> Result<(), ClientError> {
        let params = serde_json::to_value(SubCloseParams {
            subscription_id: subscription_id.to_owned(),
        })
        .unwrap_or(Value::Null);
        let res = self
            .send_raw_rpc_inner(WIRE_SUB_CLOSE, params, Duration::from_secs(10), false)
            .await;
        {
            let mut guard = self.router.lock().await;
            remove_subscription_entry(&mut guard, subscription_id);
        }
        res.map(|_| ())
    }

    pub async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let _ = self.tx.send(Message::Close(None)).await;
        if let Some(h) = self.reader_task.lock().await.take() {
            h.abort();
        }
        if let Some(h) = self.writer_task.lock().await.take() {
            h.abort();
        }
    }
}

fn map_jsonrpc_error(err: JsonRpcErrorObject) -> ClientError {
    if err.code == agentcpm_pd_protocol::errors::ERR_APPLICATION
        && let Some(data) = err.data
        && let Some(contract) = crate::error::contract_error_from_rpc_data(&data)
    {
        return ClientError::Contract(contract);
    }
    ClientError::ProtocolError {
        code: err.code,
        message: err.message,
    }
}

async fn fail_all_pending(router: &Arc<Mutex<ClientRouterState>>, reason: &str) {
    let mut guard = router.lock().await;
    if guard.closed_reason.is_none() {
        guard.closed_reason = Some(reason.to_owned());
    }
    for (_, tx) in guard.pending.drain() {
        let _ = tx.send(RawRpcOutcome::Disconnected(reason.to_owned()));
    }
}

async fn dispatch_incoming_frame(router: &Arc<Mutex<ClientRouterState>>, val: Value) {
    if val.get("id").is_some() && val.get("method").is_none() {
        if let Ok(resp) = serde_json::from_value::<JsonRpcResponse>(val)
            && let Some(id) = resp.id
        {
            let sender = router.lock().await.pending.remove(&id);
            if let Some(tx) = sender {
                let outcome = match (resp.result, resp.error) {
                    (Some(r), None) => RawRpcOutcome::Result(r),
                    (_, Some(e)) => RawRpcOutcome::Error(e),
                    _ => RawRpcOutcome::Error(JsonRpcErrorObject {
                        code: -32603,
                        message: "malformed empty JSON-RPC response".to_owned(),
                        data: None,
                    }),
                };
                let _ = tx.send(outcome);
            }
        }
        return;
    }

    if let Ok(notif) = serde_json::from_value::<JsonRpcNotification>(val) {
        match notif.method.as_str() {
            WIRE_SUB_EVENT => {
                if let Ok(ev) = serde_json::from_value::<SubEventNotification>(notif.params) {
                    let mut guard = router.lock().await;
                    match guard.subscriptions.get(&ev.subscription_id) {
                        Some(entry) => {
                            let _ = entry.tx.try_send(SubStreamItem::Event(ev));
                        }
                        None if !guard.ended_ids.contains(&ev.subscription_id) => {
                            // Registration race: park until open_subscription
                            // installs the entry (dropped only if it never
                            // arrives).
                            let id = ev.subscription_id.clone();
                            park_sub_item(&mut guard, &id, SubStreamItem::Event(ev));
                        }
                        // Late event for an already-ended subscription: the
                        // terminal frame was delivered; drop, never park.
                        None => {}
                    }
                }
            }
            WIRE_SUB_END => {
                if let Ok(end) = serde_json::from_value::<SubEndNotification>(notif.params) {
                    let mut guard = router.lock().await;
                    match remove_subscription_entry(&mut guard, &end.subscription_id) {
                        // Terminal frame: deliver it, drop the registry entry
                        // (bounded memory), and tombstone the id so late
                        // frames are dropped instead of parked.
                        Some(entry) => {
                            let _ = entry.tx.try_send(SubStreamItem::End(end));
                        }
                        // Registration race: park until open_subscription
                        // installs the entry (wp3fix3 review P2 — same rule
                        // as the event branch below).
                        None if !guard.ended_ids.contains(&end.subscription_id) => {
                            let id = end.subscription_id.clone();
                            park_sub_item(&mut guard, &id, SubStreamItem::End(end));
                        }
                        // Late/duplicate terminal for an already-ended
                        // subscription: dropped, never parked — the id will
                        // never be opened again, so parking it would leak
                        // the shared budget and could evict another
                        // subscription's registration-window frames.
                        None => {}
                    }
                }
            }
            WIRE_CONN_HEARTBEAT
                if serde_json::from_value::<ConnHeartbeatNotification>(notif.params).is_ok() =>
            {
                router.lock().await.last_heartbeat_at = Some(Instant::now());
            }
            _ => {}
        }
    }
}

pub fn unwrap_rpc_outcome(resp: &OperationResponse) -> Value {
    match resp.outcome {
        ResponseOutcome::Succeeded | ResponseOutcome::Accepted => {
            resp.result.clone().unwrap_or(Value::Null)
        }
        ResponseOutcome::Failed => Value::Null,
    }
}

#[cfg(test)]
mod parked_tests {
    use super::*;
    use agentcpm_pd_protocol::wire::SubTerminal;

    fn event_item_for(id: &str, cursor: &str) -> SubStreamItem {
        SubStreamItem::Event(SubEventNotification {
            subscription_id: id.to_owned(),
            event: agentcpm_pd_protocol::contract::EventEnvelope {
                version: "agentcpm-pd.event/1".to_owned(),
                instance_id: "inst".to_owned(),
                run_id: None,
                operation_id: None,
                subscription_id: None,
                cursor: cursor.to_owned(),
                event_type: "run.progress".to_owned(),
                data: serde_json::Map::new(),
            },
        })
    }

    fn event_item(cursor: &str) -> SubStreamItem {
        event_item_for("sub-x", cursor)
    }

    fn end_item() -> SubStreamItem {
        SubStreamItem::End(SubEndNotification {
            subscription_id: "sub-x".to_owned(),
            terminal: SubTerminal::Completed,
            resume_cursor: None,
            gap: None,
            error: None,
        })
    }

    #[test]
    fn parked_eviction_is_fifo_and_budgeted() {
        let mut state = ClientRouterState::default();
        park_sub_item(&mut state, "a", event_item("cur-1"));
        park_sub_item(&mut state, "b", event_item("cur-2"));
        park_sub_item(&mut state, "a", event_item("cur-3"));
        assert_eq!(state.parked_sub_items_total, 3);
        assert_eq!(state.parked_order, vec!["a".to_owned(), "b".to_owned()]);

        // Force byte-budget overflow with items that each fit the budget:
        // two ~2 MiB events together exceed the 4 MiB total, so the oldest
        // entries (a, b) must be evicted first.
        let big = |cursor: &str| {
            let mut item = event_item(cursor);
            if let SubStreamItem::Event(ev) = &mut item {
                ev.event.data.insert(
                    "blob".to_owned(),
                    Value::String("x".repeat(2 * 1024 * 1024)),
                );
            }
            item
        };
        park_sub_item(&mut state, "c", big("huge-c"));
        assert!(
            state.parked_sub_bytes > 2 * 1024 * 1024,
            "large item must be charged near its real footprint"
        );
        park_sub_item(&mut state, "d", big("huge-d"));
        assert!(
            state.parked_sub_bytes <= PARKED_SUB_BYTES_TOTAL,
            "byte budget must hold: {}",
            state.parked_sub_bytes
        );
        // FIFO: "a" (oldest insertion) lost its first item or vanished entirely.
        let a_len = state
            .parked_sub_items
            .get("a")
            .map(VecDeque::len)
            .unwrap_or(0);
        assert!(a_len < 2, "oldest subscription must be evicted first");
        assert_eq!(
            state.parked_sub_items_total,
            state
                .parked_sub_items
                .values()
                .map(VecDeque::len)
                .sum::<usize>()
        );
    }

    #[tokio::test]
    async fn ended_subscription_late_events_are_dropped_not_parked() {
        let (tx, mut rx) = mpsc::channel(SUBSCRIPTION_BUFFER_ITEMS);
        let router: Arc<Mutex<ClientRouterState>> = Arc::new(Mutex::new(ClientRouterState {
            subscriptions: HashMap::from([("sub-x".to_owned(), SubRegistryEntry { tx })]),
            ..ClientRouterState::default()
        }));

        // Production path: WIRE_SUB_END delivers, removes the entry, and
        // tombstones the id.
        dispatch_incoming_frame(
            &router,
            serde_json::to_value(JsonRpcNotification {
                jsonrpc: "2.0".to_owned(),
                method: WIRE_SUB_END.to_owned(),
                params: serde_json::to_value(SubEndNotification {
                    subscription_id: "sub-x".to_owned(),
                    terminal: SubTerminal::Completed,
                    resume_cursor: None,
                    gap: None,
                    error: None,
                })
                .unwrap(),
            })
            .unwrap(),
        )
        .await;
        match rx.recv().await {
            Some(SubStreamItem::End(end)) => assert_eq!(end.terminal, SubTerminal::Completed),
            other => panic!("expected End, got {other:?}"),
        }

        // A late event for the now-ended id must be DROPPED, not parked.
        dispatch_incoming_frame(
            &router,
            serde_json::to_value(JsonRpcNotification {
                jsonrpc: "2.0".to_owned(),
                method: WIRE_SUB_EVENT.to_owned(),
                params: serde_json::to_value(SubEventNotification {
                    subscription_id: "sub-x".to_owned(),
                    event: agentcpm_pd_protocol::contract::EventEnvelope {
                        version: "agentcpm-pd.event/1".to_owned(),
                        instance_id: "inst".to_owned(),
                        run_id: None,
                        operation_id: None,
                        subscription_id: None,
                        cursor: "late".to_owned(),
                        event_type: "run.progress".to_owned(),
                        data: serde_json::Map::new(),
                    },
                })
                .unwrap(),
            })
            .unwrap(),
        )
        .await;

        let guard = router.lock().await;
        assert!(
            !guard.parked_sub_items.contains_key("sub-x"),
            "late events for an ended subscription must be dropped, not parked"
        );
        assert!(
            guard.ended_ids.contains("sub-x"),
            "the ended id must be tombstoned"
        );
        drop(guard);
        assert!(rx.try_recv().is_err(), "nothing may be delivered after end");
    }

    /// wp3fix3 review P2 (both review channels): a late or duplicate
    /// `sub.end` for an already tombstoned subscription must be dropped, not
    /// parked — the id will never be opened again, so parking it leaks the
    /// shared budget and can evict another subscription's registration-window
    /// frames.
    #[tokio::test]
    async fn late_sub_end_after_tombstone_is_dropped_not_parked() {
        let (tx, mut rx) = mpsc::channel(SUBSCRIPTION_BUFFER_ITEMS);
        let router: Arc<Mutex<ClientRouterState>> = Arc::new(Mutex::new(ClientRouterState {
            subscriptions: HashMap::from([("sub-x".to_owned(), SubRegistryEntry { tx })]),
            ..ClientRouterState::default()
        }));

        let terminal = |id: &str| {
            serde_json::to_value(JsonRpcNotification {
                jsonrpc: "2.0".to_owned(),
                method: WIRE_SUB_END.to_owned(),
                params: serde_json::to_value(SubEndNotification {
                    subscription_id: id.to_owned(),
                    terminal: SubTerminal::Closed,
                    resume_cursor: Some("cur-1".to_owned()),
                    gap: None,
                    error: None,
                })
                .unwrap(),
            })
            .unwrap()
        };

        // First sub.end delivers, removes the entry, tombstones the id.
        dispatch_incoming_frame(&router, terminal("sub-x")).await;
        assert!(matches!(rx.recv().await, Some(SubStreamItem::End(_))));

        // A duplicate/late sub.end for the same id: dropped, never parked.
        dispatch_incoming_frame(&router, terminal("sub-x")).await;

        let guard = router.lock().await;
        assert!(
            !guard.parked_sub_items.contains_key("sub-x"),
            "a late sub.end must not be parked for a tombstoned id"
        );
        assert_eq!(
            guard.parked_sub_items_total, 0,
            "the shared parked budget must stay untouched"
        );
        assert!(guard.ended_ids.contains("sub-x"));
        drop(guard);
        assert!(
            rx.try_recv().is_err(),
            "no duplicate terminal may be delivered"
        );
    }

    #[tokio::test]
    async fn sub_end_delivers_and_removes_the_registry_entry() {
        let (tx, mut rx) = mpsc::channel(SUBSCRIPTION_BUFFER_ITEMS);
        let router: Arc<Mutex<ClientRouterState>> = Arc::new(Mutex::new(ClientRouterState {
            subscriptions: HashMap::from([("sub-x".to_owned(), SubRegistryEntry { tx })]),
            ..ClientRouterState::default()
        }));

        dispatch_incoming_frame(
            &router,
            serde_json::to_value(JsonRpcNotification {
                jsonrpc: "2.0".to_owned(),
                method: WIRE_SUB_END.to_owned(),
                params: serde_json::to_value(SubEndNotification {
                    subscription_id: "sub-x".to_owned(),
                    terminal: SubTerminal::Completed,
                    resume_cursor: Some("cur-9".to_owned()),
                    gap: None,
                    error: None,
                })
                .unwrap(),
            })
            .unwrap(),
        )
        .await;

        match rx.recv().await {
            Some(SubStreamItem::End(end)) => {
                assert_eq!(end.terminal, SubTerminal::Completed);
            }
            other => panic!("expected End, got {other:?}"),
        }
        assert!(
            !router.lock().await.subscriptions.contains_key("sub-x"),
            "terminal frame must remove the registry entry (bounded memory)"
        );
        assert!(
            router.lock().await.ended_ids.contains("sub-x"),
            "terminal frame must tombstone the ended id"
        );
    }

    #[test]
    fn parked_order_never_duplicates_an_id_under_eviction() {
        let mut state = ClientRouterState::default();
        park_sub_item(&mut state, "a", event_item("c1"));
        // One eviction round pops the item but leaves the (now empty) key
        // and its single order entry behind — exactly the state a later park
        // for the same id hits.
        assert!(drop_oldest_parked(&mut state));
        assert!(state.parked_sub_items.contains_key("a"));
        assert_eq!(state.parked_order.iter().filter(|id| *id == "a").count(), 1);

        // A later park for the same id must NOT append a second order entry
        // (the old is_empty-based push did exactly that).
        park_sub_item(&mut state, "a", event_item("c2"));
        let occurrences = state.parked_order.iter().filter(|id| *id == "a").count();
        assert_eq!(
            occurrences, 1,
            "parked_order must hold each id at most once"
        );

        // Driving eviction to completion must be panic-free and consistent.
        while drop_oldest_parked(&mut state) {}
        assert!(state.parked_order.is_empty());
        assert!(state.parked_sub_items.is_empty());
        assert_eq!(state.parked_sub_items_total, 0);
        assert_eq!(state.parked_sub_bytes, 0);
    }

    #[test]
    fn cursor_expired_minimal_rpc_data_maps_to_its_contract_code() {
        use crate::error::contract_error_from_rpc_data;
        use agentcpm_pd_protocol::errors::CODE_CURSOR_EXPIRED;

        // Minimal payload (no retry/effect) as used by sub.open rejections.
        let minimal = serde_json::json!({
            "code": "CURSOR_EXPIRED",
            "message": "requested cursor expired",
            "details": {"gap": {"kind": "retention", "recovery": "fetch_snapshot_then_resume"}}
        });
        let contract =
            contract_error_from_rpc_data(&minimal).expect("minimal payload must parse leniently");
        assert_eq!(contract.code, CODE_CURSOR_EXPIRED);
        assert!(
            contract
                .details
                .as_ref()
                .is_some_and(|d| d.contains_key("gap"))
        );

        // Through the RPC mapper the code must stay CURSOR_EXPIRED, not fall
        // through to UNAVAILABLE.
        let mapped = map_jsonrpc_error(agentcpm_pd_protocol::wire::JsonRpcErrorObject {
            code: agentcpm_pd_protocol::errors::ERR_APPLICATION,
            message: "cursor expired".to_owned(),
            data: Some(minimal),
        });
        assert_eq!(mapped.into_contract_error().code, CODE_CURSOR_EXPIRED);

        // Full contract payloads keep working.
        let full = serde_json::json!({
            "code": "CURSOR_EXPIRED",
            "message": "m",
            "retry": {"mode": "never"},
            "effect": "not_applied"
        });
        assert!(contract_error_from_rpc_data(&full).is_some());
    }

    #[test]
    fn oversized_sub_end_is_charged_at_its_true_footprint() {
        use agentcpm_pd_protocol::errors::{ContractError, EffectStatus, RetrySpec};
        let mut state = ClientRouterState::default();
        let end = SubEndNotification {
            subscription_id: "sub-big".to_owned(),
            terminal: SubTerminal::Error,
            resume_cursor: Some("c".repeat(400 * 1024)),
            gap: None,
            error: Some(ContractError {
                code: "X".to_owned(),
                message: "m".repeat(400 * 1024),
                details: None,
                retry: RetrySpec::never(),
                effect: EffectStatus::NotApplied,
            }),
        };
        park_sub_item(&mut state, "sub-big", SubStreamItem::End(end));
        assert!(
            state.parked_sub_bytes > 800 * 1024,
            "a ~800 KiB sub.end must be charged near its real footprint, got {}",
            state.parked_sub_bytes
        );
        assert_eq!(state.parked_sub_items_total, 1);
    }

    #[test]
    fn single_item_beyond_the_whole_budget_is_rejected_before_parking() {
        use agentcpm_pd_protocol::errors::{ContractError, EffectStatus, RetrySpec};
        let mut state = ClientRouterState::default();
        let id = "s".repeat(1_600 * 1024);
        let end = SubEndNotification {
            subscription_id: id.clone(),
            terminal: SubTerminal::Error,
            resume_cursor: Some("c".repeat(1_600 * 1024)),
            gap: None,
            error: Some(ContractError {
                code: "X".to_owned(),
                message: "m".repeat(1_600 * 1024),
                details: None,
                retry: RetrySpec::never(),
                effect: EffectStatus::NotApplied,
            }),
        };
        park_sub_item(&mut state, &id, SubStreamItem::End(end));
        assert!(
            !state.parked_sub_items.contains_key(&id),
            "an item larger than the entire byte budget must not be parked"
        );
        assert_eq!(state.parked_sub_items_total, 0);
        assert_eq!(state.parked_sub_bytes, 0);
        assert!(state.parked_order.is_empty());
    }

    #[test]
    fn long_subscription_id_is_part_of_the_estimate() {
        let mut state = ClientRouterState::default();
        let id = "s".repeat(400 * 1024);
        park_sub_item(&mut state, &id, event_item_for(&id, "cur-1"));
        assert!(
            state.parked_sub_bytes > 400 * 1024,
            "the map-key copy of the subscription id must be charged: {}",
            state.parked_sub_bytes
        );
    }

    #[test]
    fn dangling_parked_order_entry_does_not_panic() {
        // Hand-constructed inconsistent state (defensive fallback): an order
        // entry whose key is absent from the map must be skipped, never panic
        // the reader task.
        let mut state = ClientRouterState::default();
        state.parked_order.push_back("ghost".to_owned());
        assert!(!drop_oldest_parked(&mut state));
        assert!(state.parked_order.is_empty());
    }

    #[test]
    fn ended_tombstones_are_bounded_fifo() {
        let mut state = ClientRouterState::default();
        for i in 0..(ENDED_TOMBSTONE_CAP + 64) {
            mark_ended(&mut state, &format!("sub-{i}"));
        }
        assert_eq!(state.ended_ids.len(), ENDED_TOMBSTONE_CAP);
        assert_eq!(state.ended_order.len(), ENDED_TOMBSTONE_CAP);
        assert!(!state.ended_ids.contains("sub-0"));
        assert!(
            state
                .ended_ids
                .contains(&format!("sub-{}", ENDED_TOMBSTONE_CAP + 63))
        );
    }

    #[test]
    fn parked_helpers_agree_on_counters() {
        let mut state = ClientRouterState::default();
        park_sub_item(&mut state, "a", event_item("c1"));
        park_sub_item(&mut state, "a", end_item());
        assert_eq!(state.parked_sub_items_total, 2);
        // Flush semantics used by open_subscription:
        let parked = state.parked_sub_items.remove("a").unwrap();
        state.parked_order.retain(|id| id != "a");
        state.parked_sub_items_total = state.parked_sub_items_total.saturating_sub(parked.len());
        state.parked_sub_bytes = state.parked_sub_bytes.saturating_sub(
            parked
                .iter()
                .map(|item| estimate_parked_item_bytes("a", item))
                .sum(),
        );
        assert_eq!(state.parked_sub_items_total, 0);
        assert_eq!(state.parked_sub_bytes, 0);
        assert!(state.parked_order.is_empty());
    }
}

#[cfg(test)]
mod flush_tests {
    use super::*;
    use agentcpm_pd_protocol::wire::SubTerminal;

    fn end_item_for(id: &str) -> SubStreamItem {
        SubStreamItem::End(SubEndNotification {
            subscription_id: id.to_owned(),
            terminal: SubTerminal::Completed,
            resume_cursor: Some("cur-9".to_owned()),
            gap: None,
            error: None,
        })
    }

    fn event_item_for(id: &str, cursor: &str) -> SubStreamItem {
        SubStreamItem::Event(SubEventNotification {
            subscription_id: id.to_owned(),
            event: agentcpm_pd_protocol::contract::EventEnvelope {
                version: "agentcpm-pd.event/1".to_owned(),
                instance_id: "inst".to_owned(),
                run_id: None,
                operation_id: None,
                subscription_id: None,
                cursor: cursor.to_owned(),
                event_type: "run.progress".to_owned(),
                data: serde_json::Map::new(),
            },
        })
    }

    /// Review round-3 P1-B, deterministic: a terminal frame parked in the
    /// registration window is flushed in order, then the freshly registered
    /// entry is removed and the id tombstoned.
    #[tokio::test]
    async fn flush_with_parked_end_removes_entry_and_tombstones() {
        let (tx, mut rx) = mpsc::channel(SUBSCRIPTION_BUFFER_ITEMS);
        let mut state = ClientRouterState::default();
        // Simulate the registration window: an event and a terminal frame
        // arrive before the entry is installed.
        park_sub_item(&mut state, "sub-w", event_item_for("sub-w", "cur-1"));
        park_sub_item(&mut state, "sub-w", end_item_for("sub-w"));
        // The entry is installed right before the flush (as open_subscription does).
        state
            .subscriptions
            .insert("sub-w".to_owned(), SubRegistryEntry { tx: tx.clone() });

        flush_parked_sub_items(&mut state, "sub-w", &tx);

        // Arrival order preserved: event, then terminal.
        match rx.recv().await {
            Some(SubStreamItem::Event(ev)) => assert_eq!(ev.event.cursor, "cur-1"),
            other => panic!("expected the flushed event, got {other:?}"),
        }
        match rx.recv().await {
            Some(SubStreamItem::End(end)) => {
                assert_eq!(end.terminal, SubTerminal::Completed);
                assert_eq!(end.resume_cursor.as_deref(), Some("cur-9"));
            }
            other => panic!("expected the flushed terminal, got {other:?}"),
        }

        // The entry must be gone and the id tombstoned.
        assert!(!state.subscriptions.contains_key("sub-w"));
        assert!(state.ended_ids.contains("sub-w"));
        assert!(state.parked_sub_items.is_empty());
        assert!(state.parked_order.is_empty());
        assert_eq!(state.parked_sub_items_total, 0);
        assert_eq!(state.parked_sub_bytes, 0);
        // No third frame: the channel closes once the entry and the local
        // sender are dropped.
        drop(tx);
        assert!(rx.recv().await.is_none());
    }

    /// Without a parked terminal the entry stays registered (normal path).
    #[tokio::test]
    async fn flush_without_end_keeps_the_entry() {
        let (tx, mut rx) = mpsc::channel(SUBSCRIPTION_BUFFER_ITEMS);
        let mut state = ClientRouterState::default();
        park_sub_item(&mut state, "sub-k", event_item_for("sub-k", "cur-1"));
        state
            .subscriptions
            .insert("sub-k".to_owned(), SubRegistryEntry { tx: tx.clone() });

        flush_parked_sub_items(&mut state, "sub-k", &tx);

        assert!(state.subscriptions.contains_key("sub-k"));
        assert!(!state.ended_ids.contains("sub-k"));
        assert!(matches!(rx.recv().await, Some(SubStreamItem::Event(_))));
        drop(tx);
    }

    /// wp3fix3 review P2: frames parked after a terminal frame are discarded —
    /// the direct (registered) path drops post-`sub.end` events via the
    /// tombstone, and the flush path must not deliver them after the
    /// terminal, where consumers would never read them.
    #[tokio::test]
    async fn flush_discards_frames_after_the_first_terminal() {
        let (tx, mut rx) = mpsc::channel(SUBSCRIPTION_BUFFER_ITEMS);
        let mut state = ClientRouterState::default();
        park_sub_item(&mut state, "sub-t", event_item_for("sub-t", "cur-1"));
        park_sub_item(&mut state, "sub-t", end_item_for("sub-t"));
        park_sub_item(&mut state, "sub-t", event_item_for("sub-t", "cur-late"));
        park_sub_item(&mut state, "sub-t", end_item_for("sub-t"));
        state
            .subscriptions
            .insert("sub-t".to_owned(), SubRegistryEntry { tx: tx.clone() });

        flush_parked_sub_items(&mut state, "sub-t", &tx);

        match rx.recv().await {
            Some(SubStreamItem::Event(ev)) => assert_eq!(ev.event.cursor, "cur-1"),
            other => panic!("expected the pre-terminal event, got {other:?}"),
        }
        match rx.recv().await {
            Some(SubStreamItem::End(end)) => assert_eq!(end.terminal, SubTerminal::Completed),
            other => panic!("expected the first terminal, got {other:?}"),
        }
        // Nothing after the first terminal, ever.
        drop(tx);
        assert!(
            rx.recv().await.is_none(),
            "no frame may be delivered after the terminal"
        );
        assert!(!state.subscriptions.contains_key("sub-t"));
        assert!(state.ended_ids.contains("sub-t"));
        assert_eq!(state.parked_sub_items_total, 0);
        assert_eq!(state.parked_sub_bytes, 0);
    }
}
