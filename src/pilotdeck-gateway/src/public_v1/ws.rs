//! `pd-connect/1` WebSocket transport session over `/v1/ws` (`007 §6, WP4`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
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
use crate::public_v1::store::{StoredEvent, V1Store, now_rfc3339};

const MAX_SUBSCRIPTION_CREDIT: u64 = 100_000;
const GATEWAY_FEATURES: &[&str] = &[
    "subscriptions",
    "control-priority",
    "idempotency",
    "credit",
    "heartbeat",
];

struct ActiveSubscription {
    credit: Arc<AtomicU64>,
    credit_notify: Arc<Notify>,
    cancel_tx: watch::Sender<bool>,
}

struct InFlightGuard {
    set: Arc<StdMutex<HashSet<String>>>,
    id: String,
}

impl InFlightGuard {
    fn try_acquire(set: &Arc<StdMutex<HashSet<String>>>, id: &str) -> Option<Self> {
        let mut guard = set.lock().expect("ws in-flight mutex");
        if !guard.insert(id.to_owned()) {
            return None;
        }
        Some(Self {
            set: set.clone(),
            id: id.to_owned(),
        })
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.set.lock() {
            guard.remove(&self.id);
        }
    }
}

struct SubRegistrationGuard {
    subs: Arc<Mutex<HashMap<String, ActiveSubscription>>>,
    sub_id: String,
    armed: bool,
}

