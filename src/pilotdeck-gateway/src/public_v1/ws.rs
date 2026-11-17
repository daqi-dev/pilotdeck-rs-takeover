//! `pd-connect/1` WebSocket transport session over `/v1/ws` (`007 §6, WP4`).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use agentcpm_pd_protocol::contract::{EventEnvelope, OPERATION_VERSION};
use agentcpm_pd_protocol::envelope::is_control_method;
use agentcpm_pd_protocol::errors::{
    CODE_CURSOR_EXPIRED, CODE_INVALID_ARGUMENT, CODE_RESOURCE_LIMIT, CODE_UNAUTHENTICATED,
    ContractError, ERR_NOT_INITIALIZED, ERR_UNSUPPORTED_VERSION, EffectStatus,
    JSONRPC_INVALID_PARAMS, JSONRPC_INVALID_REQUEST, JSONRPC_PARSE_ERROR, RetrySpec,
};
use agentcpm_pd_protocol::registry::OperationRegistry;
use agentcpm_pd_protocol::wire::{
    ABI_VERSION, ConnAbandonParams, ConnHeartbeatNotification, DEFAULT_CREDIT,
    DEFAULT_HEARTBEAT_MS, InitializeParams, InitializeResult, JSONRPC_VERSION, JsonRpcNotification,
    JsonRpcRequest, JsonRpcResponse, MAX_CONTROL_PENDING, MAX_FRAME_BYTES, MAX_PENDING,
    MAX_SUBSCRIPTIONS, OperationRpcParams, OperationRpcResult, ProtocolLimits,
    SUBSCRIPTION_BUFFER_BYTES, SUBSCRIPTION_BUFFER_ITEMS, SubCloseParams, SubCreditParams,
    SubEndNotification, SubEventNotification, SubOpenParams, SubOpenResult, SubTerminal,
    TargetHandshakeInfo, WIRE_CONN_ABANDON, WIRE_CONN_HEARTBEAT, WIRE_INITIALIZE, WIRE_SUB_CLOSE,
    WIRE_SUB_CREDIT, WIRE_SUB_END, WIRE_SUB_EVENT, WIRE_SUB_OPEN, validate_request_id,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify, Semaphore, mpsc, watch};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use crate::public_v1::PublicV1State;
use crate::public_v1::runtime::{DispatchError, capability_unavailable_rpc_error};
use crate::public_v1::store::{StoredEvent, now_rfc3339};

struct ActiveSubscription {
    credit: Arc<AtomicU64>,
    credit_notify: Arc<Notify>,
    cancel_tx: watch::Sender<bool>,
}

/// P1 (wp4 review): sends the connection-stop signal on any exit — including
/// a task abort (drain deadlines abort the connection task, dropping locals
/// without running the normal close path). Without this, subscription pumps
/// observing a dropped `conn_stop` sender used to busy-loop on the failed
/// `changed()` poll while hammering the store mutex.
struct ConnStopOnDrop(watch::Sender<bool>);
impl Drop for ConnStopOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}

/// Inbound WS size bound for `/v1/ws` (`MAX_FRAME_BYTES` plus a small margin
/// so marginally oversized frames still reach the JSON-RPC `-32600` reply
/// instead of a library-level protocol close). Without an explicit config,
/// tungstenite's 64 MiB/16 MiB defaults allocate before our 1 MiB check can
/// reject.
pub fn v1_ws_config() -> tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
    use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
    let cap = MAX_FRAME_BYTES + 8 * 1024;
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(cap);
    config.max_frame_size = Some(cap);
    config
}

