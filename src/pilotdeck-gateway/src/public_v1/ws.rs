//! `pd-connect/1` WebSocket transport session over `/v1/ws` (`007 §6, WP4`).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use agentcpm_pd_protocol::contract::{EventEnvelope, OPERATION_VERSION};
use agentcpm_pd_protocol::envelope::is_control_method;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_RESOURCE_LIMIT, CODE_UNAUTHENTICATED, ContractError,
    ERR_NOT_INITIALIZED, ERR_UNSUPPORTED_VERSION, EffectStatus, JSONRPC_INVALID_PARAMS,
    JSONRPC_INVALID_REQUEST, JSONRPC_PARSE_ERROR, RetrySpec,
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

    let writer_handle = tokio::spawn(async move {
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
                    let _ = conn_stop_tx.send(true);
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
                        handle_ws_text_frame(
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
                        )
                        .await;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }

    let _ = conn_stop_tx.send(true);
    let mut guard = subscriptions.lock().await;
    for (_, sub) in guard.drain() {
        let _ = sub.cancel_tx.send(true);
    }
    drop(guard);

    hb_handle.abort();
    drop(ctrl_tx);
    drop(norm_tx);
    drop(sub_tx);
    let _ = tokio::time::timeout(Duration::from_millis(200), writer_handle).await;
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
            let guard = subscriptions.lock().await;
            if let Some(sub) = guard.get(&params.subscription_id) {
                sub.credit
                    .fetch_add(u64::from(params.add), Ordering::SeqCst);
                sub.credit_notify.notify_one();
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

    if params.stream != "run.events" {
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

    let reg = OperationRegistry::global();
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
    let follow = params
        .input
        .get("follow")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let initial_credit = u64::from(params.credit.unwrap_or(DEFAULT_CREDIT));

    let start_check = match state
        .runtime
        .store()
        .check_run_subscription_start(&run_id, after_cursor.as_deref())
    {
        Ok(res) => res,
        Err(err) => {
            send_json_frame(ctrl_tx, &JsonRpcResponse::application_error(req.id, &err)).await;
            return;
        }
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

    // Immediate Gap termination if requested afterCursor expired before earliest retained event.
    let start_seq = match start_check {
        Ok(seq) => seq,
        Err(gap) => {
            send_sub_end(
                ctrl_tx,
                &sub_id,
                SubTerminal::Gap,
                after_cursor,
                Some(gap),
                None,
            )
            .await;
            return;
        }
    };

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

    let pump_state = state.clone();
    let pump_subs = subscriptions.clone();
    let pump_ctrl = ctrl_tx.clone();
    let pump_sub = sub_tx.clone();
    let mut pump_conn_stop = conn_stop.clone();
    tokio::spawn(async move {
        let mut cancel_rx = cancel_rx;
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
            if buffer.len() > SUBSCRIPTION_BUFFER_ITEMS || buffer_bytes > SUBSCRIPTION_BUFFER_BYTES
            {
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

            if buffer.is_empty() && (!follow || terminal_reached) {
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
                _ = cancel_rx.changed() => {}
                _ = pump_conn_stop.changed() => {}
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }

        pump_subs.lock().await.remove(&sub_id);
    });
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
    if let Ok(txt) = serde_json::to_string(frame) {
        let _ = tx.send(Message::Text(txt.into())).await;
    }
}

pub fn server_started_at() -> String {
    now_rfc3339()
}