impl Drop for SubRegistrationGuard {
    fn drop(&mut self) {
        if self.armed {
            let subs = self.subs.clone();
            let sub_id = self.sub_id.clone();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    if let Some(sub) = subs.lock().await.remove(&sub_id) {
                        let _ = sub.cancel_tx.send(true);
                    }
                });
            }
        }
    }
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
///
/// `http_credential` is the Bearer credential the connection presented at
/// the HTTP upgrade (always present on `/v1/ws` — the upgrade was
/// authorized). The session re-verifies it (or the `initialize`
/// credential, when the client presents one) at every inbound frame
/// admission boundary, so disabling or revoking an access key closes the
/// stream at its next activity ("旧流下次鉴权关闭") instead of keeping a
/// dead credential's connection alive. The credential is never logged.
pub async fn serve_v1_ws<S>(
    ws_stream: WebSocketStream<S>,
    state: Arc<PublicV1State>,
    mut server_stop: watch::Receiver<bool>,
    http_credential: Option<String>,
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
    let in_flight_ids: Arc<StdMutex<HashSet<String>>> = Arc::new(StdMutex::new(HashSet::new()));
    let subscriptions: Arc<Mutex<HashMap<String, ActiveSubscription>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let conn_id = state.runtime.store().next_id();

    // The credential this session authenticated with at HTTP upgrade; must
    // remain present and match any credential presented on `initialize`.
    let session_credential = http_credential;
    let mut revocation_rx = state.subscribe_auth_revocation();

    // Fail closed immediately if HTTP upgrade did not supply a valid credential.
    let initial_valid = match session_credential.as_deref() {
        Some(c) => state.verify_token_async(c).await,
        None => false,
    };
    if !initial_valid {
        let _ = ctrl_tx
            .send(Message::Close(Some(
                tokio_tungstenite::tungstenite::protocol::CloseFrame {
                    code: CloseCode::Policy,
                    reason: "missing or invalid session credential".into(),
                },
            )))
            .await;
        drop(ctrl_tx);
        drop(norm_tx);
        drop(sub_tx);
        let _ = tokio::time::timeout(Duration::from_millis(200), &mut writer_handle).await;
        return;
    }

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
        if *server_stop.borrow() {
            shutdown_v1_ws(&conn_stop_guard, &subscriptions, &ctrl_tx).await;
            break;
        }
        tokio::select! {
            biased;
            _ = server_stop.changed() => {
                if *server_stop.borrow() {
                    shutdown_v1_ws(&conn_stop_guard, &subscriptions, &ctrl_tx).await;
                    break;
                }
            }
            changed = revocation_rx.changed() => {
                if changed.is_ok() {
                    let still_valid = match session_credential.as_deref() {
                        Some(c) => state.verify_token_async(c).await,
                        None => false,
                    };
                    if !still_valid {
                        // Give active subscription pumps a short window to flush
                        // `sub.end(error/UNAUTHENTICATED)` onto `ctrl_tx` before closing.
                        tokio::time::sleep(Duration::from_millis(80)).await;
                        let subs = {
                            let mut guard = subscriptions.lock().await;
                            guard.drain().collect::<Vec<_>>()
                        };
                        for (_, sub) in subs {
                            let _ = sub.cancel_tx.send(true);
                        }
                        let _ = ctrl_tx
                            .send(Message::Close(Some(
                                tokio_tungstenite::tungstenite::protocol::CloseFrame {
                                    code: CloseCode::Policy,
                                    reason: "credential no longer active".into(),
                                },
                            )))
                            .await;
                        break;
                    }
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
                                Some(json!({
                                    "reason": "frame_too_large",
                                    "maxFrameBytes": MAX_FRAME_BYTES,
                                })),
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
                            &session_credential,
                            &initialized,
                            &control_sem,
                            &normal_sem,
                            &in_flight_ids,
                            &subscriptions,
                            &ctrl_tx,
                            &norm_tx,
                            &sub_tx,
                            &conn_stop_rx,
                        );
                        enum InnerOutcome {
                            Stop,
                            Done(bool),
                        }
                        tokio::pin!(handled);
                        let outcome = loop {
                            tokio::select! {
                                biased;
                                res = server_stop.changed() => {
                                    if res.is_err() || *server_stop.borrow() {
                                        break InnerOutcome::Stop;
                                    }
                                }
                                result = &mut handled => break InnerOutcome::Done(result),
                            }
                        };
                        match outcome {
                            InnerOutcome::Stop => {
                                shutdown_v1_ws(&conn_stop_guard, &subscriptions, &ctrl_tx).await;
                                break;
                            }
                            InnerOutcome::Done(true) => {
                                let _ = ctrl_tx
                                    .send(Message::Close(Some(
                                        tokio_tungstenite::tungstenite::protocol::CloseFrame {
                                            code: CloseCode::Policy,
                                            reason: "credential no longer active".into(),
                                        },
                                    )))
                                    .await;
                                break;
                            }
                            InnerOutcome::Done(false) => {}
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

async fn shutdown_v1_ws(
    conn_stop_guard: &ConnStopOnDrop,
    subscriptions: &Arc<Mutex<HashMap<String, ActiveSubscription>>>,
    ctrl_tx: &mpsc::Sender<Message>,
) {
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
        .send(Message::Close(Some(
            tokio_tungstenite::tungstenite::protocol::CloseFrame {
                code: CloseCode::Restart,
                reason: "gateway shutting down".into(),
            },
        )))
        .await;
}

/// Handles one inbound text frame. Returns `true` when the session's
/// credential stopped verifying (Plan A disconnect); `false` on every other
/// path.
#[allow(clippy::too_many_arguments)]
async fn handle_ws_text_frame(
    raw: &str,
    state: &Arc<PublicV1State>,
    conn_id: &str,
    session_credential: &Option<String>,
    initialized: &Arc<AtomicBool>,
    control_sem: &Arc<Semaphore>,
    normal_sem: &Arc<Semaphore>,
    in_flight_ids: &Arc<StdMutex<HashSet<String>>>,
    subscriptions: &Arc<Mutex<HashMap<String, ActiveSubscription>>>,
    ctrl_tx: &mpsc::Sender<Message>,
    norm_tx: &mpsc::Sender<Message>,
    sub_tx: &mpsc::Sender<Message>,
    conn_stop: &watch::Receiver<bool>,
) -> bool {
    // Plan A fail-closed re-verification BEFORE full JSON processing:
    // revoked/disabled or missing session credentials are rejected first.
    let cred_valid = match session_credential.as_deref() {
        Some(credential) => state.verify_token_async(credential).await,
        None => false,
    };
    if !cred_valid {
        let err = ContractError::new(
            CODE_UNAUTHENTICATED,
            "credential rejected: access key is no longer active",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        );
        let request_id = serde_json::from_str::<Value>(raw)
            .ok()
            .and_then(|v| v.get("id").and_then(Value::as_str).map(ToOwned::to_owned))
            .unwrap_or_default();
        send_json_frame(
            ctrl_tx,
            &JsonRpcResponse::application_error(request_id, &err),
        )
        .await;
        return true;
    }

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
            return false;
        }
    };

    if val.get("id").is_none() || val.get("id").is_some_and(Value::is_null) {
        if let Ok(notif) = serde_json::from_value::<JsonRpcNotification>(val)
            && notif.method == WIRE_CONN_ABANDON
            && serde_json::from_value::<ConnAbandonParams>(notif.params).is_ok()
        {
            // Per D12/T11: conn.abandon MUST NOT cancel the underlying Gateway run.
        }
        return false;
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
            return false;
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
        return false;
    }

    if let Err(msg) = validate_request_id(&req.id) {
        send_json_frame(
            ctrl_tx,
            &JsonRpcResponse::err(Some(req.id), JSONRPC_INVALID_REQUEST, msg, None),
        )
        .await;
        return false;
    }

    // Track unique in-flight requestId (`007 §6.3`): duplicate concurrent ids
    // immediately fail with JSON-RPC -32600 without affecting the first call.
    let in_flight_guard = match InFlightGuard::try_acquire(in_flight_ids, &req.id) {
        Some(guard) => guard,
        None => {
            send_json_frame(
                ctrl_tx,
                &JsonRpcResponse::err(
                    Some(req.id),
                    JSONRPC_INVALID_REQUEST,
                    "duplicate in-flight requestId",
                    None,
                ),
            )
            .await;
            return false;
        }
    };

    if req.method == WIRE_INITIALIZE {
        if initialized.load(Ordering::SeqCst) {
            send_json_frame(
                ctrl_tx,
                &JsonRpcResponse::err(
                    Some(req.id),
                    JSONRPC_INVALID_REQUEST,
                    "connection already initialized",
                    None,
                ),
            )
            .await;
            return false;
        }
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
                return false;
            }
        };
        if params.abi_version != ABI_VERSION {
            send_json_frame(
                ctrl_tx,
                &JsonRpcResponse::err(
                    Some(req.id),
                    ERR_UNSUPPORTED_VERSION,
                    format!("unsupported abiVersion={}", params.abi_version),
                    Some(json!({"supported": [ABI_VERSION]})),
                ),
            )
            .await;
            return false;
        }
        if params.api_version != OPERATION_VERSION {
            send_json_frame(
                ctrl_tx,
                &JsonRpcResponse::err(
                    Some(req.id),
                    ERR_UNSUPPORTED_VERSION,
                    format!("unsupported apiVersion={}", params.api_version),
                    Some(json!({"supported": [OPERATION_VERSION]})),
                ),
            )
            .await;
            return false;
        }
        if let Some(cred) = &params.credential {
            let matches_upgrade = session_credential.as_deref().is_some_and(|hc| {
                crate::public_v1::constant_time_eq(hc.as_bytes(), cred.as_bytes())
            });
            if !matches_upgrade || !state.verify_token_async(cred).await {
                let err = ContractError::new(
                    CODE_UNAUTHENTICATED,
                    "invalid or mismatched credential during initialize",
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                );
                send_json_frame(ctrl_tx, &JsonRpcResponse::application_error(req.id, &err)).await;
                return true;
            }
        }

        let supported_features: Vec<String> = GATEWAY_FEATURES
            .iter()
            .copied()
            .filter(|feature| params.features.iter().any(|requested| requested == feature))
            .map(ToOwned::to_owned)
            .collect();

        initialized.store(true, Ordering::SeqCst);
        let meta = state.runtime.store().meta();
        let res = InitializeResult {
            abi_version: ABI_VERSION,
            supported: true,
            api_version: OPERATION_VERSION.to_owned(),
            supported_features,
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
        return false;
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
        return false;
    }

    match req.method.as_str() {
        WIRE_SUB_OPEN => {
            handle_sub_open(
                req,
                state,
                session_credential,
                subscriptions,
                ctrl_tx,
                sub_tx,
                conn_stop,
            )
            .await;
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
                    return false;
                }
            };
            let add_amount = u64::from(params.add);
            if add_amount == 0 || add_amount > MAX_SUBSCRIPTION_CREDIT {
                let err = ContractError::new(
                    CODE_INVALID_ARGUMENT,
                    format!(
                        "sub.credit `add` must be within 1..={MAX_SUBSCRIPTION_CREDIT}, got {add_amount}"
                    ),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                );
                send_json_frame(ctrl_tx, &JsonRpcResponse::application_error(req.id, &err)).await;
                return false;
            }
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
                let mut cur = credit.load(Ordering::SeqCst);
                let accepted = loop {
                    let Some(next) = cur.checked_add(add_amount) else {
                        break false;
                    };
                    if next > MAX_SUBSCRIPTION_CREDIT {
                        break false;
                    }
                    match credit.compare_exchange_weak(
                        cur,
                        next,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    ) {
                        Ok(_) => break true,
                        Err(actual) => cur = actual,
                    }
                };
                if !accepted {
                    let err = ContractError::new(
                        CODE_RESOURCE_LIMIT,
                        format!(
                            "subscription `{}` credit exceeds maximum {MAX_SUBSCRIPTION_CREDIT}",
                            params.subscription_id
                        ),
                        RetrySpec::backoff_read(Some(100)),
                        EffectStatus::NotApplied,
                    );
                    send_json_frame(ctrl_tx, &JsonRpcResponse::application_error(req.id, &err))
                        .await;
                    return false;
                }
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
                    return false;
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
                return false;
            };
            let st = state.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let _in_flight_guard = in_flight_guard;
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
    false
}

