//! `agentcpm-pd terminal attach` — the dedicated per-terminal process
//! (`007 §4`/`§6.7`, t015 PR-4).
//!
//! The attach process is a CLIENT of the Gateway, never a child of it: the
//! host (desktop/relay, or a human on their tty) starts one attach per
//! terminal. Its two segments:
//!
//! - attach <-> Gateway over `/v1/ws` through the ordinary `terminals.*`
//!   operations: `terminals.get` -> `terminals.control acquire` (the lease
//!   token lives only in this process's memory — never argv, logs, or host
//!   frames) -> `terminals.input`/`terminals.resize` with the allocated
//!   `inputSeq`, and a `terminals.output` subscription for the byte stream.
//!   `serverEpoch` is verified on every input (no replay across gateway
//!   restarts); a lost terminal reports through `term.exit`.
//! - host <-> attach over stdio `pd-connect/1`-shaped frames:
//!   host -> attach: `term.input {id, bytesBase64, inputSeq, controlEpoch}`
//!   (request/response: the ack carries `acceptedThroughSeq` and
//!   `deduplicated`), `term.resize {rows, columns}`;
//!   attach -> host: `term.output` (the `terminals.output` result frame
//!   verbatim — `bytes`/`gap`/`exit` types, cursors, `bytesBase64`),
//!   `term.exit {exitCode, exitSignal, state, cursor}`, and
//!   `conn.state {ready|reconnecting|failed}`.
//!
//! `--machine` speaks those JSON frames on stdio (NDJSON). Without it, a
//! human's tty is switched to raw mode: stdin bytes become `term.input`
//! chunks, `term.output` bytes are decoded to stdout, and SIGWINCH is
//! forwarded as `term.resize` (007 §4's "person at their own TTY" mode).

use std::io::Write as _;
use std::time::Duration;

use crate::file_transfer::{base64_decode, base64_encode};
use agentcpm_pd_protocol::errors::CODE_CURSOR_EXPIRED;
use agentcpm_pd_protocol::wire::MAX_FRAME_BYTES;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::mpsc;

use crate::error::ClientError;
use crate::ws_client::{PdWsClient, SubStreamItem};
use crate::{PdClient, PdClientOptions};

/// Human-mode input chunking: one `term.input` per read, bounded so the
/// serialized request stays far under the 1 MiB frame cap.
const RAW_INPUT_CHUNK: usize = 4096;

/// Lease renewal cadence: renew at ~1/3 of the 60 s TTL.
const RENEW_INTERVAL: Duration = Duration::from_secs(20);

/// Bounded reconnect attempts, following the client's backoff schedule.
const RECONNECT_ATTEMPTS: usize = 8;

/// Terminals-domain contract codes the reconnect reacts to. The protocol
/// crate exports only the shared set, so these mirror the gateway's
/// `terminals.rs` constants.
const CODE_CONTROL_CONFLICT: &str = "CONTROL_CONFLICT";
const CODE_LEASE_EXPIRED: &str = "LEASE_EXPIRED";

pub struct AttachOptions {
    pub target: Option<String>,
    pub no_start: bool,
    pub terminal_id: String,
    pub machine: bool,
}

enum HostFrame {
    /// A host request awaiting its ack.
    Input {
        id: Option<String>,
        bytes: Vec<u8>,
    },
    Resize {
        rows: u16,
        columns: u16,
    },
}

/// Runs the attach process to completion, returning the process exit code
/// (007 §6.8: 0 normal end including terminal exit, 1 runtime-fatal,
/// 130 SIGINT).
pub async fn run_attach(opts: AttachOptions) -> i32 {
    let mut stdout = std::io::stdout();
    let client = match PdClient::connect(PdClientOptions {
        home_override: None,
        target: opts.target.clone(),
        no_start: opts.no_start,
        spawner: None,
    })
    .await
    {
        Ok(client) => client,
        Err(err) => {
            let _ = write_conn_state(&mut stdout, opts.machine, "failed", &err.to_string());
            return 1;
        }
    };
    let ws = client.ws().clone();
    let mut session = match attach_session(ws, &opts.terminal_id).await {
        Ok(session) => session,
        Err(err) => {
            let _ = write_conn_state(&mut stdout, opts.machine, "failed", &err.to_string());
            return 1;
        }
    };
    let _ = write_conn_state(&mut stdout, opts.machine, "ready", "");

    // A terminal that already ended reports through the normal exit path.
    if session.subscription.is_none() {
        let terminal = fetch_terminal(&session.ws, &opts.terminal_id)
            .await
            .unwrap_or(Value::Null);
        let state = terminal.get("state").and_then(Value::as_str).unwrap_or("");
        let _ = write_term_exit(
            &mut stdout,
            opts.machine,
            &json!({
                "exitCode": terminal.get("exitCode").cloned().unwrap_or(Value::Null),
                "exitSignal": terminal.get("exitSignal").cloned().unwrap_or(Value::Null),
                "cursor": Value::Null,
            }),
            state.to_owned(),
        );
        return 0;
    }

    // Human tty mode: raw stdin, decoded stdout, SIGWINCH -> term.resize.
    let raw_guard = if opts.machine {
        None
    } else {
        RawTtyGuard::acquire().ok()
    };

    let (host_tx, mut host_rx) = mpsc::channel::<HostFrame>(64);
    let reader = spawn_stdin_reader(opts.machine, host_tx);

    let exit_code = run_until_exit(&client, &opts, &mut session, &mut host_rx, &mut stdout).await;
    if let Some(subscription) = session.subscription.take() {
        let _ = subscription.close().await;
    }
    let _ = stdout.flush();
    drop(raw_guard);
    // The reader parks in a blocking stdin read until EOF; with the host
    // still holding stdin open it never ends on its own, so joining it
    // unconditionally would hang the process after term.exit / SIGINT.
    // Dropping the receiver fails any parked send; abort cancels the read
    // itself (a reader that already saw EOF finished — abort is a no-op
    // then). Exit-code semantics are unchanged: 0 / 130 / 1 as before.
    drop(host_rx);
    reader.abort();
    let _ = reader.await;
    exit_code
}