/// Runs a single authenticated `/v1/ws` `pd-connect/1` session to completion or server shutdown.
pub async fn serve_v1_ws<S>(
    ws_stream: WebSocketStream<S>,
    state: Arc<PublicV1State>,
    mut server_stop: watch::Receiver<bool>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut ws_sink, mut ws_source) = ws_stream.split();

    // Prioritized outbound lanes: Control (locally synthesized sub.end, close)
    // > Normal (incl. heartbeat) > Event stream (sub.event and the relayed
    // Completed sub.end, which shares the event lane so it can never overtake
    // the events of its own subscription; 007 §6.4).
    let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<Message>(128);
    let (norm_tx, mut norm_rx) = mpsc::channel::<Message>(256);
    let (sub_tx, mut sub_rx) = mpsc::channel::<Message>(256);

    let mut writer_handle = tokio::spawn(async move {
        loop {
            let msg = tokio::select! {
                biased;
                maybe_ctrl = ctrl_rx.recv() => match maybe_ctrl {
                    Some(m) => m,
                    None => break,
                },
                maybe_norm = norm_rx.recv() => match maybe_norm {
                    Some(m) => m,
                    None => break,
                },
                maybe_sub = sub_rx.recv() => match maybe_sub {
                    Some(m) => m,
                    None => break,
                },
            };
            let is_close = matches!(msg, Message::Close(_));
            if ws_sink.send(msg).await.is_err() {
                break;
            }
            if is_close {
                break;
            }
        }
    });

    let initialized = Arc::new(AtomicBool::new(false));
    let control_sem = Arc::new(Semaphore::new(MAX_CONTROL_PENDING));
    let normal_sem = Arc::new(Semaphore::new(MAX_PENDING));
    let subscriptions: Arc<Mutex<HashMap<String, ActiveSubscription>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let conn_id = state.runtime.store().next_id();

    let (conn_stop_tx, conn_stop_rx) = watch::channel(false);
    // P1 (wp4 review): owns the sender so every exit path — including a task
    // abort — signals the pumps before the watch closes.
    let conn_stop_guard = ConnStopOnDrop(conn_stop_tx);

    // Periodic server heartbeat (`5s`).
    let norm_hb = norm_tx.clone();
    let mut hb_stop = conn_stop_rx.clone();
    let hb_handle = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(DEFAULT_HEARTBEAT_MS));
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let ts_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    let notif = JsonRpcNotification {
                        jsonrpc: JSONRPC_VERSION.to_owned(),
                        method: WIRE_CONN_HEARTBEAT.to_owned(),
                        params: serde_json::to_value(ConnHeartbeatNotification { ts_ms }).unwrap_or(Value::Null),
                    };
                    if let Ok(txt) = serde_json::to_string(&notif) {
                        let _ = norm_hb.try_send(Message::Text(txt.into()));
                    }
                }
                _ = hb_stop.changed() => break,
            }
        }
    });

    loop {
        tokio::select! {
            biased;
            _ = server_stop.changed() => {
                if *server_stop.borrow() {
                    let _ = conn_stop_guard.0.send(true);
                    let subs = {
                        let mut guard = subscriptions.lock().await;
                        guard.drain().collect::<Vec<_>>()
                    };
                    for (_, sub) in subs {
                        let _ = sub.cancel_tx.send(true);
                    }
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    let _ = ctrl_tx
                        .send(Message::Close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
                            code: CloseCode::Restart,
                            reason: "gateway shutting down".into(),
                        })))
                        .await;
                    break;
                }
            }
            frame = ws_source.next() => {
                match frame {
                    Some(Ok(Message::Text(txt))) => {
                        if txt.len() > MAX_FRAME_BYTES {
                            let resp = JsonRpcResponse::err(
                                None,
                                JSONRPC_INVALID_REQUEST,
                                format!("frame exceeds {MAX_FRAME_BYTES} byte limit"),
                                None,
                            );
                            send_json_frame(&ctrl_tx, &resp).await;
                            continue;
                        }
                        // P2 (wp4 review): frame handling is stop-preemptible —
                        // a handler awaiting a full outbound channel must not
                        // hide `server_stop` from this loop.
                        let handled = handle_ws_text_frame(
                            &txt,
                            &state,
                            &conn_id,
                            &initialized,
                            &control_sem,
                            &normal_sem,
                            &subscriptions,
                            &ctrl_tx,
                            &norm_tx,
                            &sub_tx,
                            &conn_stop_rx,
                        );
                        tokio::select! {
                            biased;
                            _ = server_stop.changed() => {}
                            _ = handled => {}
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }

    let _ = conn_stop_guard.0.send(true);
    let mut guard = subscriptions.lock().await;
    for (_, sub) in guard.drain() {
        let _ = sub.cancel_tx.send(true);
    }
    drop(guard);

    hb_handle.abort();
    drop(ctrl_tx);
    drop(norm_tx);
    drop(sub_tx);
    // P2 (wp4 review): a detached writer keeps the socket fd alive past the
    // connection permit's release; abort it when the drain budget expires.
    if tokio::time::timeout(Duration::from_millis(200), &mut writer_handle)
        .await
        .is_err()
    {
        writer_handle.abort();
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_ws_text_frame(
    raw: &str,
    state: &Arc<PublicV1State>,
    conn_id: &str,
    initialized: &Arc<AtomicBool>,
    control_sem: &Arc<Semaphore>,
    normal_sem: &Arc<Semaphore>,
    subscriptions: &Arc<Mutex<HashMap<String, ActiveSubscription>>>,
    ctrl_tx: &mpsc::Sender<Message>,
    norm_tx: &mpsc::Sender<Message>,
    sub_tx: &mpsc::Sender<Message>,
    conn_stop: &watch::Receiver<bool>,
) {
    let val: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            send_json_frame(
                ctrl_tx,
                &JsonRpcResponse::err(
                    None,
                    JSONRPC_PARSE_ERROR,
                    format!("invalid JSON: {e}"),
                    None,
                ),
            )
            .await;
            return;
        }
    };

    if val.get("id").is_none() || val.get("id").is_some_and(Value::is_null) {
        if let Ok(notif) = serde_json::from_value::<JsonRpcNotification>(val)
            && notif.method == WIRE_CONN_ABANDON
            && serde_json::from_value::<ConnAbandonParams>(notif.params).is_ok()
        {
            // Per D12/T11: conn.abandon MUST NOT cancel the underlying Gateway run.
        }
        return;
    }

    let req: JsonRpcRequest = match serde_json::from_value(val) {
        Ok(r) => r,
        Err(e) => {
            send_json_frame(
                ctrl_tx,
                &JsonRpcResponse::err(
                    None,
                    JSONRPC_INVALID_REQUEST,
                    format!("invalid JSON-RPC request frame: {e}"),
                    None,
                ),
            )
            .await;
            return;
        }
    };

    if req.jsonrpc != JSONRPC_VERSION {
        send_json_frame(
            ctrl_tx,
            &JsonRpcResponse::err(
                Some(req.id),
                JSONRPC_INVALID_REQUEST,
                "jsonrpc field must be `2.0`",
                None,
            ),
        )
        .await;
        return;
    }

    if let Err(msg) = validate_request_id(&req.id) {
        send_json_frame(
            ctrl_tx,
            &JsonRpcResponse::err(Some(req.id), JSONRPC_INVALID_REQUEST, msg, None),
        )
        .await;
        return;
    }

    if req.method == WIRE_INITIALIZE {
        let params: InitializeParams = match serde_json::from_value(req.params) {
            Ok(p) => p,
            Err(e) => {
                send_json_frame(
                    ctrl_tx,
                    &JsonRpcResponse::err(
                        Some(req.id),
                        JSONRPC_INVALID_PARAMS,
                        format!("invalid initialize params: {e}"),
                        None,
                    ),
                )
                .await;
                return;
            }
        };
        if params.abi_version != ABI_VERSION || params.api_version != OPERATION_VERSION {
            send_json_frame(
                ctrl_tx,
                &JsonRpcResponse::err(
                    Some(req.id),
                    ERR_UNSUPPORTED_VERSION,
                    format!(
                        "unsupported abiVersion={} apiVersion={}",
                        params.abi_version, params.api_version
                    ),
                    None,
                ),
            )
            .await;
            return;
        }
        if let Some(cred) = &params.credential
            && !state.verify_token_str(cred)
        {
            let err = ContractError::new(
                CODE_UNAUTHENTICATED,
                "invalid credential during initialize",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            );
            send_json_frame(ctrl_tx, &JsonRpcResponse::application_error(req.id, &err)).await;
            return;
        }

        initialized.store(true, Ordering::SeqCst);
        let meta = state.runtime.store().meta();
        let res = InitializeResult {
            abi_version: ABI_VERSION,
            supported: true,
            api_version: OPERATION_VERSION.to_owned(),
            supported_features: vec![
                "subscriptions".to_owned(),
                "control-priority".to_owned(),
                "idempotency".to_owned(),
            ],
            connection_id: conn_id.to_owned(),
            cli: None,
            target: TargetHandshakeInfo {
                name: "local".to_owned(),
                instance_id: meta.instance_id.clone(),
                node_id: meta.node_id.clone(),
                gateway_version: env!("CARGO_PKG_VERSION").to_owned(),
                schema_digest: OperationRegistry::global().registry_digest().to_owned(),
                transport: "ws".to_owned(),
            },
            limits: ProtocolLimits::default(),
        };
        send_json_frame(
            ctrl_tx,
            &JsonRpcResponse::ok(req.id, serde_json::to_value(res).unwrap_or(Value::Null)),
        )
        .await;
        return;
    }

    if !initialized.load(Ordering::SeqCst) {
        send_json_frame(
            ctrl_tx,
            &JsonRpcResponse::err(
                Some(req.id),
                ERR_NOT_INITIALIZED,
                "connection not initialized; send `initialize` first",
                None,
            ),
        )
        .await;
        return;
    }

    match req.method.as_str() {
        WIRE_SUB_OPEN => {
            handle_sub_open(req, state, subscriptions, ctrl_tx, sub_tx, conn_stop).await;
        }
        WIRE_SUB_CREDIT => {
            let params: SubCreditParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    send_json_frame(
                        ctrl_tx,
                        &JsonRpcResponse::err(
                            Some(req.id),
                            JSONRPC_INVALID_PARAMS,
                            format!("invalid sub.credit params: {e}"),
                            None,
                        ),
                    )
                    .await;
                    return;
                }
            };
            // P2 (wp4 review): clone the shared handles out and drop the map
            // guard before any await — holding it across the response send
            // deadlocked sub.open/sub.close and pump cleanup whenever the
            // control lane filled up.
            let entry = {
                let guard = subscriptions.lock().await;
                guard
                    .get(&params.subscription_id)
                    .map(|sub| (sub.credit.clone(), sub.credit_notify.clone()))
            };
            if let Some((credit, credit_notify)) = entry {
                credit.fetch_add(u64::from(params.add), Ordering::SeqCst);
                credit_notify.notify_one();
                send_json_frame(
                    ctrl_tx,
                    &JsonRpcResponse::ok(req.id, json!({"accepted": true})),
                )
                .await;
            } else {
                let err = ContractError::new(
                    agentcpm_pd_protocol::errors::CODE_NOT_FOUND,
                    format!("subscription `{}` not found", params.subscription_id),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                );
                send_json_frame(ctrl_tx, &JsonRpcResponse::application_error(req.id, &err)).await;
            }
        }
        WIRE_SUB_CLOSE => {
            let params: SubCloseParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    send_json_frame(
                        ctrl_tx,
                        &JsonRpcResponse::err(
                            Some(req.id),
                            JSONRPC_INVALID_PARAMS,
                            format!("invalid sub.close params: {e}"),
                            None,
                        ),
                    )
                    .await;
                    return;
                }
            };
            if let Some(sub) = subscriptions.lock().await.remove(&params.subscription_id) {
                let _ = sub.cancel_tx.send(true);
            }
            send_json_frame(
                ctrl_tx,
                &JsonRpcResponse::ok(req.id, json!({"closed": true})),
            )
            .await;
        }
        _ => {
            let is_ctrl = is_control_method(&req.method);
            let sem = if is_ctrl {
                control_sem.clone()
            } else {
                normal_sem.clone()
            };
            let out_tx = if is_ctrl {
                ctrl_tx.clone()
            } else {
                norm_tx.clone()
            };
            let Ok(permit) = sem.try_acquire_owned() else {
                let err = ContractError::new(
                    CODE_RESOURCE_LIMIT,
                    "too many concurrent in-flight requests on connection",
                    RetrySpec::backoff_read(Some(100)),
                    EffectStatus::NotApplied,
                );
                send_json_frame(&out_tx, &JsonRpcResponse::application_error(req.id, &err)).await;
                return;
            };
            let st = state.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let parsed: OperationRpcParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        let err = ContractError::new(
                            CODE_INVALID_ARGUMENT,
                            format!("invalid params envelope: {e}"),
                            RetrySpec::never(),
                            EffectStatus::NotApplied,
                        );
                        send_json_frame(&out_tx, &JsonRpcResponse::application_error(req.id, &err))
                            .await;
                        return;
                    }
                };

                let response = match st
                    .runtime
                    .dispatch(
                        &req.method,
                        parsed.input,
                        parsed.idempotency_key.as_deref(),
                        parsed.expected_revision.as_deref(),
                    )
                    .await
                {
                    Ok((outcome, result)) => {
                        let rpc_res = OperationRpcResult { outcome, result };
                        JsonRpcResponse::ok(
                            req.id,
                            serde_json::to_value(rpc_res).unwrap_or(Value::Null),
                        )
                    }
                    Err(DispatchError::Application(err)) => {
                        JsonRpcResponse::application_error(req.id, &err)
                    }
                    Err(DispatchError::Rpc {
                        code,
                        message,
                        data,
                    }) => JsonRpcResponse::err(Some(req.id), code, message, data),
                };
                send_json_frame(&out_tx, &response).await;
            });
        }
    }
}