async fn handle_sub_open(
    req: JsonRpcRequest,
    state: &Arc<PublicV1State>,
    session_credential: &Option<String>,
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
    // `execution='stream'` operations, of which only implemented ones are
    // served (S3 substrate: "implemented" = a registered handler, replacing
    // the JSON `currentRound` check); everything else answers -32004 like
    // before. t015: a stream additionally needs a registered data plane
    // (a [`StreamSource`] factory) — `run.events` is built in here, every
    // other domain registers its own through [`register_stream_source`].
    let reg = OperationRegistry::global();
    let factory = stream_source_factory(&params.stream);
    let stream_admitted = reg.get(&params.stream).is_some_and(|spec| {
        spec.execution == agentcpm_pd_protocol::registry::ExecutionMode::Stream
            && agentcpm_pd_protocol::registry::is_implemented(&params.stream)
    }) && factory.is_some();
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

    // t015: instanceId binding follows the requested stream's own declaration
    // (`run.events` declares and requires it; `terminals.output` forbids
    // additional properties, so the pre-generalization forced injection
    // would have failed its validation). A present instanceId is still
    // verified against this instance either way.
    let declares_instance_id = reg
        .get(&params.stream)
        .is_some_and(|spec| spec.declares_instance_id);
    if let Err(err) = agentcpm_pd_protocol::envelope::bind_instance_id(
        &mut params.input,
        &state.runtime.store().meta().instance_id,
        declares_instance_id,
    ) {
        send_json_frame(ctrl_tx, &JsonRpcResponse::application_error(req.id, &err)).await;
        return;
    }
    // t015: input validation follows the requested stream's own schema (the
    // pre-generalization code hardcoded `run.events` here — correct only as
    // long as no other stream was ever admitted).
    if let Err(errs) = reg.validate_input(&params.stream, &params.input) {
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

    let initial_credit = u64::from(params.credit.unwrap_or(DEFAULT_CREDIT));
    if initial_credit > MAX_SUBSCRIPTION_CREDIT {
        let err = ContractError::new(
            CODE_INVALID_ARGUMENT,
            format!(
                "sub.open `credit` must not exceed {MAX_SUBSCRIPTION_CREDIT}, got {initial_credit}"
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        );
        send_json_frame(ctrl_tx, &JsonRpcResponse::application_error(req.id, &err)).await;
        return;
    }

    // t015: the source factory owns the stream's start-position validation
    // (`run.events`: `check_run_subscription_start`, with the -32000
    // CURSOR_EXPIRED + `details.gap` rejection below; `terminals.output`:
    // outputEpoch/byteOffset checks). Errors answer the sub.open RPC before
    // any subscription id becomes visible — same wire position the run
    // start-check held.
    let Some(factory) = factory else {
        return;
    };
    let sub_id = state.runtime.store().next_id();
    let source = match factory(state, &params.input, &sub_id) {
        Ok(source) => source,
        Err(err) => {
            send_json_frame(ctrl_tx, &JsonRpcResponse::application_error(req.id, &err)).await;
            return;
        }
    };
    let credit = Arc::new(AtomicU64::new(initial_credit));
    let credit_notify = Arc::new(Notify::new());
    let (cancel_tx, cancel_rx) = watch::channel(false);

    // Register subscription BEFORE sending OK frame so a concurrent `server_stop`
    // cancellation cannot leave an orphaned un-tracked subscription on client.
    subscriptions.lock().await.insert(
        sub_id.clone(),
        ActiveSubscription {
            credit: credit.clone(),
            credit_notify: credit_notify.clone(),
            cancel_tx,
        },
    );
    let mut reg_guard = SubRegistrationGuard {
        subs: subscriptions.clone(),
        sub_id: sub_id.clone(),
        armed: true,
    };

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

    reg_guard.armed = false;
    let revocation_rx = state.subscribe_auth_revocation();
    tokio::spawn(subscription_pump(
        subscriptions.clone(),
        state.clone(),
        session_credential.clone(),
        revocation_rx,
        ctrl_tx.clone(),
        sub_tx.clone(),
        sub_id,
        source,
        credit,
        credit_notify,
        cancel_rx,
        conn_stop.clone(),
    ));
}

// ---------------------------------------------------------------------------
// Stream sources (t015): the subscription pump's pluggable data plane
// ---------------------------------------------------------------------------

/// One deliverable frame of a stream subscription, fully produced by a
/// [`StreamSource`]: the serialized `sub.event` notification text, the resume
/// cursor it advances to, and its charge against the slow-consumer byte
/// budget.
pub struct StreamFrame {
    /// The serialized JSON-RPC `sub.event` notification (the source embeds
    /// the subscription id it was constructed with).
    pub text: String,
    /// The frame's cursor — reported as `sub.end.resumeCursor` once
    /// everything up to and including it has been delivered.
    pub cursor: String,
    /// Charge against `SUBSCRIPTION_BUFFER_BYTES`.
    pub estimated_bytes: usize,
}

/// The result of one [`StreamSource`] fetch.
pub struct StreamFetch {
    /// Frames in stream order, already past everything previously fetched.
    pub frames: Vec<StreamFrame>,
    /// The stream reached its terminal state (run terminal for
    /// `run.events`; exited/failed/lost with the exit frame emitted for
    /// `terminals.output`). The pump completes the subscription only once
    /// the buffer drains AND a fetch came up short.
    pub stream_ended: bool,
}

/// The data plane of one stream subscription. The pump drives it with a
/// batch size, sleeps on [`StreamSource::wake`] between fetches, and enforces
/// the wire contract (credit, frame budget, slow consumer, terminal lanes)
/// uniformly. Implementations own their cursor/start-position validation and
/// report failures as contract errors the `sub.open` RPC relays.
///
/// `run.events` is the first implementation ([`RunEventsSource`], the
/// original pump logic byte-for-byte); `terminals.output` is the second
/// (`domains/terminals.rs`). Future S3 streams (`resources.watch`,
/// `notifications.events`, `operations.events`) register through
/// [`register_stream_source`] without touching the pump again.
pub trait StreamSource: Send {
    /// Fetch the next batch of frames after everything already fetched.
    fn fetch_next(&mut self, batch: usize) -> Result<StreamFetch, ContractError>;
    /// A notification that may carry new frames; the pump re-fetches on it.
    fn wake(&self) -> Arc<Notify>;
    /// Recovery guidance for the INPUT_TOO_LARGE error of an oversized
    /// frame (run.events: the snapshot re-open flow; terminals.output: the
    /// earliest-retained-cursor re-open).
    fn oversized_recovery_hint(&self) -> &'static str {
        "resume from run.snapshot"
    }
}

/// Builds one stream's data plane from its `sub.open` input, validating the
/// start position (cursor shape and retention) before any subscription id
/// becomes visible. Errors are contract errors answered on the sub.open RPC.
pub type StreamSourceFactory =
    fn(&Arc<PublicV1State>, &Value, &str) -> Result<Box<dyn StreamSource>, ContractError>;

fn stream_source_factories() -> &'static std::sync::Mutex<HashMap<&'static str, StreamSourceFactory>>
{
    static FACTORIES: std::sync::OnceLock<
        std::sync::Mutex<HashMap<&'static str, StreamSourceFactory>>,
    > = std::sync::OnceLock::new();
    FACTORIES.get_or_init(|| {
        let mut map: HashMap<&'static str, StreamSourceFactory> = HashMap::new();
        map.insert("run.events", RunEventsSource::start);
        std::sync::Mutex::new(map)
    })
}

/// Registers `method_id`'s stream data plane. Domain files call this from
/// their `register()` (the domain table build happens before the first
/// connection is served, so the map is complete by then). A duplicate
/// registration for one stream id panics — two domains claiming one stream
/// is a wiring bug.
pub fn register_stream_source(method_id: &'static str, factory: StreamSourceFactory) {
    let previous = stream_source_factories()
        .lock()
        .expect("stream source factories")
        .insert(method_id, factory);
    assert!(
        previous.is_none(),
        "duplicate stream source registration for `{method_id}`"
    );
}

fn stream_source_factory(method_id: &str) -> Option<StreamSourceFactory> {
    stream_source_factories()
        .lock()
        .expect("stream source factories")
        .get(method_id)
        .copied()
}

/// The `run.events` data plane — the pump's original logic, extracted
/// unchanged: SQLite events strictly after `last_fetched_seq`, completion
/// gated on the run's terminal state, waking on the store-wide event
/// notifier.
struct RunEventsSource {
    store: Arc<V1Store>,
    instance_id: String,
    run_id: String,
    sub_id: String,
    last_fetched_seq: i64,
}

impl RunEventsSource {
    /// The stream's `sub.open` admission: resolves the start position and
    /// rejects expired/invalid cursors (moved verbatim from the pre-t015
    /// `handle_sub_open`).
    fn start(
        state: &Arc<PublicV1State>,
        input: &Value,
        sub_id: &str,
    ) -> Result<Box<dyn StreamSource>, ContractError> {
        let run_id = input["runId"].as_str().unwrap_or_default().to_owned();
        let after_cursor = input
            .get("afterCursor")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        match state
            .runtime
            .store()
            .check_run_subscription_start(&run_id, after_cursor.as_deref())
        {
            // P1 (wp4 review): a cursor that is already expired at open is
            // rejected with -32000 CURSOR_EXPIRED and `details.gap` carrying the
            // `$defs.gap` object — only mid-subscription gaps end via
            // `sub.end` (007 §6.4 / public-contract transportContract).
            Err(err) => Err(err),
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
                Err(err)
            }
            Ok(Ok(start_seq)) => Ok(Box::new(Self {
                store: state.runtime.store().clone(),
                instance_id: state.runtime.store().meta().instance_id.clone(),
                run_id,
                sub_id: sub_id.to_owned(),
                last_fetched_seq: start_seq,
            })),
        }
    }
}