/// One resolved event of the steady-state select. The select statement's
/// futures (which borrow the session's subscription) are dropped before
/// the handlers below run, so each handler may mutate the session freely.
#[allow(clippy::large_enum_variant)] // the largest variant is the output buffer
enum Step {
    CtrlC,
    Renew,
    Winch,
    Host(Option<HostFrame>),
    Sub(Option<SubStreamItem>),
}

/// The attach's steady state: forward host input, deliver output frames,
/// renew the lease, and finish on the terminal's exit frame, SIGINT, or an
/// unrecoverable failure.
async fn run_until_exit(
    client: &PdClient,
    opts: &AttachOptions,
    session: &mut AttachSession,
    host_rx: &mut mpsc::Receiver<HostFrame>,
    stdout: &mut std::io::Stdout,
) -> i32 {
    let machine = opts.machine;
    let mut renewal = tokio::time::interval(RENEW_INTERVAL);
    renewal.tick().await;
    // Human mode forwards the local tty's SIGWINCH as term.resize with the
    // tty's own size (unix-only file: the product targets macOS/Linux).
    let mut winch = if machine {
        None
    } else {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()).ok()
    };
    loop {
        let step = tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => Step::CtrlC,
            _ = async {
                match winch.as_mut() {
                    Some(stream) => stream.recv().await,
                    None => std::future::pending().await,
                }
            } => Step::Winch,
            _ = renewal.tick() => Step::Renew,
            frame = host_rx.recv() => Step::Host(frame),
            item = async {
                match session.subscription.as_mut() {
                    Some(subscription) => subscription.recv().await,
                    None => std::future::pending().await,
                }
            } => Step::Sub(item),
        };
        match step {
            Step::CtrlC => {
                // 007 §6.8: SIGINT detaches; the terminal keeps running.
                return 130;
            }
            Step::Winch => {
                if let Some((rows, columns)) = local_tty_size() {
                    let _ = send_resize(session, &opts.terminal_id, rows, columns).await;
                }
            }
            Step::Renew => {
                if renew_lease(&session.ws, &session.lease, &opts.terminal_id)
                    .await
                    .is_err()
                {
                    let _ =
                        write_conn_state(stdout, machine, "reconnecting", "lease renewal failed");
                    match reconnect(client, opts, session).await {
                        ReconnectOutcome::Retried => continue,
                        ReconnectOutcome::TerminalEnded => return 0,
                        ReconnectOutcome::Failed(reason) => {
                            let _ = write_conn_state(stdout, machine, "failed", &reason);
                            return 1;
                        }
                    }
                }
            }
            Step::Host(None) => return 0,
            Step::Host(Some(HostFrame::Input { id, bytes })) => {
                let seq = session.next_input_seq;
                match send_input(session, &opts.terminal_id, seq, &bytes).await {
                    Ok(ack) => {
                        session.next_input_seq = seq + 1;
                        if machine {
                            let _ = write_host_ack(stdout, id, &ack);
                        }
                    }
                    Err(err) => {
                        if machine {
                            let _ = write_host_error(stdout, id, &err);
                        }
                        let _ = write_conn_state(stdout, machine, "failed", &err.to_string());
                        return 1;
                    }
                }
            }
            Step::Host(Some(HostFrame::Resize { rows, columns })) => {
                if let Err(err) = send_resize(session, &opts.terminal_id, rows, columns).await {
                    if machine {
                        let _ = write_host_error(stdout, None, &err);
                    }
                    let _ = write_conn_state(stdout, machine, "failed", &err.to_string());
                    return 1;
                }
            }
            Step::Sub(Some(SubStreamItem::Event(ev))) => {
                let data = Value::Object(ev.event.data);
                // Every `terminals.output` frame carries `nextCursor` (bytes
                // advance it; gap/exit/heartbeat hold it at the stream
                // position), so the latest one seen is the reconnect's resume
                // point. Frames arrive in order, making it monotonic.
                if let Some(next) = data.get("nextCursor").filter(|v| !v.is_null()) {
                    session.output_cursor = Some(next.clone());
                }
                let kind = data.get("type").and_then(Value::as_str).unwrap_or("");
                if kind == "exit" {
                    let state = current_state(&session.ws, &opts.terminal_id).await;
                    let _ = write_term_exit(stdout, machine, &data, state);
                    return 0;
                }
                if !write_term_output(stdout, machine, &data) {
                    return 1;
                }
            }
            Step::Sub(terminal_frame) => {
                // `sub.end` or a closed stream: reconnect and resume
                // (server restart, slow consumer, or takeover); the exit
                // frame arrives on the new subscription if the terminal
                // ended meanwhile.
                let reason = match terminal_frame {
                    Some(SubStreamItem::End(_)) => "subscription ended",
                    None => "connection lost",
                    _ => unreachable!("event frames are handled above"),
                };
                let _ = write_conn_state(stdout, machine, "reconnecting", reason);
                match reconnect(client, opts, session).await {
                    ReconnectOutcome::Retried => continue,
                    ReconnectOutcome::TerminalEnded => return 0,
                    ReconnectOutcome::Failed(reason) => {
                        let _ = write_conn_state(stdout, machine, "failed", &reason);
                        return 1;
                    }
                }
            }
        }
    }
}

/// The local tty's window size, for forwarding SIGWINCH as term.resize.
fn local_tty_size() -> Option<(u16, u16)> {
    use std::os::fd::AsFd;
    let winsize = rustix::termios::tcgetwinsize(std::io::stdin().as_fd()).ok()?;
    Some((winsize.ws_row, winsize.ws_col))
}

// ---------------------------------------------------------------------------
// Gateway segment
// ---------------------------------------------------------------------------