async fn handle_sub_open(
    req: JsonRpcRequest,
    state: &Arc<PublicV1State>,
    subscriptions: &Arc<Mutex<HashMap<String, ActiveSubscription>>>,
    ctrl_tx: &mpsc::Sender<Message>,
    sub_tx: &mpsc::Sender<Message>,
    conn_stop: &watch::Receiver<bool>,
) {
    let mut params: SubOpenParams = match serde_json::from_value(req.params) {
        Ok(p) => p,
        Err(e) => {
            send_json_frame(
                ctrl_tx,
                &JsonRpcResponse::err(
                    Some(req.id),
                    JSONRPC_INVALID_PARAMS,
                    format!("invalid sub.open params: {e}"),
                    None,
                ),
            )
            .await;
            return;
        }
    };

    // P2 (wp4 review): stream admission follows the contract — the six
    // `execution='stream'` operations, of which only current-round ones are
    // served; everything else answers -32004 like before.
    let reg = OperationRegistry::global();
    let stream_admitted = reg.get(&params.stream).is_some_and(|spec| {
        spec.execution == agentcpm_pd_protocol::registry::ExecutionMode::Stream
            && spec.is_current_round
    });
    if !stream_admitted {
        let err = capability_unavailable_rpc_error(&params.stream);
        match err {
            DispatchError::Rpc {
                code,
                message,
                data,
            } => {
                send_json_frame(
                    ctrl_tx,
                    &JsonRpcResponse::err(Some(req.id), code, message, data),
                )
                .await;
            }
            DispatchError::Application(c) => {
                send_json_frame(ctrl_tx, &JsonRpcResponse::application_error(req.id, &c)).await;
            }
        }
        return;
    }

    if let Err(err) = agentcpm_pd_protocol::envelope::bind_instance_id(
        &mut params.input,
        &state.runtime.store().meta().instance_id,
        true,
    ) {
        send_json_frame(ctrl_tx, &JsonRpcResponse::application_error(req.id, &err)).await;
        return;
    }
    if let Err(errs) = reg.validate_input("run.events", &params.input) {
        let err = agentcpm_pd_protocol::registry::validation_errors_to_contract_error(errs);
        send_json_frame(ctrl_tx, &JsonRpcResponse::application_error(req.id, &err)).await;
        return;
    }

    // P2 (wp4 review): the stream contract forbids `page` on `sub.open`
    // (paged reads are the `run.events` RPC) and requires `follow=true`.
    if params.input.get("page").is_some() {
        let err = ContractError::new(
            CODE_INVALID_ARGUMENT,
            "sub.open rejects `page`; use the run.events operation for paged reads",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        );
        send_json_frame(ctrl_tx, &JsonRpcResponse::application_error(req.id, &err)).await;
        return;
    }
    if params.input.get("follow").and_then(Value::as_bool) == Some(false) {
        let err = ContractError::new(
            CODE_INVALID_ARGUMENT,
            "sub.open requires follow=true",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        );
        send_json_frame(ctrl_tx, &JsonRpcResponse::application_error(req.id, &err)).await;
        return;
    }

    if subscriptions.lock().await.len() >= MAX_SUBSCRIPTIONS {
        let err = ContractError::new(
            CODE_RESOURCE_LIMIT,
            format!("subscription count limit ({MAX_SUBSCRIPTIONS}) exceeded"),
            RetrySpec::backoff_read(Some(200)),
            EffectStatus::NotApplied,
        );
        send_json_frame(ctrl_tx, &JsonRpcResponse::application_error(req.id, &err)).await;
        return;
    }

    let run_id = params.input["runId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let after_cursor = params
        .input
        .get("afterCursor")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let initial_credit = u64::from(params.credit.unwrap_or(DEFAULT_CREDIT));

    let start_check = state
        .runtime
        .store()
        .check_run_subscription_start(&run_id, after_cursor.as_deref());

    let start_seq = match start_check {
        // P1 (wp4 review): a cursor that is already expired at open is
        // rejected with -32000 CURSOR_EXPIRED and `details.gap` carrying the
        // `$defs.gap` object — only mid-subscription gaps end via
        // `sub.end` (007 §6.4 / public-contract transportContract).
        Err(err) => {
            send_json_frame(ctrl_tx, &JsonRpcResponse::application_error(req.id, &err)).await;
            return;
        }
        Ok(Err(gap)) => {
            let mut details = serde_json::Map::new();
            details.insert(
                "gap".to_owned(),
                serde_json::to_value(&gap).unwrap_or(Value::Null),
            );
            let mut err = ContractError::new(
                CODE_CURSOR_EXPIRED,
                format!(
                    "afterCursor `{}` is older than the earliest retained event; recover via run.snapshot -> checkpoint.cursor -> sub.open",
                    after_cursor.as_deref().unwrap_or_default()
                ),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            );
            err.details = Some(details);
            send_json_frame(ctrl_tx, &JsonRpcResponse::application_error(req.id, &err)).await;
            return;
        }
        Ok(Ok(seq)) => seq,
    };

    let sub_id = state.runtime.store().next_id();
    let open_res = SubOpenResult {
        subscription_id: sub_id.clone(),
    };
    send_json_frame(
        ctrl_tx,
        &JsonRpcResponse::ok(
            req.id,
            serde_json::to_value(open_res).unwrap_or(Value::Null),
        ),
    )
    .await;

    let credit = Arc::new(AtomicU64::new(initial_credit));
    let credit_notify = Arc::new(Notify::new());
    let (cancel_tx, cancel_rx) = watch::channel(false);

    subscriptions.lock().await.insert(
        sub_id.clone(),
        ActiveSubscription {
            credit: credit.clone(),
            credit_notify: credit_notify.clone(),
            cancel_tx,
        },
    );

    tokio::spawn(run_subscription_pump(
        state.clone(),
        subscriptions.clone(),
        ctrl_tx.clone(),
        sub_tx.clone(),
        sub_id,
        run_id,
        start_seq,
        credit,
        credit_notify,
        cancel_rx,
        conn_stop.clone(),
    ));
}

/// The per-subscription event pump. Extracted so the dead-watch regression
/// test can drive it directly (`P1` wp4 review).
#[allow(clippy::too_many_arguments)]
async fn run_subscription_pump(
    pump_state: Arc<PublicV1State>,
    pump_subs: Arc<Mutex<HashMap<String, ActiveSubscription>>>,
    pump_ctrl: mpsc::Sender<Message>,
    pump_sub: mpsc::Sender<Message>,
    sub_id: String,
    run_id: String,
    start_seq: i64,
    credit: Arc<AtomicU64>,
    credit_notify: Arc<Notify>,
    mut cancel_rx: watch::Receiver<bool>,
    mut pump_conn_stop: watch::Receiver<bool>,
) {
    let mut last_fetched_seq = start_seq;
    let mut last_sent_cursor: Option<String> = None;
    let mut buffer: VecDeque<StoredEvent> = VecDeque::new();
    let mut buffer_bytes: usize = 0;
    let event_notify = pump_state.runtime.notify_all_events();
    let instance_id = pump_state.runtime.store().meta().instance_id.clone();

    loop {
        if *cancel_rx.borrow() || *pump_conn_stop.borrow() {
            let term = if *pump_conn_stop.borrow() {
                SubTerminal::Shutdown
            } else {
                SubTerminal::Closed
            };
            send_sub_end(&pump_ctrl, &sub_id, term, last_sent_cursor, None, None).await;
            break;
        }

        let notified = event_notify.notified();
        let terminal_reached;
        match pump_state
            .runtime
            .store()
            .fetch_run_events_after(&run_id, last_fetched_seq, 256)
        {
            Ok((events, is_term)) => {
                terminal_reached = is_term;
                for ev in events {
                    last_fetched_seq = ev.seq;
                    buffer_bytes = buffer_bytes.saturating_add(estimate_event_bytes(&ev));
                    buffer.push_back(ev);
                }
            }
            Err(err) => {
                send_sub_end(
                    &pump_ctrl,
                    &sub_id,
                    SubTerminal::Error,
                    last_sent_cursor,
                    None,
                    Some(err),
                )
                .await;
                break;
            }
        }

        // Deliver buffered events up to available credit first if consumer has credit.
        while credit.load(Ordering::SeqCst) > 0
            && let Some(ev) = buffer.front()
        {
            let ev_bytes = estimate_event_bytes(ev);
            let notif = JsonRpcNotification {
                jsonrpc: JSONRPC_VERSION.to_owned(),
                method: WIRE_SUB_EVENT.to_owned(),
                params: serde_json::to_value(SubEventNotification {
                    subscription_id: sub_id.clone(),
                    event: EventEnvelope {
                        version: "agentcpm-pd.event/1".to_owned(),
                        instance_id: instance_id.clone(),
                        run_id: Some(ev.resource_id.clone()),
                        operation_id: None,
                        subscription_id: Some(sub_id.clone()),
                        cursor: ev.cursor.clone(),
                        event_type: ev.event_type.clone(),
                        data: ev.data.clone(),
                    },
                })
                .unwrap_or(Value::Null),
            };
            let Ok(txt) = serde_json::to_string(&notif) else {
                let _ = buffer.pop_front();
                buffer_bytes = buffer_bytes.saturating_sub(ev_bytes);
                continue;
            };
            // P2 (wp4 review): a single serialized event can exceed the wire
            // frame budget; ending the subscription with an error (and the
            // last delivered cursor) beats sending a frame the peer would
            // have to drop.
            if txt.len() > MAX_FRAME_BYTES {
                let err = ContractError::new(
                    CODE_RESOURCE_LIMIT,
                    format!(
                        "event {} exceeds the {MAX_FRAME_BYTES} byte frame limit; resume from run.snapshot",
                        ev.cursor
                    ),
                    RetrySpec::backoff_read(Some(250)),
                    EffectStatus::NotApplied,
                );
                send_sub_end(
                    &pump_ctrl,
                    &sub_id,
                    SubTerminal::Error,
                    last_sent_cursor.clone(),
                    None,
                    Some(err),
                )
                .await;
                break;
            }
            match pump_sub.try_send(Message::Text(txt.into())) {
                Ok(()) => {
                    credit.fetch_sub(1, Ordering::SeqCst);
                    last_sent_cursor = Some(ev.cursor.clone());
                    let _ = buffer.pop_front();
                    buffer_bytes = buffer_bytes.saturating_sub(ev_bytes);
                }
                Err(mpsc::error::TrySendError::Full(_)) => break,
                Err(mpsc::error::TrySendError::Closed(_)) => return,
            }
        }

        // Slow consumer guard (T6): disconnect only slow subscriber once pending backlog crosses limit.
        if buffer.len() > SUBSCRIPTION_BUFFER_ITEMS || buffer_bytes > SUBSCRIPTION_BUFFER_BYTES {
            send_sub_end(
                &pump_ctrl,
                &sub_id,
                SubTerminal::SlowConsumer,
                last_sent_cursor,
                None,
                None,
            )
            .await;
            break;
        }

        if buffer.is_empty() && terminal_reached {
            // 007 §6.4 (`completed`: 事件全部送达): the Completed terminal
            // is relayed on the same event lane as this subscription's
            // `sub.event` frames. The writer's biased control priority
            // would otherwise let a ctrl-lane `sub.end` overtake events
            // already queued for this subscription, so consumers that
            // stop at `sub.end` (run events --follow, api connect
            // replay) would drop the tail (wire-order bug found by the
            // WP3 client work, 2026-11-17). Locally synthesized
            // terminals (slow consumer, close, shutdown, error, gap at
            // open) keep the ctrl lane so they can jump a saturated
            // event lane.
            send_sub_end(
                &pump_sub,
                &sub_id,
                SubTerminal::Completed,
                last_sent_cursor,
                None,
                None,
            )
            .await;
            break;
        }

        tokio::select! {
            _ = notified => {}
            _ = credit_notify.notified() => {}
            changed = cancel_rx.changed() => {
                if changed.is_err() {
                    // P1 (wp4 review): the subscription entry (and its cancel
                    // sender) was removed without signalling — a failed poll
                    // resolves immediately, so treating it as "keep looping"
                    // busy-spun on the store mutex.
                    break;
                }
            }
            changed = pump_conn_stop.changed() => {
                if changed.is_err() {
                    // P1 (wp4 review): the connection task was aborted without
                    // signalling stop — exit with a best-effort frame instead
                    // of busy-spinning.
                    send_sub_end(
                        &pump_ctrl,
                        &sub_id,
                        SubTerminal::Shutdown,
                        last_sent_cursor,
                        None,
                        None,
                    )
                    .await;
                    break;
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }

    pump_subs.lock().await.remove(&sub_id);
}

fn estimate_event_bytes(ev: &StoredEvent) -> usize {
    ev.cursor.len()
        + ev.event_type.len()
        + ev.resource_id.len()
        + serde_json::to_string(&ev.data).map_or(64, |s| s.len())
        + 64
}

/// Sends one `sub.end` notification on `tx`. Relay terminals that follow a
/// subscription's events (Completed) must use the subscription's event lane
/// so the writer cannot reorder them ahead of queued `sub.event` frames;
/// locally synthesized terminals may use the control lane.
async fn send_sub_end(
    tx: &mpsc::Sender<Message>,
    subscription_id: &str,
    terminal: SubTerminal,
    resume_cursor: Option<String>,
    gap: Option<agentcpm_pd_protocol::contract::GapFrame>,
    error: Option<ContractError>,
) {
    let notif = JsonRpcNotification {
        jsonrpc: JSONRPC_VERSION.to_owned(),
        method: WIRE_SUB_END.to_owned(),
        params: serde_json::to_value(SubEndNotification {
            subscription_id: subscription_id.to_owned(),
            terminal,
            resume_cursor,
            gap,
            error,
        })
        .unwrap_or(Value::Null),
    };
    if let Ok(txt) = serde_json::to_string(&notif) {
        let _ = tx.send(Message::Text(txt.into())).await;
    }
}

async fn send_json_frame(tx: &mpsc::Sender<Message>, frame: &JsonRpcResponse) {
    // P2 (wp4 review): outbound frames obey the same 1 MiB wire budget as
    // inbound ones. A response that cannot fit is degraded to a resource
    // limit error with the request's id instead of being silently truncated
    // by the peer.
    let mut txt = match serde_json::to_string(frame) {
        Ok(txt) => txt,
        Err(_) => return,
    };
    if txt.len() > MAX_FRAME_BYTES {
        let err = ContractError::new(
            CODE_RESOURCE_LIMIT,
            format!("response exceeds the {MAX_FRAME_BYTES} byte frame limit; narrow the request"),
            RetrySpec::backoff_read(Some(250)),
            EffectStatus::NotApplied,
        );
        let degraded =
            JsonRpcResponse::application_error(frame.id.clone().unwrap_or_default(), &err);
        txt = match serde_json::to_string(&degraded) {
            Ok(degraded_txt) => degraded_txt,
            Err(_) => return,
        };
    }
    let _ = tx.send(Message::Text(txt.into())).await;
}

pub fn server_started_at() -> String {
    now_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::runtime::PublicV1Runtime;
    use crate::public_v1::store::{InstanceMeta, RunTransition, SubmitDisposition, V1Store};
    use crate::public_v1::test_support::TempDir;
    use serde_json::json;

    /// P1 (wp4 review): a pump whose connection task is aborted without the
    /// stop signal (the sender dropped without `send(true)`) must exit and
    /// remove its subscription entry instead of busy-spinning on the failed
    /// `changed()` poll while hammering the store mutex.
    #[tokio::test]
    async fn subscription_pump_exits_when_the_connection_watch_sender_drops() {
        let tmp = TempDir::new("pump-dead-watch");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (file, _token) = load_local_instance_and_token(&root_dir, None).unwrap();
        let iid = file.instance_id.clone();
        let meta = InstanceMeta {
            instance_id: file.instance_id.clone(),
            owner_id: file.owner_id.clone(),
            product: file.product.clone(),
            protocol_version: file.protocol_version.clone(),
            node_id: file.node_id.clone(),
            generation: "gen-test-1".to_owned(),
            server_revision: "srv-test-1".to_owned(),
            initialization_revision: file.initialization_revision.clone(),
        };
        let store = V1Store::open_and_recover(&root_dir, meta).unwrap();
        store
            .register_project(&json!({
                "instanceId": iid,
                "projectId": "proj-pump",
                "name": "Pump",
                "environmentId": "local",
                "root": {"instanceId": iid, "rootId": "root-1", "relativePath": "workspace"}
            }))
            .unwrap();
        store
            .create_session(
                &json!({"instanceId": iid, "sessionId": "sess-pump", "projectId": "proj-pump"}),
            )
            .unwrap();
        let run_id = match store
            .submit_run(
                &json!({"instanceId": iid, "sessionId": "sess-pump", "input": [{"kind": "text", "text": "[pd-driver:sleep=60000]"}]}),
                Some("key-pump-1"),
            )
            .unwrap()
        {
            SubmitDisposition::NewRun { run_id, .. } => run_id,
            other => panic!("expected a new run, got {other:?}"),
        };
        store
            .transition_run_state(
                &run_id,
                RunTransition::new(
                    agentcpm_pd_protocol::contract::RunState::Running,
                    "run.started",
                    &json!({"runId": run_id}),
                ),
            )
            .unwrap();

        let runtime = PublicV1Runtime::new(store);
        let state = Arc::new(PublicV1State::new(runtime, "test-token".to_owned()));

        // Subscription bookkeeping the pump cleans up on exit.
        let subscriptions: Arc<Mutex<HashMap<String, ActiveSubscription>>> = Arc::default();
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(16);
        let (sub_tx, _sub_rx) = mpsc::channel(16);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let sub_id = "sub-dead-watch".to_owned();
        subscriptions.lock().await.insert(
            sub_id.clone(),
            ActiveSubscription {
                credit: Arc::new(AtomicU64::new(64)),
                credit_notify: Arc::new(Notify::new()),
                cancel_tx,
            },
        );

        // The aborted connection: the watch sender is dropped without ever
        // sending `true` (the pre-fix pump busy-looped on this forever).
        let (conn_stop_tx, conn_stop) = watch::channel(false);
        drop(conn_stop_tx);

        let pump = tokio::spawn(run_subscription_pump(
            state,
            subscriptions.clone(),
            ctrl_tx,
            sub_tx,
            sub_id.clone(),
            run_id,
            0,
            Arc::new(AtomicU64::new(64)),
            Arc::new(Notify::new()),
            cancel_rx,
            conn_stop,
        ));

        tokio::time::timeout(Duration::from_secs(5), pump)
            .await
            .expect("the pump exits after the connection watch sender drops")
            .unwrap();
        assert!(
            subscriptions.lock().await.get(&sub_id).is_none(),
            "the pump removed its subscription entry"
        );
    }
}