impl StreamSource for RunEventsSource {
    fn fetch_next(&mut self, batch: usize) -> Result<StreamFetch, ContractError> {
        let (events, stream_ended) =
            self.store
                .fetch_run_events_after(&self.run_id, self.last_fetched_seq, batch)?;
        let mut frames = Vec::with_capacity(events.len());
        for ev in events {
            self.last_fetched_seq = ev.seq;
            let estimated_bytes = estimate_event_bytes(&ev);
            let cursor = ev.cursor.clone();
            let notif = JsonRpcNotification {
                jsonrpc: JSONRPC_VERSION.to_owned(),
                method: WIRE_SUB_EVENT.to_owned(),
                params: serde_json::to_value(SubEventNotification {
                    subscription_id: self.sub_id.clone(),
                    event: EventEnvelope {
                        version: "agentcpm-pd.event/1".to_owned(),
                        instance_id: self.instance_id.clone(),
                        run_id: Some(ev.resource_id.clone()),
                        operation_id: None,
                        subscription_id: Some(self.sub_id.clone()),
                        cursor: ev.cursor.clone(),
                        event_type: ev.event_type.clone(),
                        data: ev.data.clone(),
                    },
                })
                .unwrap_or(Value::Null),
            };
            // Both serializations of this structurally-fixed notification
            // succeed; an empty text is the pump's skip marker.
            let text = serde_json::to_string(&notif).unwrap_or_default();
            frames.push(StreamFrame {
                text,
                cursor,
                estimated_bytes,
            });
        }
        Ok(StreamFetch {
            frames,
            stream_ended,
        })
    }