struct AttachSession {
    ws: PdWsClient,
    lease: LeaseState,
    subscription: Option<crate::ws_client::PdWsSubscription>,
    next_input_seq: i64,
    /// The last `nextCursor` delivered on the output subscription. A
    /// reconnect's `sub.open` passes it as the `terminals.output` `cursor`
    /// so the new stream continues where the old one stopped instead of
    /// replaying the retained window from its start. `None` until the
    /// first frame arrives, and reset when the ring evicted past it.
    output_cursor: Option<Value>,
}

#[derive(Clone)]
struct LeaseState {
    control_epoch: i64,
    token: String,
    server_epoch: String,
}

/// `terminals.get` -> `terminals.control acquire` -> output subscription.
async fn attach_session(ws: PdWsClient, terminal_id: &str) -> Result<AttachSession, ClientError> {
    let terminal = fetch_terminal(&ws, terminal_id).await?;
    let state = terminal.get("state").and_then(Value::as_str).unwrap_or("");
    if matches!(state, "exited" | "failed" | "lost") {
        // Already ended: report through the normal exit path.
        return Ok(AttachSession {
            ws,
            lease: LeaseState {
                control_epoch: -1,
                token: String::new(),
                server_epoch: terminal
                    .get("serverEpoch")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            },
            subscription: None,
            next_input_seq: 0,
            output_cursor: None,
        });
    }
    let expected = terminal
        .get("controlEpoch")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let (lease, watermark) = acquire_lease(&ws, terminal_id, expected).await?;
    let subscription = ws
        .open_subscription(
            "terminals.output",
            json!({"terminalId": terminal_id, "follow": true}),
            Some(32),
            true,
        )
        .await?;
    Ok(AttachSession {
        ws,
        lease,
        subscription: Some(subscription),
        next_input_seq: watermark + 1,
        output_cursor: None,
    })
}

async fn fetch_terminal(ws: &PdWsClient, terminal_id: &str) -> Result<Value, ClientError> {
    let response = ws
        .call_operation(
            "terminals.get",
            json!({"terminalId": terminal_id}),
            None,
            None,
            None,
        )
        .await?;
    Ok(response.result.unwrap_or(Value::Null)["terminal"].clone())
}

