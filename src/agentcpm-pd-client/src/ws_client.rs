//! Multiplexed `pd-connect/1` over `/v1/ws` client state machine (`007 §6, WP2`).

use std::collections::HashMap;
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
    ended: bool,
}

#[derive(Default)]
struct ClientRouterState {
    pending: HashMap<String, oneshot::Sender<RawRpcOutcome>>,
    subscriptions: HashMap<String, SubRegistryEntry>,
    last_heartbeat_at: Option<Instant>,
    closed_reason: Option<String>,
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
        let init_id = client.id_gen.next_id();
        let raw_val = match client
            .send_raw_rpc_inner(
                init_id,
                WIRE_INITIALIZE,
                params_val,
                Duration::from_secs(10),
                false,
            )
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
            .send_raw_rpc_inner(
                request_id.clone(),
                &rpc.method,
                rpc.params,
                timeout,
                is_write,
            )
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
    pub async fn send_raw_rpc(
        &self,
        request_id: String,
        method: &str,
        params: Value,
        timeout: Duration,
        treat_disconnect_as_result_unknown: bool,
    ) -> Result<Value, ClientError> {
        self.send_raw_rpc_inner(
            request_id,
            method,
            params,
            timeout,
            treat_disconnect_as_result_unknown,
        )
        .await
    }

    async fn send_raw_rpc_inner(
        &self,
        request_id: String,
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
        let req_id = self.id_gen.next_id();
        let raw = self
            .send_raw_rpc_inner(
                req_id,
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
        self.router.lock().await.subscriptions.insert(
            opened.subscription_id.clone(),
            SubRegistryEntry {
                tx: sub_tx,
                ended: false,
            },
        );

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
        let req_id = self.id_gen.next_id();
        let params = serde_json::to_value(SubCreditParams {
            subscription_id: subscription_id.to_owned(),
            add,
        })
        .unwrap_or(Value::Null);
        self.send_raw_rpc_inner(
            req_id,
            WIRE_SUB_CREDIT,
            params,
            Duration::from_secs(10),
            false,
        )
        .await
        .map(|_| ())
    }

    pub async fn send_sub_close(&self, subscription_id: &str) -> Result<(), ClientError> {
        let req_id = self.id_gen.next_id();
        let params = serde_json::to_value(SubCloseParams {
            subscription_id: subscription_id.to_owned(),
        })
        .unwrap_or(Value::Null);
        let res = self
            .send_raw_rpc_inner(
                req_id,
                WIRE_SUB_CLOSE,
                params,
                Duration::from_secs(10),
                false,
            )
            .await;
        self.router
            .lock()
            .await
            .subscriptions
            .remove(subscription_id);
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
        && let Ok(contract) = serde_json::from_value::<ContractError>(data)
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
                    let tx = {
                        let guard = router.lock().await;
                        guard
                            .subscriptions
                            .get(&ev.subscription_id)
                            .filter(|s| !s.ended)
                            .map(|s| s.tx.clone())
                    };
                    if let Some(tx) = tx {
                        let _ = tx.try_send(SubStreamItem::Event(ev));
                    }
                }
            }
            WIRE_SUB_END => {
                if let Ok(end) = serde_json::from_value::<SubEndNotification>(notif.params) {
                    let tx = {
                        let mut guard = router.lock().await;
                        guard
                            .subscriptions
                            .get_mut(&end.subscription_id)
                            .map(|entry| {
                                entry.ended = true;
                                entry.tx.clone()
                            })
                    };
                    if let Some(tx) = tx {
                        let _ = tx.try_send(SubStreamItem::End(end));
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