    fn wake(&self) -> Arc<Notify> {
        self.store.event_notifier()
    }
}

/// The per-subscription event pump. Extracted so the dead-watch regression
/// test can drive it directly (`P1` wp4 review); t015 made its data plane a
/// [`StreamSource`] so `run.events` and `terminals.output` (and later
/// streams) share the wire contract enforcement unchanged.
#[allow(clippy::too_many_arguments)]
async fn subscription_pump(
    pump_subs: Arc<Mutex<HashMap<String, ActiveSubscription>>>,
    state: Arc<PublicV1State>,
    session_credential: Option<String>,
    mut revocation_rx: watch::Receiver<u64>,
    pump_ctrl: mpsc::Sender<Message>,
    pump_sub: mpsc::Sender<Message>,
    sub_id: String,
    mut source: Box<dyn StreamSource>,
    credit: Arc<AtomicU64>,
    credit_notify: Arc<Notify>,
    mut cancel_rx: watch::Receiver<bool>,
    mut pump_conn_stop: watch::Receiver<bool>,
) {
    let mut last_sent_cursor: Option<String> = None;
    let mut buffer: VecDeque<StreamFrame> = VecDeque::new();
    let mut buffer_bytes: usize = 0;
    let event_notify = source.wake();
    let fetch_batch = 256usize;
    let mut fetched_full_batch;

    // Force initial borrow sync so subsequent `.changed()` fires strictly on new updates,
    // while checking current credential validity up front.
    let _ = revocation_rx.borrow_and_update();

    'pump: loop {
        // Active session credential revocation check before each batch/poll.
        let cred_valid = match session_credential.as_deref() {
            Some(c) => state.verify_token_async(c).await,
            None => false,
        };
        if !cred_valid {
            buffer.clear();
            let err = ContractError::new(
                CODE_UNAUTHENTICATED,
                "credential rejected: access key is no longer active",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            );
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

        if *cancel_rx.borrow() || *pump_conn_stop.borrow() {
            let term = if *pump_conn_stop.borrow() {
                SubTerminal::Shutdown
            } else {
                SubTerminal::Closed
            };
            send_sub_end(&pump_ctrl, &sub_id, term, last_sent_cursor, None, None).await;
            break;
        }

        let event_notified = event_notify.notified();
        tokio::pin!(event_notified);
        event_notified.as_mut().enable();
        let credit_notified = credit_notify.notified();
        tokio::pin!(credit_notified);
        credit_notified.as_mut().enable();

        let terminal_reached;
        match source.fetch_next(fetch_batch) {
            Ok(fetch) => {
                terminal_reached = fetch.stream_ended;
                fetched_full_batch = fetch.frames.len() >= fetch_batch;
                for frame in fetch.frames {
                    buffer_bytes = buffer_bytes.saturating_add(frame.estimated_bytes);
                    buffer.push_back(frame);
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
            && let Some(frame) = buffer.front()
        {
            if frame.text.is_empty() {
                let frame_bytes = frame.estimated_bytes;
                let _ = buffer.pop_front();
                buffer_bytes = buffer_bytes.saturating_sub(frame_bytes);
                continue;
            }
            if frame.text.len() > MAX_FRAME_BYTES {
                let err = ContractError::new(
                    "INPUT_TOO_LARGE",
                    format!(
                        "event {} exceeds the {MAX_FRAME_BYTES} byte frame limit; {}",
                        frame.cursor,
                        source.oversized_recovery_hint()
                    ),
                    RetrySpec::backoff_read(Some(250)),
                    EffectStatus::NotApplied,
                );
                let _ = buffer.pop_front();
                send_sub_end_bounded(
                    &pump_sub,
                    &pump_ctrl,
                    &mut cancel_rx,
                    &mut pump_conn_stop,
                    &sub_id,
                    SubTerminal::Error,
                    last_sent_cursor.clone(),
                    None,
                    Some(err),
                )
                .await;
                break 'pump;
            }
            match pump_sub.try_send(Message::Text(frame.text.clone().into())) {
                Ok(()) => {
                    credit.fetch_sub(1, Ordering::SeqCst);
                    last_sent_cursor = Some(frame.cursor.clone());
                    let frame_bytes = frame.estimated_bytes;
                    let _ = buffer.pop_front();
                    buffer_bytes = buffer_bytes.saturating_sub(frame_bytes);
                }
                Err(mpsc::error::TrySendError::Full(_)) => break,
                Err(mpsc::error::TrySendError::Closed(_)) => break 'pump,
            }
        }

        if buffer.len() > SUBSCRIPTION_BUFFER_ITEMS || buffer_bytes > SUBSCRIPTION_BUFFER_BYTES {
            buffer.clear();
            send_sub_end_bounded(
                &pump_sub,
                &pump_ctrl,
                &mut cancel_rx,
                &mut pump_conn_stop,
                &sub_id,
                SubTerminal::SlowConsumer,
                last_sent_cursor,
                None,
                None,
            )
            .await;
            break;
        }

        if buffer.is_empty() && terminal_reached && !fetched_full_batch {
            send_sub_end_bounded(
                &pump_sub,
                &pump_ctrl,
                &mut cancel_rx,
                &mut pump_conn_stop,
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
            _ = event_notified => {}
            _ = credit_notified => {}
            _ = revocation_rx.changed() => {
                // Loop immediately so the credential re-check runs on wake.
            }
            changed = cancel_rx.changed() => {
                if changed.is_err() {
                    break;
                }
            }
            changed = pump_conn_stop.changed() => {
                if changed.is_err() {
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

#[allow(clippy::too_many_arguments)]
async fn send_sub_end_bounded(
    primary_tx: &mpsc::Sender<Message>,
    ctrl_fallback_tx: &mpsc::Sender<Message>,
    cancel_rx: &mut watch::Receiver<bool>,
    conn_stop_rx: &mut watch::Receiver<bool>,
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
    let Ok(txt) = serde_json::to_string(&notif) else {
        return;
    };
    let msg = Message::Text(txt.into());
    if primary_tx.try_send(msg.clone()).is_ok() {
        return;
    }
    tokio::select! {
        biased;
        _ = cancel_rx.changed() => {
            let _ = ctrl_fallback_tx.try_send(msg);
        }
        _ = conn_stop_rx.changed() => {
            let _ = ctrl_fallback_tx.try_send(msg);
        }
        res = tokio::time::timeout(Duration::from_millis(200), primary_tx.send(msg.clone())) => {
            if !matches!(res, Ok(Ok(()))) {
                let _ = ctrl_fallback_tx.try_send(msg);
            }
        }
    }
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

        let source = RunEventsSource::start(&state, &json!({"runId": run_id}), &sub_id)
            .expect("run.events source starts");
        let pump = tokio::spawn(subscription_pump(
            subscriptions.clone(),
            state.clone(),
            Some("test-token".to_owned()),
            state.subscribe_auth_revocation(),
            ctrl_tx,
            sub_tx,
            sub_id.clone(),
            source,
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
    /// P1 (WP5 e2e): `completed` may only fire when the last fetch came up
    /// short — a capped 256-event batch on an already-terminal run used to
    /// send `completed` right after the first batch, silently skipping every
    /// event beyond it (reachable via the documented snapshot ->
    /// resubscribe recovery on a finished run). The pump here gets a big
    /// credit, so the only correct outcome is: every event delivered, then
    /// exactly one `completed`.
    #[tokio::test]
    async fn completed_requires_the_fetch_to_have_caught_up() {
        let tmp = TempDir::new("pump-capped");
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
                "projectId": "proj-capped",
                "name": "CappedFetch",
                "environmentId": "local",
                "root": {"instanceId": iid, "rootId": "root-1", "relativePath": "workspace"}
            }))
            .unwrap();
        store
            .create_session(
                &json!({"instanceId": iid, "sessionId": "sess-capped", "projectId": "proj-capped"}),
            )
            .unwrap();
        let run_id = match store
            .submit_run(
                &json!({"instanceId": iid, "sessionId": "sess-capped", "input": [{"kind": "text", "text": "capped"}]}),
                Some("key-capped-1"),
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
        // 300 events: the pump's fetch batch is 256, so the first fetch is
        // capped and the run is already terminal.
        for i in 0..300 {
            store
                .append_run_progress_event(
                    &run_id,
                    "sess-capped",
                    "run.output.delta",
                    &json!({"runId": run_id, "i": i}),
                )
                .unwrap();
        }
        store
            .transition_run_state(
                &run_id,
                RunTransition::new(
                    agentcpm_pd_protocol::contract::RunState::Succeeded,
                    "run.completed",
                    &json!({"runId": run_id}),
                ),
            )
            .unwrap();

        let runtime = PublicV1Runtime::new(store);
        let state = Arc::new(PublicV1State::new(runtime, "test-token".to_owned()));
        let subscriptions: Arc<Mutex<HashMap<String, ActiveSubscription>>> = Arc::default();
        let (ctrl_tx, mut ctrl_rx) = mpsc::channel(1024);
        let (sub_tx, mut sub_rx) = mpsc::channel(1024);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let sub_id = "sub-capped".to_owned();
        subscriptions.lock().await.insert(
            sub_id.clone(),
            ActiveSubscription {
                credit: Arc::new(AtomicU64::new(100_000)),
                credit_notify: Arc::new(Notify::new()),
                cancel_tx,
            },
        );
        let (conn_stop_tx, conn_stop) = watch::channel(false);
        let _conn_stop_holder = conn_stop_tx;

        let source = RunEventsSource::start(&state, &json!({"runId": run_id}), &sub_id)
            .expect("run.events source starts");
        let pump = tokio::spawn(subscription_pump(
            subscriptions.clone(),
            state.clone(),
            Some("test-token".to_owned()),
            state.subscribe_auth_revocation(),
            ctrl_tx.clone(),
            sub_tx.clone(),
            sub_id.clone(),
            source,
            Arc::new(AtomicU64::new(100_000)),
            Arc::new(Notify::new()),
            cancel_rx,
            conn_stop,
        ));

        let mut events = 0usize;
        let mut completed = 0usize;
        let deadline = Duration::from_secs(5);
        while let Ok(message) = tokio::time::timeout(deadline, sub_rx.recv()).await {
            let Message::Text(txt) = message.expect("text frame") else {
                continue;
            };
            let frame: Value = serde_json::from_str(&txt).unwrap();
            if frame["method"] == "sub.event" {
                events += 1;
            } else if frame["method"] == "sub.end" {
                assert_eq!(frame["params"]["terminal"], "completed", "{frame}");
                completed += 1;
                break;
            }
        }
        // Whole-run events (run.queued / run.started / run.completed) plus
        // 300 deltas.
        assert_eq!(
            events, 303,
            "every event must be delivered before completed"
        );
        assert_eq!(completed, 1, "exactly one completed terminal");
        // No terminal may ever jump the event lane (ctrl must stay silent).
        while let Ok(message) = ctrl_rx.try_recv() {
            if let Message::Text(txt) = message {
                let frame: Value = serde_json::from_str(&txt).unwrap();
                assert_ne!(
                    frame["method"], "sub.end",
                    "terminal left the event lane: {frame}"
                );
            }
        }
        assert!(
            subscriptions.lock().await.get(&sub_id).is_none(),
            "the pump removed its subscription entry after the terminal"
        );
        let _ = pump.await;
    }

    /// P1-B (wp4fix review): a single event whose serialized `sub.event` frame
    /// exceeds the wire budget ends the subscription with EXACTLY ONE
    /// `sub.end(error)` — the pre-fix `break` only left the delivery loop, so the
    /// outer loop re-serialized the same oversized event and emitted one
    /// `sub.end(error)` per iteration forever (subscription entry leaked).
    #[tokio::test]
    async fn oversized_event_ends_the_subscription_exactly_once() {
        let tmp = TempDir::new("pump-oversized");
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
                "projectId": "proj-big",
                "name": "BigEvent",
                "environmentId": "local",
                "root": {"instanceId": iid, "rootId": "root-1", "relativePath": "workspace"}
            }))
            .unwrap();
        store
            .create_session(
                &json!({"instanceId": iid, "sessionId": "sess-big", "projectId": "proj-big"}),
            )
            .unwrap();
        let run_id = match store
            .submit_run(
                &json!({"instanceId": iid, "sessionId": "sess-big", "input": [{"kind": "text", "text": "oversized"}]}),
                Some("key-big-1"),
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
        // One event whose serialized sub.event frame exceeds the 1 MiB budget.
        store
            .append_run_progress_event(
                &run_id,
                "sess-big",
                "run.progress",
                &json!({"runId": run_id, "delta": "x".repeat(1_200_000)}),
            )
            .unwrap();

        let runtime = PublicV1Runtime::new(store);
        let state = Arc::new(PublicV1State::new(runtime, "test-token".to_owned()));
        let subscriptions: Arc<Mutex<HashMap<String, ActiveSubscription>>> = Arc::default();
        let (ctrl_tx, mut ctrl_rx) = mpsc::channel(16);
        let (sub_tx, mut sub_rx) = mpsc::channel(16);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let sub_id = "sub-oversized".to_owned();
        subscriptions.lock().await.insert(
            sub_id.clone(),
            ActiveSubscription {
                credit: Arc::new(AtomicU64::new(64)),
                credit_notify: Arc::new(Notify::new()),
                cancel_tx,
            },
        );
        let (conn_stop_tx, conn_stop) = watch::channel(false);
        let _conn_stop_holder = conn_stop_tx;

        let source = RunEventsSource::start(&state, &json!({"runId": run_id}), &sub_id)
            .expect("run.events source starts");
        let pump = tokio::spawn(subscription_pump(
            subscriptions.clone(),
            state.clone(),
            Some("test-token".to_owned()),
            state.subscribe_auth_revocation(),
            ctrl_tx.clone(),
            sub_tx.clone(),
            sub_id.clone(),
            source,
            Arc::new(AtomicU64::new(64)),
            Arc::new(Notify::new()),
            cancel_rx,
            conn_stop,
        ));
        drop(ctrl_tx);
        drop(sub_tx);

        // With the pre-fix busy loop the pump never exits; the deadline fails.
        tokio::time::timeout(Duration::from_secs(5), pump)
            .await
            .expect("the pump exits after ending the subscription")
            .unwrap();

        // wp4fix2 review P2: the error terminal rides the EVENT lane so it
        // cannot overtake this subscription's already-queued `sub.event`
        // frames, and its contract code is INPUT_TOO_LARGE per 007 §6.1.
        while let Ok(message) = ctrl_rx.try_recv() {
            if let Message::Text(txt) = message {
                let frame: Value = serde_json::from_str(&txt).unwrap();
                assert_ne!(
                    frame["method"], "sub.end",
                    "the oversized terminal must not jump the event lane: {frame}"
                );
            }
        }
        let mut sub_end_error = 0;
        while let Ok(message) = sub_rx.try_recv() {
            if let Message::Text(txt) = message {
                assert!(
                    txt.len() <= MAX_FRAME_BYTES,
                    "no oversized frame ever reaches the event lane ({} bytes)",
                    txt.len()
                );
                let frame: Value = serde_json::from_str(&txt).unwrap();
                if frame["method"] == "sub.end" {
                    assert_eq!(frame["params"]["terminal"], "error", "frame: {frame}");
                    assert_eq!(
                        frame["params"]["error"]["code"], "INPUT_TOO_LARGE",
                        "007 §6.1 fixes the code for oversized outbound events: {frame}"
                    );
                    sub_end_error += 1;
                }
            }
        }
        assert_eq!(
            sub_end_error, 1,
            "exactly one sub.end(error) for the oversized event"
        );
        assert!(
            subscriptions.lock().await.get(&sub_id).is_none(),
            "the subscription entry is removed"
        );
    }

    #[tokio::test]
    async fn handle_ws_text_frame_enforces_credential_initialize_version_inflight_and_credit_rules()
    {
        let tmp = TempDir::new("ws-frame-rules");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (file, token) = load_local_instance_and_token(&root_dir, None).unwrap();
        let meta = InstanceMeta {
            instance_id: file.instance_id.clone(),
            owner_id: file.owner_id.clone(),
            product: file.product.clone(),
            protocol_version: file.protocol_version.clone(),
            node_id: file.node_id.clone(),
            generation: "gen-test-ws".to_owned(),
            server_revision: "srv-test-ws".to_owned(),
            initialization_revision: file.initialization_revision.clone(),
        };
        let store = V1Store::open_and_recover(&root_dir, meta).unwrap();
        let runtime = PublicV1Runtime::new(store);
        let state = Arc::new(PublicV1State::new(runtime, token.clone()));
        let initialized = Arc::new(AtomicBool::new(false));
        let control_sem = Arc::new(Semaphore::new(MAX_CONTROL_PENDING));
        let normal_sem = Arc::new(Semaphore::new(MAX_PENDING));
        let in_flight_ids: Arc<StdMutex<HashSet<String>>> = Arc::default();
        let subscriptions: Arc<Mutex<HashMap<String, ActiveSubscription>>> = Arc::default();
        let (ctrl_tx, mut ctrl_rx) = mpsc::channel(16);
        let (norm_tx, _norm_rx) = mpsc::channel(16);
        let (sub_tx, _sub_rx) = mpsc::channel(16);
        let (_conn_stop_tx, conn_stop) = watch::channel(false);

        // 1) Missing session credential fails closed BEFORE JSON parsing.
        let close = handle_ws_text_frame(
            "{not json",
            &state,
            "conn-1",
            &None,
            &initialized,
            &control_sem,
            &normal_sem,
            &in_flight_ids,
            &subscriptions,
            &ctrl_tx,
            &norm_tx,
            &sub_tx,
            &conn_stop,
        )
        .await;
        assert!(close, "missing session credential must force close");
        let Message::Text(unauth_txt) = ctrl_rx.recv().await.unwrap() else {
            panic!("expected UNAUTHENTICATED text frame");
        };
        let unauth: Value = serde_json::from_str(&unauth_txt).unwrap();
        assert_eq!(unauth["error"]["data"]["code"], CODE_UNAUTHENTICATED);

        // 2) Unsupported abiVersion returns -32006 with data.supported.
        let bad_ver = json!({
            "jsonrpc": "2.0",
            "id": "init-bad-ver",
            "method": "initialize",
            "params": {
                "abi_version": 999,
                "api_version": OPERATION_VERSION,
                "client": {"name": "test", "version": "1.0"}
            }
        })
        .to_string();
        let close = handle_ws_text_frame(
            &bad_ver,
            &state,
            "conn-1",
            &Some(token.clone()),
            &initialized,
            &control_sem,
            &normal_sem,
            &in_flight_ids,
            &subscriptions,
            &ctrl_tx,
            &norm_tx,
            &sub_tx,
            &conn_stop,
        )
        .await;
        assert!(!close);
        let Message::Text(resp_txt) = ctrl_rx.recv().await.unwrap() else {
            panic!("expected text frame");
        };
        let resp: Value = serde_json::from_str(&resp_txt).unwrap();
        assert_eq!(resp["error"]["code"], ERR_UNSUPPORTED_VERSION);
        assert_eq!(resp["error"]["data"]["supported"], json!([ABI_VERSION]));

        // 3) initialize.credential mismatch against HTTP upgrade bearer forces disconnect.
        let bad_cred = json!({
            "jsonrpc": "2.0",
            "id": "init-bad-cred",
            "method": "initialize",
            "params": {
                "abi_version": ABI_VERSION,
                "api_version": OPERATION_VERSION,
                "credential": "wrong-credential",
                "client": {"name": "test", "version": "1.0"}
            }
        })
        .to_string();
        let close = handle_ws_text_frame(
            &bad_cred,
            &state,
            "conn-1",
            &Some(token.clone()),
            &initialized,
            &control_sem,
            &normal_sem,
            &in_flight_ids,
            &subscriptions,
            &ctrl_tx,
            &norm_tx,
            &sub_tx,
            &conn_stop,
        )
        .await;
        assert!(close, "mismatched initialize credential must force close");
        assert!(!initialized.load(Ordering::SeqCst));
        let Message::Text(resp_txt) = ctrl_rx.recv().await.unwrap() else {
            panic!("expected text frame");
        };
        let resp: Value = serde_json::from_str(&resp_txt).unwrap();
        assert_eq!(resp["error"]["data"]["code"], CODE_UNAUTHENTICATED);

        // 4) Valid initialize succeeds and filters features to GATEWAY_FEATURES intersection.
        let ok_init = json!({
            "jsonrpc": "2.0",
            "id": "init-ok",
            "method": "initialize",
            "params": {
                "abi_version": ABI_VERSION,
                "api_version": OPERATION_VERSION,
                "credential": token,
                "features": ["subscriptions", "nonexistent-feature", "heartbeat"],
                "client": {"name": "test", "version": "1.0"}
            }
        })
        .to_string();
        let close = handle_ws_text_frame(
            &ok_init,
            &state,
            "conn-1",
            &Some(token.clone()),
            &initialized,
            &control_sem,
            &normal_sem,
            &in_flight_ids,
            &subscriptions,
            &ctrl_tx,
            &norm_tx,
            &sub_tx,
            &conn_stop,
        )
        .await;
        assert!(!close);
        assert!(initialized.load(Ordering::SeqCst));
        let Message::Text(resp_txt) = ctrl_rx.recv().await.unwrap() else {
            panic!("expected text frame");
        };
        let resp: Value = serde_json::from_str(&resp_txt).unwrap();
        assert_eq!(
            resp["result"]["supported_features"],
            json!(["subscriptions", "heartbeat"])
        );

        // 5) Repeat initialize on an already initialized connection is rejected with -32600.
        let repeat_init = json!({
            "jsonrpc": "2.0",
            "id": "init-repeat",
            "method": "initialize",
            "params": {
                "abi_version": ABI_VERSION,
                "api_version": OPERATION_VERSION,
                "client": {"name": "test", "version": "1.0"}
            }
        })
        .to_string();
        let close = handle_ws_text_frame(
            &repeat_init,
            &state,
            "conn-1",
            &Some(token.clone()),
            &initialized,
            &control_sem,
            &normal_sem,
            &in_flight_ids,
            &subscriptions,
            &ctrl_tx,
            &norm_tx,
            &sub_tx,
            &conn_stop,
        )
        .await;
        assert!(!close);
        let Message::Text(resp_txt) = ctrl_rx.recv().await.unwrap() else {
            panic!("expected text frame");
        };
        let resp: Value = serde_json::from_str(&resp_txt).unwrap();
        assert_eq!(resp["error"]["code"], JSONRPC_INVALID_REQUEST);

        // 6) Duplicate in-flight requestId returns -32600 without affecting existing in-flight entry.
        in_flight_ids
            .lock()
            .expect("mutex")
            .insert("req-inflight".to_owned());
        let dup_req = json!({
            "jsonrpc": "2.0",
            "id": "req-inflight",
            "method": "system.capabilities",
            "params": {}
        })
        .to_string();
        let close = handle_ws_text_frame(
            &dup_req,
            &state,
            "conn-1",
            &Some(token.clone()),
            &initialized,
            &control_sem,
            &normal_sem,
            &in_flight_ids,
            &subscriptions,
            &ctrl_tx,
            &norm_tx,
            &sub_tx,
            &conn_stop,
        )
        .await;
        assert!(!close);
        assert!(
            in_flight_ids
                .lock()
                .expect("mutex")
                .contains("req-inflight")
        );
        let Message::Text(resp_txt) = ctrl_rx.recv().await.unwrap() else {
            panic!("expected text frame");
        };
        let resp: Value = serde_json::from_str(&resp_txt).unwrap();
        assert_eq!(resp["error"]["code"], JSONRPC_INVALID_REQUEST);

        // 7) Excessive sub.open credit is rejected with INVALID_ARGUMENT.
        let over_credit = json!({
            "jsonrpc": "2.0",
            "id": "sub-too-high",
            "method": "sub.open",
            "params": {
                "stream": "run.events",
                "input": {"runId": "missing"},
                "credit": MAX_SUBSCRIPTION_CREDIT + 1
            }
        })
        .to_string();
        let close = handle_ws_text_frame(
            &over_credit,
            &state,
            "conn-1",
            &Some(token),
            &initialized,
            &control_sem,
            &normal_sem,
            &in_flight_ids,
            &subscriptions,
            &ctrl_tx,
            &norm_tx,
            &sub_tx,
            &conn_stop,
        )
        .await;
        assert!(!close);
        let Message::Text(resp_txt) = ctrl_rx.recv().await.unwrap() else {
            panic!("expected text frame");
        };
        let resp: Value = serde_json::from_str(&resp_txt).unwrap();
        assert_eq!(resp["error"]["data"]["code"], CODE_INVALID_ARGUMENT);
    }
}