async fn acquire_lease(
    ws: &PdWsClient,
    terminal_id: &str,
    expected_control_epoch: i64,
) -> Result<(LeaseState, i64), ClientError> {
    let response = ws
        .call_operation(
            "terminals.control",
            json!({
                "terminalId": terminal_id,
                "action": "acquire",
                "expectedControlEpoch": expected_control_epoch,
            }),
            None,
            None,
            None,
        )
        .await?;
    let result = response.result.unwrap_or(Value::Null);
    let lease = &result["lease"];
    Ok((
        LeaseState {
            control_epoch: lease["controlEpoch"].as_i64().unwrap_or(0),
            token: lease["leaseToken"].as_str().unwrap_or_default().to_owned(),
            server_epoch: result["terminal"]["serverEpoch"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        },
        lease["lastAcceptedInputSeq"].as_i64().unwrap_or(0),
    ))
}

async fn send_input(
    session: &mut AttachSession,
    terminal_id: &str,
    input_seq: i64,
    bytes: &[u8],
) -> Result<Value, ClientError> {
    let response = session
        .ws
        .call_operation(
            "terminals.input",
            json!({
                "terminalId": terminal_id,
                "serverEpoch": session.lease.server_epoch,
                "controlEpoch": session.lease.control_epoch,
                "leaseToken": session.lease.token,
                "inputSeq": input_seq,
                "bytesBase64": base64_encode(bytes),
            }),
            None,
            None,
            None,
        )
        .await?;
    Ok(response.result.unwrap_or(Value::Null))
}

async fn send_resize(
    session: &mut AttachSession,
    terminal_id: &str,
    rows: u16,
    columns: u16,
) -> Result<(), ClientError> {
    session
        .ws
        .call_operation(
            "terminals.resize",
            json!({
                "terminalId": terminal_id,
                "controlEpoch": session.lease.control_epoch,
                "leaseToken": session.lease.token,
                "rows": rows,
                "columns": columns,
            }),
            None,
            None,
            None,
        )
        .await?;
    Ok(())
}

/// Renews the held lease over `ws` — the steady-state timer's connection
/// during normal operation, or a freshly reconnected one during
/// [`reconnect`]. The result carries the same `lease.lastAcceptedInputSeq`
/// watermark an acquire does, so a reconnect can re-sync the input
/// sequence.
async fn renew_lease(
    ws: &PdWsClient,
    lease: &LeaseState,
    terminal_id: &str,
) -> Result<i64, ClientError> {
    let response = ws
        .call_operation(
            "terminals.control",
            json!({
                "terminalId": terminal_id,
                "action": "renew",
                "expectedControlEpoch": lease.control_epoch,
                "leaseToken": lease.token,
            }),
            None,
            None,
            None,
        )
        .await?;
    let result = response.result.unwrap_or(Value::Null);
    Ok(result["lease"]["lastAcceptedInputSeq"]
        .as_i64()
        .unwrap_or(0))
}

async fn current_state(ws: &PdWsClient, terminal_id: &str) -> String {
    match fetch_terminal(ws, terminal_id).await {
        Ok(terminal) => terminal
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
        Err(_) => "unknown".to_owned(),
    }
}

enum ReconnectOutcome {
    Retried,
    TerminalEnded,
    Failed(String),
}

/// What a failed renew of the held lease says about it. The gateway
/// answers `LEASE_EXPIRED` only when the stored lease died on its own
/// (nobody took anything from us — a fresh acquire is allowed), and
/// `CONTROL_CONFLICT` when the control epoch or the token changed (a
/// takeover — the held token can never become valid again). Anything
/// else — transport, storage, timeout — says nothing about the lease, so
/// the attempt just retries later.
enum RenewVerdict {
    Dead,
    TakenOver,
    Unknown,
}

fn renew_verdict(err: &ClientError) -> RenewVerdict {
    match err {
        ClientError::Contract(c) if c.code == CODE_LEASE_EXPIRED => RenewVerdict::Dead,
        ClientError::Contract(c) if c.code == CODE_CONTROL_CONFLICT => RenewVerdict::TakenOver,
        _ => RenewVerdict::Unknown,
    }
}

async fn reconnect(
    client: &PdClient,
    opts: &AttachOptions,
    session: &mut AttachSession,
) -> ReconnectOutcome {
    for attempt in 0..RECONNECT_ATTEMPTS {
        if attempt > 0 {
            let wait = crate::ws_client::PdWsClient::backoff_schedule_ms()
                [attempt.min(crate::ws_client::PdWsClient::backoff_schedule_ms().len() - 1)];
            tokio::time::sleep(Duration::from_millis(wait)).await;
        }
        let Ok(ws) = reconnect_ws(client, opts.target.as_deref()).await else {
            continue;
        };
        let terminal = match fetch_terminal(&ws, &opts.terminal_id).await {
            Ok(terminal) => terminal,
            Err(_) => continue,
        };
        let state = terminal.get("state").and_then(Value::as_str).unwrap_or("");
        if matches!(state, "exited" | "failed" | "lost") {
            return ReconnectOutcome::TerminalEnded;
        }
        // The held lease is almost always still alive (the renewal timer
        // kept it fresh until the connection dropped), so renew it on the
        // fresh connection first: acquiring here would collide with our
        // own lease (`CONTROL_CONFLICT`) and be misread as a takeover.
        let watermark = match renew_lease(&ws, &session.lease, &opts.terminal_id).await {
            Ok(watermark) => watermark,
            Err(err) => match renew_verdict(&err) {
                // The held lease died while we were away: nothing was
                // taken from us, so a fresh acquire is allowed.
                RenewVerdict::Dead => {
                    let expected = terminal
                        .get("controlEpoch")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    match acquire_lease(&ws, &opts.terminal_id, expected).await {
                        Ok((lease, watermark)) => {
                            // Adopt the fresh lease before the
                            // subscription open below: if that open fails
                            // (ring eviction, transport), the next attempt
                            // renews THIS lease instead of acquiring
                            // again — which would collide with it.
                            session.lease = lease;
                            watermark
                        }
                        // A conflict here is genuine: the renew above
                        // already proved the held token dead, so this
                        // lease belongs to another controller.
                        Err(err) => {
                            if matches!(&err, ClientError::Contract(c) if c.code == CODE_CONTROL_CONFLICT)
                            {
                                return ReconnectOutcome::Failed(
                                    "another controller took the terminal over".to_owned(),
                                );
                            }
                            continue;
                        }
                    }
                }
                RenewVerdict::TakenOver => {
                    return ReconnectOutcome::Failed(
                        "another controller took the terminal over".to_owned(),
                    );
                }
                // The failure says nothing about the lease; retry later.
                RenewVerdict::Unknown => continue,
            },
        };
        match resume_subscription(session, ws, &opts.terminal_id, watermark).await {
            Ok(()) => return ReconnectOutcome::Retried,
            Err(err) => {
                // The ring evicted past the saved cursor while we were
                // away. The server's recovery hint is to re-open from the
                // earliest retained cursor, which a cursorless open does;
                // the bytes in between are gone (a gap, not a duplicate).
                if matches!(&err, ClientError::Contract(c) if c.code == CODE_CURSOR_EXPIRED) {
                    session.output_cursor = None;
                }
                continue;
            }
        }
    }
    ReconnectOutcome::Failed("reconnect attempts exhausted".to_owned())
}

/// Re-opens the output subscription on a fresh connection and, on
/// success, moves that connection into the session. `watermark` is the
/// active lease's `lastAcceptedInputSeq` (renewed or freshly acquired);
/// input resumes right after it.
async fn resume_subscription(
    session: &mut AttachSession,
    ws: PdWsClient,
    terminal_id: &str,
    watermark: i64,
) -> Result<(), ClientError> {
    // Resume from the last delivered `nextCursor` so the re-opened stream
    // continues where the old one stopped; without a cursor the server
    // replays the whole retained window (duplicating already-delivered
    // bytes).
    let mut sub_input = json!({"terminalId": terminal_id, "follow": true});
    if let Some(cursor) = &session.output_cursor {
        sub_input["cursor"] = cursor.clone();
    }
    let new_sub = ws
        .open_subscription("terminals.output", sub_input, Some(32), true)
        .await?;
    session.ws = ws;
    session.next_input_seq = watermark + 1;
    if let Some(old) = session.subscription.replace(new_sub) {
        let _ = old.close().await;
    }
    Ok(())
}

/// Re-uses the client's resolved home to build a fresh ws session for the
/// same target (the old session died with the connection).
async fn reconnect_ws(client: &PdClient, target: Option<&str>) -> Result<PdWsClient, ClientError> {
    let opts = PdClientOptions {
        home_override: Some(client.home().clone()),
        target: target.map(ToOwned::to_owned),
        no_start: true,
        spawner: None,
    };
    let client = PdClient::connect(opts).await?;
    Ok(client.ws().clone())
}

// ---------------------------------------------------------------------------
// Host segment (stdio frames)
// ---------------------------------------------------------------------------

fn write_line(machine: bool, stdout: &mut std::io::Stdout, frame: &Value) -> std::io::Result<()> {
    if machine {
        let line = serde_json::to_string(frame).unwrap_or_default();
        writeln!(stdout, "{line}")
    } else {
        Ok(())
    }
}

fn write_conn_state(
    stdout: &mut std::io::Stdout,
    machine: bool,
    state: &str,
    reason: &str,
) -> std::io::Result<()> {
    if !machine {
        if !reason.is_empty() {
            eprintln!("agentcpm-pd terminal attach: {state}: {reason}");
        }
        return Ok(());
    }
    let mut params = json!({"state": state});
    if !reason.is_empty() {
        params["reason"] = json!(reason);
    }
    write_line(
        machine,
        stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "conn.state",
            "params": params,
        }),
    )
}

/// Forwards one `terminals.output` result frame verbatim as `term.output`.
/// Human mode decodes the bytes to stdout instead.
fn write_term_output(stdout: &mut std::io::Stdout, machine: bool, data: &Value) -> bool {
    if machine {
        let frame = json!({
            "jsonrpc": "2.0",
            "method": "term.output",
            "params": data,
        });
        let Ok(line) = serde_json::to_string(&frame) else {
            return true;
        };
        return writeln!(stdout, "{line}").is_ok();
    }
    let Some(encoded) = data.get("bytesBase64").and_then(Value::as_str) else {
        return true;
    };
    match base64_decode(encoded) {
        Ok(bytes) => stdout.write_all(&bytes).is_ok() && stdout.flush().is_ok(),
        Err(_) => true,
    }
}

fn write_term_exit(
    stdout: &mut std::io::Stdout,
    machine: bool,
    data: &Value,
    state: String,
) -> std::io::Result<()> {
    let params = json!({
        "exitCode": data.get("exitCode").cloned().unwrap_or(Value::Null),
        "exitSignal": data.get("exitSignal").cloned().unwrap_or(Value::Null),
        "state": state,
        "cursor": data.get("cursor").cloned().unwrap_or(Value::Null),
    });
    write_line(
        machine,
        stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "term.exit",
            "params": params,
        }),
    )
}

fn write_host_ack(
    stdout: &mut std::io::Stdout,
    id: Option<String>,
    ack: &Value,
) -> std::io::Result<()> {
    let Some(id) = id else {
        return Ok(());
    };
    write_line(
        true,
        stdout,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": ack,
        }),
    )
}

fn write_host_error(
    stdout: &mut std::io::Stdout,
    id: Option<String>,
    err: &ClientError,
) -> std::io::Result<()> {
    let Some(id) = id else {
        return Ok(());
    };
    let message = err.to_string();
    write_line(
        true,
        stdout,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32000, "message": message},
        }),
    )
}

/// Reads host input: NDJSON `term.input`/`term.resize` frames in machine
/// mode, raw bytes (chunked) in human mode.
fn spawn_stdin_reader(
    machine: bool,
    host_tx: mpsc::Sender<HostFrame>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if machine {
            let stdin = tokio::io::stdin();
            let mut lines = BufReader::new(stdin).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                if line.len() > MAX_FRAME_BYTES {
                    continue;
                }
                let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
                match method {
                    "term.input" => {
                        let Some(encoded) = frame["params"]["bytesBase64"]
                            .as_str()
                            .map(ToOwned::to_owned)
                        else {
                            continue;
                        };
                        let Ok(bytes) = base64_decode(&encoded) else {
                            continue;
                        };
                        let id = frame
                            .get("id")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned);
                        if host_tx.send(HostFrame::Input { id, bytes }).await.is_err() {
                            break;
                        }
                    }
                    "term.resize" => {
                        let (Some(rows), Some(columns)) = (
                            frame["params"]["rows"].as_u64(),
                            frame["params"]["columns"].as_u64(),
                        ) else {
                            continue;
                        };
                        if host_tx
                            .send(HostFrame::Resize {
                                rows: u16::try_from(rows.min(u64::from(u16::MAX))).unwrap_or(24),
                                columns: u16::try_from(columns.min(u64::from(u16::MAX)))
                                    .unwrap_or(80),
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        } else {
            let mut stdin = tokio::io::stdin();
            let mut chunk = vec![0u8; RAW_INPUT_CHUNK];
            loop {
                match stdin.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if host_tx
                            .send(HostFrame::Input {
                                id: None,
                                bytes: chunk[..n].to_vec(),
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Human tty mode
// ---------------------------------------------------------------------------

/// Switches the controlling tty to raw mode for the attach's lifetime,
/// restoring the original line discipline on drop. rustix termios only —
/// no unsafe (the workspace forbids it).
struct RawTtyGuard {
    saved: Option<rustix::termios::Termios>,
}

impl RawTtyGuard {
    fn acquire() -> Result<Self, String> {
        use std::os::fd::AsFd;
        let stdin = std::io::stdin();
        let fd = stdin.as_fd();
        let original = rustix::termios::tcgetattr(fd).map_err(|e| e.to_string())?;
        let mut raw = original.clone();
        raw.make_raw();
        rustix::termios::tcsetattr(fd, rustix::termios::OptionalActions::Drain, &raw)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            saved: Some(original),
        })
    }
}

impl Drop for RawTtyGuard {
    fn drop(&mut self) {
        if let Some(saved) = self.saved.take() {
            use std::os::fd::AsFd;
            let _ = rustix::termios::tcsetattr(
                std::io::stdin().as_fd(),
                rustix::termios::OptionalActions::Drain,
                &saved,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — reconnect lease handling against an in-process gateway double
// (rb7 review P1: the reconnect must reuse the attach's own still-live
// lease instead of colliding with it through acquire).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use agentcpm_pd_protocol::contract::{OPERATION_VERSION, ResponseOutcome};
    use agentcpm_pd_protocol::errors::{ContractError, ERR_APPLICATION, EffectStatus, RetrySpec};
    use agentcpm_pd_protocol::wire::{
        ABI_VERSION, InitializeResult, JsonRpcRequest, JsonRpcResponse, OperationRpcResult,
        ProtocolLimits, TargetHandshakeInfo, WIRE_INITIALIZE,
    };
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message;

    use crate::config::{
        LocalInstanceMetadata, ReadyFile, atomic_write_file, ensure_secure_dir,
        local_metadata_path, local_token_path, ready_path,
    };

    const TERMINAL_ID: &str = "term-rb7";
    const INSTANCE_ID: &str = "local-019a0000-0000-7000-8000-0000000000a1";

    // -- Harness ---------------------------------------------------------------

    struct TempHome(PathBuf);

    impl TempHome {
        /// A throwaway `~/.pilotdeck`-shaped home whose ready file points
        /// at the mock gateway, so `PdClient::connect` (and `reconnect`'s
        /// re-connects) attach to it without spawning anything.
        fn new(tag: &str, ws_url: &str) -> Self {
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "agentcpm-pd-attach-{tag}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            ensure_secure_dir(&dir).unwrap();
            let meta = LocalInstanceMetadata {
                instance_id: INSTANCE_ID.to_owned(),
                owner_id: "owner-local".to_owned(),
                product: "agentcpm-pd".to_owned(),
                protocol_version: OPERATION_VERSION.to_owned(),
                node_id: "local".to_owned(),
                initialized_at: "2026-11-20T00:00:00Z".to_owned(),
                initialization_revision: "init-1".to_owned(),
            };
            atomic_write_file(
                &local_metadata_path(&dir),
                &serde_json::to_vec_pretty(&meta).unwrap(),
                false,
            )
            .unwrap();
            atomic_write_file(&local_token_path(&dir), b"secret-local-token", true).unwrap();
            let ready = ReadyFile {
                pid: std::process::id(),
                instance_id: INSTANCE_ID.to_owned(),
                generation: "gen-1".to_owned(),
                protocol_version: OPERATION_VERSION.to_owned(),
                ws_url: ws_url.to_owned(),
                http_url: None,
                started_at: "2026-11-20T00:00:00Z".to_owned(),
                server_revision: "rev-1".to_owned(),
            };
            atomic_write_file(
                &ready_path(&dir),
                &serde_json::to_vec_pretty(&ready).unwrap(),
                false,
            )
            .unwrap();
            Self(dir)
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The lease columns of the gateway's terminal row that `reconnect`
    /// interacts with.
    struct MockLease {
        control_epoch: i64,
        token: Option<String>,
        alive: bool,
    }

    /// A minimal in-process gateway double speaking the same wire shapes
    /// as the real one: `initialize`, `terminals.get`, `terminals.control`
    /// with the gateway's epoch -> liveness -> token check order, and
    /// `sub.open`. Every request is recorded for assertions.
    struct MockGateway {
        lease: Mutex<MockLease>,
        /// (method, operation/stream input) pairs in arrival order.
        log: Mutex<Vec<(String, Value)>>,
        /// How many leading `sub.open` calls fail with CURSOR_EXPIRED
        /// (the output ring evicting past the resume cursor).
        cursor_expired_opens: usize,
        opens: AtomicUsize,
    }

    fn terminal_json(lease: &MockLease) -> Value {
        json!({
            "terminalId": TERMINAL_ID,
            "projectId": "proj-1",
            "serverEpoch": "gen-1",
            "state": "running",
            "controlEpoch": lease.control_epoch,
            "lastAcceptedInputSeq": 0,
            "outputEpoch": "oe-1",
            "earliestOutputOffset": 0,
            "latestOutputOffset": 0,
            "rows": 24,
            "columns": 80,
        })
    }

    fn lease_json(lease: &MockLease, token: &str) -> Value {
        json!({
            "terminalId": TERMINAL_ID,
            "controlEpoch": lease.control_epoch,
            "leaseToken": token,
            "expiresAt": "2026-11-20T00:01:00Z",
            "lastAcceptedInputSeq": 0,
            "previousAcceptedWatermark": 0,
        })
    }

    impl MockGateway {
        fn ok_result(req: &JsonRpcRequest, result: Value) -> JsonRpcResponse {
            JsonRpcResponse::ok(
                req.id.clone(),
                serde_json::to_value(OperationRpcResult {
                    outcome: ResponseOutcome::Succeeded,
                    result,
                })
                .unwrap(),
            )
        }

        fn err_response(req: &JsonRpcRequest, code: &str, message: &str) -> JsonRpcResponse {
            JsonRpcResponse::err(
                Some(req.id.clone()),
                ERR_APPLICATION,
                message,
                Some(json!({"code": code, "message": message})),
            )
        }

        fn control(&self, req: &JsonRpcRequest, input: &Value) -> JsonRpcResponse {
            let action = input["action"].as_str().unwrap_or_default().to_owned();
            let expected = input["expectedControlEpoch"].as_i64().unwrap_or(-1);
            let token = input["leaseToken"].as_str().map(ToOwned::to_owned);
            let mut lease = self.lease.lock().unwrap();
            // Mirrors the gateway's `terminal_control_lease`: acquire
            // conflicts with any live lease; renew checks the control
            // epoch, then lease liveness, then the token.
            let conflict: Option<(&str, String)> = match action.as_str() {
                "acquire" => {
                    if lease.alive {
                        Some((
                            CODE_CONTROL_CONFLICT,
                            "terminal already has an active controller; use takeover".to_owned(),
                        ))
                    } else {
                        lease.control_epoch += 1;
                        lease.token = Some(format!("tok-{}", lease.control_epoch));
                        lease.alive = true;
                        None
                    }
                }
                "renew" => {
                    if lease.control_epoch != expected {
                        Some((
                            CODE_CONTROL_CONFLICT,
                            format!(
                                "expectedControlEpoch `{expected}` does not match `{}`",
                                lease.control_epoch
                            ),
                        ))
                    } else if !lease.alive {
                        Some((
                            CODE_LEASE_EXPIRED,
                            "lease expired; acquire again".to_owned(),
                        ))
                    } else if lease.token.as_deref() != token.as_deref() {
                        Some((
                            CODE_CONTROL_CONFLICT,
                            "leaseToken does not match the active controller".to_owned(),
                        ))
                    } else {
                        None
                    }
                }
                other => Some(("INVALID_ARGUMENT", format!("unsupported action `{other}`"))),
            };
            if let Some((code, message)) = conflict {
                return Self::err_response(req, code, &message);
            }
            let token = lease.token.clone().unwrap_or_default();
            Self::ok_result(
                req,
                json!({
                    "terminal": terminal_json(&lease),
                    "lease": lease_json(&lease, &token),
                }),
            )
        }

        fn sub_open(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
            let opened = self.opens.fetch_add(1, Ordering::SeqCst) + 1;
            if opened <= self.cursor_expired_opens {
                return Self::err_response(req, CODE_CURSOR_EXPIRED, "requested cursor expired");
            }
            JsonRpcResponse::ok(
                req.id.clone(),
                json!({"subscriptionId": format!("sub-{opened}")}),
            )
        }

        fn handle(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
            // Operation calls wrap their input in params.input; sub.open
            // nests the stream input under the same key.
            let input = req.params.get("input").cloned().unwrap_or(Value::Null);
            self.log.lock().unwrap().push((req.method.clone(), input));
            match req.method.as_str() {
                WIRE_INITIALIZE => JsonRpcResponse::ok(
                    req.id.clone(),
                    serde_json::to_value(InitializeResult {
                        abi_version: ABI_VERSION,
                        supported: true,
                        api_version: OPERATION_VERSION.to_owned(),
                        supported_features: vec!["subscriptions".to_owned()],
                        connection_id: "conn-mock-1".to_owned(),
                        cli: None,
                        target: TargetHandshakeInfo {
                            name: "local".to_owned(),
                            instance_id: INSTANCE_ID.to_owned(),
                            node_id: "local".to_owned(),
                            gateway_version: "0.1.0".to_owned(),
                            schema_digest: "deadbeef".to_owned(),
                            transport: "ws".to_owned(),
                        },
                        limits: ProtocolLimits::default(),
                    })
                    .unwrap(),
                ),
                "terminals.get" => {
                    let lease = self.lease.lock().unwrap();
                    Self::ok_result(req, json!({"terminal": terminal_json(&lease)}))
                }
                "terminals.control" => {
                    let input = req.params.get("input").cloned().unwrap_or(Value::Null);
                    self.control(req, &input)
                }
                "sub.open" => self.sub_open(req),
                _ => Self::ok_result(req, json!({})),
            }
        }
    }

    async fn spawn_mock_gateway(
        lease: MockLease,
        cursor_expired_opens: usize,
    ) -> (String, Arc<MockGateway>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let gateway = Arc::new(MockGateway {
            lease: Mutex::new(lease),
            log: Mutex::new(Vec::new()),
            cursor_expired_opens,
            opens: AtomicUsize::new(0),
        });
        let g = gateway.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let g = g.clone();
                tokio::spawn(async move {
                    // `check_ready_live` probes TCP reachability without a
                    // websocket handshake; those probes just close again.
                    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    while let Some(Ok(Message::Text(txt))) = ws.next().await {
                        let Ok(req) = serde_json::from_str::<JsonRpcRequest>(&txt) else {
                            continue;
                        };
                        let frame = g.handle(&req);
                        let Ok(line) = serde_json::to_string(&frame) else {
                            continue;
                        };
                        if ws.send(Message::Text(line.into())).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        (format!("ws://{addr}/v1/ws"), gateway)
    }

    struct Fixture {
        _home: TempHome,
        client: PdClient,
        opts: AttachOptions,
        gateway: Arc<MockGateway>,
    }

    /// A connected client plus an attach session holding `held` against
    /// the mock's lease state.
    async fn attach_fixture(
        tag: &str,
        lease: MockLease,
        held: LeaseState,
        output_cursor: Option<Value>,
        cursor_expired_opens: usize,
    ) -> (Fixture, AttachSession) {
        let (ws_url, gateway) = spawn_mock_gateway(lease, cursor_expired_opens).await;
        let home = TempHome::new(tag, &ws_url);
        let client = PdClient::connect(PdClientOptions {
            home_override: Some(home.0.clone()),
            target: None,
            no_start: true,
            spawner: None,
        })
        .await
        .expect("connects to the mock gateway through the seeded ready file");
        let session = AttachSession {
            ws: client.ws().clone(),
            lease: held,
            subscription: None,
            next_input_seq: 1,
            output_cursor,
        };
        let opts = AttachOptions {
            target: None,
            no_start: true,
            terminal_id: TERMINAL_ID.to_owned(),
            machine: true,
        };
        (
            Fixture {
                _home: home,
                client,
                opts,
                gateway,
            },
            session,
        )
    }

    fn held_lease(control_epoch: i64, token: &str) -> LeaseState {
        LeaseState {
            control_epoch,
            token: token.to_owned(),
            server_epoch: "gen-1".to_owned(),
        }
    }

    fn control_actions(log: &[(String, Value)]) -> Vec<String> {
        log.iter()
            .filter(|(method, _)| method == "terminals.control")
            .map(|(_, input)| input["action"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    fn sub_open_inputs(log: &[(String, Value)]) -> Vec<Value> {
        log.iter()
            .filter(|(method, _)| method == "sub.open")
            .map(|(_, input)| input.clone())
            .collect()
    }

    // -- Tests -----------------------------------------------------------------

    /// The P1 itself: a reconnect while the attach's own lease is still
    /// alive (renewal kept it fresh) must renew that lease and re-open the
    /// subscription — acquiring would collide with our own lease and be
    /// misread as a takeover.
    #[tokio::test]
    async fn reconnect_renews_the_still_live_own_lease_instead_of_acquiring() {
        let (fx, mut session) = attach_fixture(
            "own-live-lease",
            MockLease {
                control_epoch: 1,
                token: Some("tok-1".to_owned()),
                alive: true,
            },
            held_lease(1, "tok-1"),
            Some(json!({"outputEpoch": "oe-1", "byteOffset": 5})),
            0,
        )
        .await;

        let outcome = reconnect(&fx.client, &fx.opts, &mut session).await;

        assert!(matches!(outcome, ReconnectOutcome::Retried));
        assert!(session.subscription.is_some());
        assert_eq!(session.lease.token, "tok-1");
        let log = fx.gateway.log.lock().unwrap().clone();
        let renewals: Vec<&Value> = log
            .iter()
            .filter(|(m, i)| m == "terminals.control" && i["action"] == "renew")
            .map(|(_, i)| i)
            .collect();
        assert_eq!(renewals.len(), 1, "exactly one renew, got log: {log:?}");
        assert_eq!(renewals[0]["leaseToken"], json!("tok-1"));
        assert_eq!(renewals[0]["expectedControlEpoch"], json!(1));
        assert!(
            !log.iter()
                .any(|(m, i)| m == "terminals.control" && i["action"] == "acquire"),
            "renewing the own live lease must skip acquire: {log:?}"
        );
        // The re-opened stream resumes at the saved cursor.
        let opens = sub_open_inputs(&log);
        assert_eq!(opens.len(), 1);
        assert_eq!(
            opens[0]["cursor"],
            json!({"outputEpoch": "oe-1", "byteOffset": 5})
        );
    }

    /// A dead held lease falls through to a fresh acquire and resumes.
    #[tokio::test]
    async fn reconnect_after_lease_expiry_acquires_a_fresh_lease() {
        let (fx, mut session) = attach_fixture(
            "lease-expired",
            MockLease {
                control_epoch: 1,
                token: None,
                alive: false,
            },
            held_lease(1, "tok-1"),
            None,
            0,
        )
        .await;

        // Distinguish the watermark re-sync from the fixture's default.
        session.next_input_seq = 7;
        let outcome = reconnect(&fx.client, &fx.opts, &mut session).await;

        assert!(matches!(outcome, ReconnectOutcome::Retried));
        assert_eq!(session.lease.control_epoch, 2);
        assert_eq!(session.lease.token, "tok-2");
        assert_eq!(
            session.next_input_seq, 1,
            "input resumes after the acquire watermark"
        );
        let log = fx.gateway.log.lock().unwrap().clone();
        assert_eq!(
            control_actions(&log),
            vec!["renew".to_owned(), "acquire".to_owned()],
            "the expired lease is renewed first, then acquired: {log:?}"
        );
    }

    /// A renew that conflicts on the token is a takeover: fail with the
    /// explicit conflict reason, without a pointless acquire.
    #[tokio::test]
    async fn reconnect_reports_takeover_when_the_held_token_no_longer_matches() {
        let (fx, mut session) = attach_fixture(
            "takeover",
            MockLease {
                control_epoch: 1,
                token: Some("tok-other".to_owned()),
                alive: true,
            },
            held_lease(1, "tok-1"),
            None,
            0,
        )
        .await;

        let outcome = reconnect(&fx.client, &fx.opts, &mut session).await;

        assert!(matches!(&outcome, ReconnectOutcome::Failed(reason)
                if reason == "another controller took the terminal over"));
        let log = fx.gateway.log.lock().unwrap().clone();
        assert!(
            !log.iter()
                .any(|(m, i)| m == "terminals.control" && i["action"] == "acquire"),
            "a renew conflict is decided without acquire: {log:?}"
        );
    }

    /// The CURSOR_EXPIRED recovery loop: the first attempt acquires a
    /// fresh lease, its sub.open is rejected, and the retry must renew the
    /// lease acquired this round — acquiring again would collide with it
    /// (the pre-fix bug made this recovery fail 100% of the time).
    #[tokio::test]
    async fn cursor_expired_retry_renews_the_lease_acquired_this_round() {
        let (fx, mut session) = attach_fixture(
            "cursor-expired",
            MockLease {
                control_epoch: 1,
                token: None,
                alive: false,
            },
            held_lease(1, "tok-1"),
            Some(json!({"outputEpoch": "oe-1", "byteOffset": 5})),
            1,
        )
        .await;

        let outcome = reconnect(&fx.client, &fx.opts, &mut session).await;

        assert!(matches!(outcome, ReconnectOutcome::Retried));
        assert!(session.subscription.is_some());
        assert_eq!(session.lease.token, "tok-2");
        assert_eq!(
            session.output_cursor, None,
            "the recovery re-open is cursorless"
        );
        let log = fx.gateway.log.lock().unwrap().clone();
        assert_eq!(
            control_actions(&log),
            vec!["renew".to_owned(), "acquire".to_owned(), "renew".to_owned()],
            "the retry renews the lease acquired this round: {log:?}"
        );
        let opens = sub_open_inputs(&log);
        assert_eq!(opens.len(), 2);
        assert_eq!(
            opens[0]["cursor"],
            json!({"outputEpoch": "oe-1", "byteOffset": 5})
        );
        assert!(
            opens[1].get("cursor").is_none(),
            "the second open must carry no cursor: {log:?}"
        );
    }

    /// Error classification for the renew-first strategy.
    #[test]
    fn renew_verdict_classifies_lease_errors() {
        let contract = |code: &str| {
            ClientError::Contract(ContractError::new(
                code.to_owned(),
                format!("{code} message"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ))
        };
        assert!(matches!(
            renew_verdict(&contract(CODE_LEASE_EXPIRED)),
            RenewVerdict::Dead
        ));
        assert!(matches!(
            renew_verdict(&contract(CODE_CONTROL_CONFLICT)),
            RenewVerdict::TakenOver
        ));
        // Unrelated contract codes and transport failures say nothing
        // about the lease.
        assert!(matches!(
            renew_verdict(&contract(CODE_CURSOR_EXPIRED)),
            RenewVerdict::Unknown
        ));
        assert!(matches!(
            renew_verdict(&ClientError::connection_failed("ws closed")),
            RenewVerdict::Unknown
        ));
    }
}
