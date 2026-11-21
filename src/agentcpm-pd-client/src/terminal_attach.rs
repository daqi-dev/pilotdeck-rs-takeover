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
use agentcpm_pd_protocol::errors::{
    CODE_CURSOR_EXPIRED, CODE_FORBIDDEN, CODE_INSTANCE_MISMATCH, CODE_NOT_FOUND,
    CODE_UNAUTHENTICATED, JSONRPC_INVALID_PARAMS, JSONRPC_METHOD_NOT_FOUND,
};
use agentcpm_pd_protocol::framing::{FrameDecodeError, decode_ndjson_line};
use agentcpm_pd_protocol::wire::{JsonRpcFrame, JsonRpcResponse, MAX_FRAME_BYTES};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, BufReader};
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

#[derive(Debug)]
enum HostFrame {
    /// A host request awaiting its ack.
    Input {
        id: Option<String>,
        bytes: Vec<u8>,
    },
    Resize {
        id: Option<String>,
        rows: u16,
        columns: u16,
    },
    ErrorReply(JsonRpcResponse),
}

struct AttachExit {
    code: i32,
    release_lease: bool,
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
            client.close().await;
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
        session.ws.close().await;
        client.close().await;
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

    let exit = run_until_exit(&client, &opts, &mut session, &mut host_rx, &mut stdout).await;
    let _ = stdout.flush();
    // Restore original TTY attributes immediately before network teardown.
    drop(raw_guard);
    if exit.release_lease {
        release_lease(&session.ws, &session.lease, &opts.terminal_id).await;
    }
    if let Some(subscription) = session.subscription.take() {
        let _ = tokio::time::timeout(Duration::from_secs(1), subscription.close()).await;
    }
    session.ws.close().await;
    client.close().await;
    drop(host_rx);
    reader.abort();
    let _ = reader.await;
    exit.code
}

/// One resolved event of the steady-state select. The select statement's
/// futures (which borrow the session's subscription) are dropped before
/// the handlers below run, so each handler may mutate the session freely.
#[allow(clippy::large_enum_variant)] // the largest variant is the output buffer
enum Step {
    Signal(i32),
    Renew,
    Winch,
    Host(Option<HostFrame>),
    Sub(Option<SubStreamItem>),
}

struct SignalSet {
    sigint: Option<tokio::signal::unix::Signal>,
    sigterm: Option<tokio::signal::unix::Signal>,
    sighup: Option<tokio::signal::unix::Signal>,
}

impl SignalSet {
    fn install() -> Self {
        use tokio::signal::unix::{SignalKind, signal};
        Self {
            sigint: signal(SignalKind::interrupt()).ok(),
            sigterm: signal(SignalKind::terminate()).ok(),
            sighup: signal(SignalKind::hangup()).ok(),
        }
    }

    async fn recv(&mut self) -> i32 {
        tokio::select! {
            biased;
            _ = async {
                match self.sigint.as_mut() {
                    Some(s) => s.recv().await,
                    None => std::future::pending().await,
                }
            } => 130,
            _ = async {
                match self.sigterm.as_mut() {
                    Some(s) => s.recv().await,
                    None => std::future::pending().await,
                }
            } => 0,
            _ = async {
                match self.sighup.as_mut() {
                    Some(s) => s.recv().await,
                    None => std::future::pending().await,
                }
            } => 0,
        }
    }
}

fn is_reconnectable_op_error(err: &ClientError) -> bool {
    matches!(err, ClientError::ConnectionFailed(_))
        || err.is_result_unknown()
        || matches!(err, ClientError::Contract(c) if c.code == CODE_LEASE_EXPIRED)
}

async fn reconnect_interruptible(
    client: &PdClient,
    opts: &AttachOptions,
    session: &mut AttachSession,
    signals: &mut SignalSet,
) -> Result<ReconnectOutcome, AttachExit> {
    tokio::select! {
        biased;
        code = signals.recv() => Err(AttachExit {
            code,
            release_lease: true,
        }),
        outcome = reconnect(client, opts, session) => Ok(outcome),
    }
}

fn handle_terminal_ended_outcome(
    stdout: &mut std::io::Stdout,
    machine: bool,
    session: &AttachSession,
    terminal: Value,
) -> AttachExit {
    let state = terminal
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("lost")
        .to_owned();
    let _ = write_term_exit(
        stdout,
        machine,
        &json!({
            "exitCode": terminal.get("exitCode").cloned().unwrap_or(Value::Null),
            "exitSignal": terminal.get("exitSignal").cloned().unwrap_or(Value::Null),
            "cursor": session.output_cursor.clone().unwrap_or(Value::Null),
        }),
        state,
    );
    AttachExit {
        code: 0,
        release_lease: false,
    }
}

/// The attach's steady state: forward host input, deliver output frames,
/// renew the lease, and finish on the terminal's exit frame, SIGINT/SIGTERM/
/// SIGHUP, or an unrecoverable failure.
async fn run_until_exit(
    client: &PdClient,
    opts: &AttachOptions,
    session: &mut AttachSession,
    host_rx: &mut mpsc::Receiver<HostFrame>,
    stdout: &mut std::io::Stdout,
) -> AttachExit {
    let machine = opts.machine;
    let mut renewal = tokio::time::interval(RENEW_INTERVAL);
    renewal.tick().await;
    let mut signals = SignalSet::install();
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
            code = signals.recv() => Step::Signal(code),
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
            Step::Signal(code) => {
                // 007 §6.8: SIGINT/SIGTERM/SIGHUP detaches; the terminal keeps running.
                return AttachExit {
                    code,
                    release_lease: true,
                };
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
                    match reconnect_interruptible(client, opts, session, &mut signals).await {
                        Ok(ReconnectOutcome::Retried) => {
                            let _ = write_conn_state(stdout, machine, "ready", "");
                            continue;
                        }
                        Ok(ReconnectOutcome::TerminalEnded(terminal)) => {
                            return handle_terminal_ended_outcome(
                                stdout, machine, session, terminal,
                            );
                        }
                        Ok(ReconnectOutcome::Failed(reason)) => {
                            let _ = write_conn_state(stdout, machine, "failed", &reason);
                            let release_lease = !reason.contains("took the terminal over")
                                && !reason.starts_with("instance_changed");
                            return AttachExit {
                                code: 1,
                                release_lease,
                            };
                        }
                        Err(exit) => return exit,
                    }
                }
            }
            Step::Host(None) => {
                return AttachExit {
                    code: 0,
                    release_lease: true,
                };
            }
            Step::Host(Some(HostFrame::ErrorReply(resp))) => {
                if machine {
                    let val = serde_json::to_value(resp).unwrap_or(Value::Null);
                    let _ = write_line(true, stdout, &val);
                }
            }
            Step::Host(Some(HostFrame::Input { id, bytes })) => {
                let seq = session.next_input_seq;
                match send_input(session, &opts.terminal_id, seq, &bytes).await {
                    Ok(ack) => {
                        session.next_input_seq = seq + 1;
                        if machine {
                            let _ = write_host_ack(stdout, id, &ack);
                        }
                    }
                    Err(err) if is_reconnectable_op_error(&err) => {
                        let _ = write_conn_state(stdout, machine, "reconnecting", &err.to_string());
                        match reconnect_interruptible(client, opts, session, &mut signals).await {
                            Ok(ReconnectOutcome::Retried) => {
                                let _ = write_conn_state(stdout, machine, "ready", "");
                                match send_input(session, &opts.terminal_id, seq, &bytes).await {
                                    Ok(ack) => {
                                        session.next_input_seq = seq + 1;
                                        if machine {
                                            let _ = write_host_ack(stdout, id, &ack);
                                        }
                                    }
                                    Err(retry_err) => {
                                        if machine {
                                            let _ = write_host_error(stdout, id, &retry_err);
                                        }
                                        let _ = write_conn_state(
                                            stdout,
                                            machine,
                                            "failed",
                                            &retry_err.to_string(),
                                        );
                                        return AttachExit {
                                            code: 1,
                                            release_lease: true,
                                        };
                                    }
                                }
                            }
                            Ok(ReconnectOutcome::TerminalEnded(terminal)) => {
                                if machine {
                                    let _ = write_host_error(stdout, id, &err);
                                }
                                return handle_terminal_ended_outcome(
                                    stdout, machine, session, terminal,
                                );
                            }
                            Ok(ReconnectOutcome::Failed(reason)) => {
                                if machine {
                                    let _ = write_host_error(stdout, id, &err);
                                }
                                let _ = write_conn_state(stdout, machine, "failed", &reason);
                                let release_lease = !reason.contains("took the terminal over")
                                    && !reason.starts_with("instance_changed");
                                return AttachExit {
                                    code: 1,
                                    release_lease,
                                };
                            }
                            Err(exit) => return exit,
                        }
                    }
                    Err(err) => {
                        if machine {
                            let _ = write_host_error(stdout, id, &err);
                        }
                        let _ = write_conn_state(stdout, machine, "failed", &err.to_string());
                        return AttachExit {
                            code: 1,
                            release_lease: false,
                        };
                    }
                }
            }
            Step::Host(Some(HostFrame::Resize { id, rows, columns })) => {
                match send_resize(session, &opts.terminal_id, rows, columns).await {
                    Ok(()) => {
                        if machine && id.is_some() {
                            let _ = write_host_ack(stdout, id, &json!({}));
                        }
                    }
                    Err(err) if is_reconnectable_op_error(&err) => {
                        let _ = write_conn_state(stdout, machine, "reconnecting", &err.to_string());
                        match reconnect_interruptible(client, opts, session, &mut signals).await {
                            Ok(ReconnectOutcome::Retried) => {
                                let _ = write_conn_state(stdout, machine, "ready", "");
                                match send_resize(session, &opts.terminal_id, rows, columns).await {
                                    Ok(()) => {
                                        if machine && id.is_some() {
                                            let _ = write_host_ack(stdout, id, &json!({}));
                                        }
                                    }
                                    Err(retry_err) => {
                                        if machine {
                                            let _ = write_host_error(stdout, id, &retry_err);
                                        }
                                        let _ = write_conn_state(
                                            stdout,
                                            machine,
                                            "failed",
                                            &retry_err.to_string(),
                                        );
                                        return AttachExit {
                                            code: 1,
                                            release_lease: true,
                                        };
                                    }
                                }
                            }
                            Ok(ReconnectOutcome::TerminalEnded(terminal)) => {
                                if machine {
                                    let _ = write_host_error(stdout, id, &err);
                                }
                                return handle_terminal_ended_outcome(
                                    stdout, machine, session, terminal,
                                );
                            }
                            Ok(ReconnectOutcome::Failed(reason)) => {
                                if machine {
                                    let _ = write_host_error(stdout, id, &err);
                                }
                                let _ = write_conn_state(stdout, machine, "failed", &reason);
                                let release_lease = !reason.contains("took the terminal over")
                                    && !reason.starts_with("instance_changed");
                                return AttachExit {
                                    code: 1,
                                    release_lease,
                                };
                            }
                            Err(exit) => return exit,
                        }
                    }
                    Err(err) => {
                        if machine {
                            let _ = write_host_error(stdout, id, &err);
                        }
                        let _ = write_conn_state(stdout, machine, "failed", &err.to_string());
                        return AttachExit {
                            code: 1,
                            release_lease: false,
                        };
                    }
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
                    return AttachExit {
                        code: 0,
                        release_lease: false,
                    };
                }
                if !write_term_output(stdout, machine, &data) {
                    return AttachExit {
                        code: 1,
                        release_lease: true,
                    };
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
                match reconnect_interruptible(client, opts, session, &mut signals).await {
                    Ok(ReconnectOutcome::Retried) => {
                        let _ = write_conn_state(stdout, machine, "ready", "");
                        continue;
                    }
                    Ok(ReconnectOutcome::TerminalEnded(terminal)) => {
                        return handle_terminal_ended_outcome(stdout, machine, session, terminal);
                    }
                    Ok(ReconnectOutcome::Failed(reason)) => {
                        let _ = write_conn_state(stdout, machine, "failed", &reason);
                        let release_lease = !reason.contains("took the terminal over")
                            && !reason.starts_with("instance_changed");
                        return AttachExit {
                            code: 1,
                            release_lease,
                        };
                    }
                    Err(exit) => return exit,
                }
            }
        }
    }
}

/// The local tty's window size, for forwarding SIGWINCH as term.resize.
fn local_tty_size() -> Option<(u16, u16)> {
    use std::os::fd::AsFd;
    let winsize = rustix::termios::tcgetwinsize(std::io::stdin().as_fd()).ok()?;
    if winsize.ws_row >= 1 && winsize.ws_col >= 1 {
        Some((winsize.ws_row, winsize.ws_col))
    } else {
        None
    }
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
    let subscription = match ws
        .open_subscription(
            "terminals.output",
            json!({"terminalId": terminal_id, "follow": true}),
            Some(32),
            true,
        )
        .await
    {
        Ok(sub) => sub,
        Err(err) => {
            release_lease(&ws, &lease, terminal_id).await;
            return Err(err);
        }
    };
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

async fn release_lease(ws: &PdWsClient, lease: &LeaseState, terminal_id: &str) {
    if lease.token.is_empty() || ws.is_closed() {
        return;
    }
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        ws.call_operation(
            "terminals.control",
            json!({
                "terminalId": terminal_id,
                "action": "release",
                "expectedControlEpoch": lease.control_epoch,
                "leaseToken": lease.token,
            }),
            None,
            None,
            Some(Duration::from_secs(2)),
        ),
    )
    .await;
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

#[derive(Debug)]
enum ReconnectOutcome {
    Retried,
    TerminalEnded(Value),
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

fn fatal_reconnect_reason(err: &ClientError) -> Option<String> {
    match err {
        ClientError::InstanceChanged { expected, actual } => Some(format!(
            "instance_changed: expected `{expected}`, got `{actual}`"
        )),
        ClientError::NotInitialized(_) | ClientError::Usage(_) | ClientError::Config(_) => {
            Some(err.to_string())
        }
        ClientError::Contract(c) if c.code == CODE_INSTANCE_MISMATCH => {
            Some(format!("instance_changed: {}", c.message))
        }
        ClientError::Contract(c)
            if matches!(
                c.code.as_str(),
                CODE_UNAUTHENTICATED | CODE_FORBIDDEN | CODE_NOT_FOUND
            ) =>
        {
            Some(format!("{}: {}", c.code, c.message))
        }
        _ => None,
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
        let ws = match reconnect_ws(client, opts.target.as_deref()).await {
            Ok(ws) => ws,
            Err(err) => {
                if let Some(reason) = fatal_reconnect_reason(&err) {
                    return ReconnectOutcome::Failed(reason);
                }
                continue;
            }
        };
        let terminal = match fetch_terminal(&ws, &opts.terminal_id).await {
            Ok(terminal) => terminal,
            Err(err) => {
                ws.close().await;
                if let Some(reason) = fatal_reconnect_reason(&err) {
                    return ReconnectOutcome::Failed(reason);
                }
                continue;
            }
        };
        let state = terminal.get("state").and_then(Value::as_str).unwrap_or("");
        if matches!(state, "exited" | "failed" | "lost") {
            ws.close().await;
            return ReconnectOutcome::TerminalEnded(terminal);
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
                            ws.close().await;
                            if matches!(&err, ClientError::Contract(c) if c.code == CODE_CONTROL_CONFLICT)
                            {
                                return ReconnectOutcome::Failed(
                                    "another controller took the terminal over".to_owned(),
                                );
                            }
                            if let Some(reason) = fatal_reconnect_reason(&err) {
                                return ReconnectOutcome::Failed(reason);
                            }
                            continue;
                        }
                    }
                }
                RenewVerdict::TakenOver => {
                    ws.close().await;
                    return ReconnectOutcome::Failed(
                        "another controller took the terminal over".to_owned(),
                    );
                }
                // The failure says nothing about the lease; retry later.
                RenewVerdict::Unknown => {
                    ws.close().await;
                    if let Some(reason) = fatal_reconnect_reason(&err) {
                        return ReconnectOutcome::Failed(reason);
                    }
                    continue;
                }
            },
        };
        match resume_subscription(session, ws.clone(), &opts.terminal_id, watermark).await {
            Ok(()) => return ReconnectOutcome::Retried,
            Err(err) => {
                ws.close().await;
                if let Some(reason) = fatal_reconnect_reason(&err) {
                    return ReconnectOutcome::Failed(reason);
                }
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
    let old_ws = std::mem::replace(&mut session.ws, ws);
    session.next_input_seq = watermark + 1;
    if let Some(old) = session.subscription.replace(new_sub) {
        if !old_ws.is_closed() {
            let _ = tokio::time::timeout(Duration::from_millis(200), old.close()).await;
        } else {
            drop(old);
        }
    }
    old_ws.close().await;
    Ok(())
}

/// Re-uses the client's resolved home to build a fresh ws session for the
/// same bound instance (the old session died with the connection).
async fn reconnect_ws(client: &PdClient, target: Option<&str>) -> Result<PdWsClient, ClientError> {
    let opts = PdClientOptions {
        home_override: Some(client.home().clone()),
        target: target.map(ToOwned::to_owned),
        no_start: true,
        spawner: None,
    };
    let expected = client.ws().bound_instance_id();
    let reconnected = PdClient::connect_pinned(opts, expected).await?;
    let actual = reconnected.ws().bound_instance_id().to_owned();
    if actual != expected {
        reconnected.close().await;
        return Err(ClientError::InstanceChanged {
            expected: expected.to_owned(),
            actual,
        });
    }
    Ok(reconnected.ws().clone())
}

// ---------------------------------------------------------------------------
// Host segment (stdio frames)
// ---------------------------------------------------------------------------

fn write_line(machine: bool, stdout: &mut std::io::Stdout, frame: &Value) -> std::io::Result<()> {
    if machine {
        let mut line = serde_json::to_string(frame).unwrap_or_default();
        line.push('\n');
        stdout.write_all(line.as_bytes())
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
        let Ok(mut line) = serde_json::to_string(&frame) else {
            return true;
        };
        line.push('\n');
        return stdout.write_all(line.as_bytes()).is_ok();
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

fn build_host_error_response(id: String, err: &ClientError) -> JsonRpcResponse {
    match err {
        ClientError::Contract(contract) => JsonRpcResponse::application_error(id, contract),
        ClientError::ProtocolError { code, message } => {
            JsonRpcResponse::err(Some(id), *code, message.clone(), None)
        }
        other => JsonRpcResponse::application_error(id, &other.into_contract_error()),
    }
}

fn write_host_error(
    stdout: &mut std::io::Stdout,
    id: Option<String>,
    err: &ClientError,
) -> std::io::Result<()> {
    let Some(id) = id else {
        return Ok(());
    };
    let resp = build_host_error_response(id, err);
    let val = serde_json::to_value(resp).unwrap_or(Value::Null);
    write_line(true, stdout, &val)
}

async fn read_line_capped<R: AsyncBufRead + Unpin>(
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

fn parse_machine_host_line(raw: &[u8], truncated: bool, total: usize) -> Option<HostFrame> {
    if truncated {
        return FrameDecodeError::FrameTooLarge { bytes: total }
            .to_error_response()
            .map(HostFrame::ErrorReply);
    }
    let frame = match decode_ndjson_line(raw) {
        Ok(f) => f,
        Err(FrameDecodeError::EmptyLine) => return None,
        Err(err) => return err.to_error_response().map(HostFrame::ErrorReply),
    };
    let (id, method, params) = match frame {
        JsonRpcFrame::Request(req) => (Some(req.id), req.method, req.params),
        JsonRpcFrame::Notification(notif) => (None, notif.method, notif.params),
        JsonRpcFrame::Response(resp) => {
            return resp.id.map(|id| {
                HostFrame::ErrorReply(JsonRpcResponse::err(
                    Some(id),
                    agentcpm_pd_protocol::errors::JSONRPC_INVALID_REQUEST,
                    "unexpected response frame",
                    None,
                ))
            });
        }
    };
    match method.as_str() {
        "term.input" => {
            let Some(encoded) = params.get("bytesBase64").and_then(Value::as_str) else {
                return id.map(|req_id| {
                    HostFrame::ErrorReply(JsonRpcResponse::err(
                        Some(req_id),
                        JSONRPC_INVALID_PARAMS,
                        "term.input params.bytesBase64 must be a base64 string",
                        None,
                    ))
                });
            };
            match base64_decode(encoded) {
                Ok(bytes) => Some(HostFrame::Input { id, bytes }),
                Err(err) => id.map(|req_id| {
                    HostFrame::ErrorReply(JsonRpcResponse::err(
                        Some(req_id),
                        JSONRPC_INVALID_PARAMS,
                        format!("term.input invalid bytesBase64: {err}"),
                        None,
                    ))
                }),
            }
        }
        "term.resize" => {
            let Some(rows) = params.get("rows").and_then(Value::as_u64) else {
                return id.map(|req_id| {
                    HostFrame::ErrorReply(JsonRpcResponse::err(
                        Some(req_id),
                        JSONRPC_INVALID_PARAMS,
                        "term.resize params.rows must be an integer >= 1",
                        None,
                    ))
                });
            };
            let Some(columns) = params.get("columns").and_then(Value::as_u64) else {
                return id.map(|req_id| {
                    HostFrame::ErrorReply(JsonRpcResponse::err(
                        Some(req_id),
                        JSONRPC_INVALID_PARAMS,
                        "term.resize params.columns must be an integer >= 1",
                        None,
                    ))
                });
            };
            if rows == 0
                || rows > u64::from(u16::MAX)
                || columns == 0
                || columns > u64::from(u16::MAX)
            {
                return id.map(|req_id| {
                    HostFrame::ErrorReply(JsonRpcResponse::err(
                        Some(req_id),
                        JSONRPC_INVALID_PARAMS,
                        "term.resize rows and columns must be in 1..=65535",
                        None,
                    ))
                });
            }
            Some(HostFrame::Resize {
                id,
                rows: rows as u16,
                columns: columns as u16,
            })
        }
        other => id.map(|req_id| {
            HostFrame::ErrorReply(JsonRpcResponse::err(
                Some(req_id),
                JSONRPC_METHOD_NOT_FOUND,
                format!("method not found: {other}"),
                None,
            ))
        }),
    }
}

/// Reads host input: NDJSON `term.input`/`term.resize` frames in machine
/// mode, raw bytes (chunked) in human mode.
fn spawn_stdin_reader(
    machine: bool,
    host_tx: mpsc::Sender<HostFrame>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if machine {
            let mut reader = BufReader::new(tokio::io::stdin());
            while let Ok(Some((line, truncated, total))) =
                read_line_capped(&mut reader, MAX_FRAME_BYTES).await
            {
                if let Some(frame) = parse_machine_host_line(&line, truncated, total)
                    && host_tx.send(frame).await.is_err()
                {
                    break;
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
        instance_id: String,
        terminal_state: String,
    }

    fn terminal_json(lease: &MockLease, state: &str) -> Value {
        json!({
            "terminalId": TERMINAL_ID,
            "projectId": "proj-1",
            "serverEpoch": "gen-1",
            "state": state,
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
                "release" => {
                    if lease.control_epoch != expected {
                        Some((CODE_CONTROL_CONFLICT, "epoch mismatch".to_owned()))
                    } else if lease.token.as_deref() != token.as_deref() {
                        Some((
                            CODE_CONTROL_CONFLICT,
                            "leaseToken does not match the active controller".to_owned(),
                        ))
                    } else {
                        lease.alive = false;
                        lease.token = None;
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
                    "terminal": terminal_json(&lease, &self.terminal_state),
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
                            instance_id: self.instance_id.clone(),
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
                    Self::ok_result(
                        req,
                        json!({"terminal": terminal_json(&lease, &self.terminal_state)}),
                    )
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
        spawn_custom_mock_gateway(
            lease,
            cursor_expired_opens,
            INSTANCE_ID.to_owned(),
            "running".to_owned(),
        )
        .await
    }

    async fn spawn_custom_mock_gateway(
        lease: MockLease,
        cursor_expired_opens: usize,
        instance_id: String,
        terminal_state: String,
    ) -> (String, Arc<MockGateway>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let gateway = Arc::new(MockGateway {
            lease: Mutex::new(lease),
            log: Mutex::new(Vec::new()),
            cursor_expired_opens,
            opens: AtomicUsize::new(0),
            instance_id,
            terminal_state,
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

    #[tokio::test]
    async fn attach_session_releases_lease_when_open_subscription_fails() {
        let (fx, _session) = attach_fixture(
            "attach-open-fail",
            MockLease {
                control_epoch: 1,
                token: None,
                alive: false,
            },
            held_lease(1, "tok-1"),
            None,
            1,
        )
        .await;

        let err = match attach_session(fx.client.ws().clone(), TERMINAL_ID).await {
            Ok(_) => panic!("expected sub.open failure to abort attach_session"),
            Err(err) => err,
        };
        assert!(
            matches!(err, ClientError::Contract(ref c) if c.code == CODE_CURSOR_EXPIRED),
            "unexpected error: {err:?}"
        );
        let log = fx.gateway.log.lock().unwrap().clone();
        assert_eq!(
            control_actions(&log),
            vec!["acquire".to_owned(), "release".to_owned()],
            "failed sub.open must release the freshly acquired lease: {log:?}"
        );
        assert!(
            !fx.gateway.lease.lock().unwrap().alive,
            "gateway mock lease must be released after rollback"
        );
        fx.client.close().await;
    }

    #[tokio::test]
    async fn reconnect_pins_bound_instance_id_even_for_url_targets_and_aborts_on_fatal_errors() {
        let (fx, mut session) = attach_fixture(
            "pin-url-instance",
            MockLease {
                control_epoch: 1,
                token: Some("tok-1".to_owned()),
                alive: true,
            },
            held_lease(1, "tok-1"),
            None,
            0,
        )
        .await;

        let (other_url, _other_gw) = spawn_custom_mock_gateway(
            MockLease {
                control_epoch: 1,
                token: Some("tok-1".to_owned()),
                alive: true,
            },
            0,
            "remote-019a0000-0000-7000-8000-ffffffffffff".to_owned(),
            "running".to_owned(),
        )
        .await;
        let mut opts = fx.opts;
        opts.target = Some(other_url);

        let outcome = reconnect(&fx.client, &opts, &mut session).await;
        assert!(
            matches!(&outcome, ReconnectOutcome::Failed(reason) if reason.contains("instance_changed")),
            "reconnect must fail immediately on mismatched bound_instance_id, got {outcome:?}"
        );

        let contract = |code: &str| {
            ClientError::Contract(ContractError::new(
                code.to_owned(),
                "fatal".to_owned(),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ))
        };
        assert!(fatal_reconnect_reason(&contract(CODE_UNAUTHENTICATED)).is_some());
        assert!(fatal_reconnect_reason(&contract(CODE_FORBIDDEN)).is_some());
        assert!(fatal_reconnect_reason(&contract(CODE_NOT_FOUND)).is_some());
        assert!(fatal_reconnect_reason(&contract(CODE_INSTANCE_MISMATCH)).is_some());
        assert!(
            fatal_reconnect_reason(&ClientError::InstanceChanged {
                expected: "a".to_owned(),
                actual: "b".to_owned(),
            })
            .is_some()
        );
        assert!(fatal_reconnect_reason(&ClientError::connection_failed("closed")).is_none());

        fx.client.close().await;
    }

    #[tokio::test]
    async fn reconnect_returns_terminal_ended_for_lost_terminal() {
        let (fx, mut session) = attach_fixture(
            "lost-terminal",
            MockLease {
                control_epoch: 1,
                token: Some("tok-1".to_owned()),
                alive: true,
            },
            held_lease(1, "tok-1"),
            None,
            0,
        )
        .await;

        let (lost_url, _lost_gw) = spawn_custom_mock_gateway(
            MockLease {
                control_epoch: 1,
                token: None,
                alive: false,
            },
            0,
            INSTANCE_ID.to_owned(),
            "lost".to_owned(),
        )
        .await;
        let mut opts = fx.opts;
        opts.target = Some(lost_url);

        let outcome = reconnect(&fx.client, &opts, &mut session).await;
        assert!(
            matches!(&outcome, ReconnectOutcome::TerminalEnded(t) if t["state"] == "lost"),
            "lost terminal must finish as TerminalEnded, got {outcome:?}"
        );
        fx.client.close().await;
    }

    #[tokio::test]
    async fn read_line_capped_recovers_after_oversized_line_and_validates_host_frames() {
        let oversized = format!("{{\"junk\":\"{}\"}}\n", "x".repeat(256));
        let valid = "{\"jsonrpc\":\"2.0\",\"id\":\"r1\",\"method\":\"term.resize\",\"params\":{\"rows\":24,\"columns\":80}}\n";
        let payload = format!("{oversized}{valid}");
        let mut reader = BufReader::new(std::io::Cursor::new(payload.into_bytes()));

        let (raw1, truncated1, total1) = read_line_capped(&mut reader, 128)
            .await
            .expect("first read succeeds")
            .expect("first line present");
        assert!(truncated1);
        assert!(total1 > 128);
        match parse_machine_host_line(&raw1, truncated1, total1) {
            Some(HostFrame::ErrorReply(resp)) => {
                assert!(resp.error.is_some());
            }
            other => panic!("expected ErrorReply for oversized line, got {other:?}"),
        }

        let (raw2, truncated2, total2) = read_line_capped(&mut reader, 128)
            .await
            .expect("second read succeeds")
            .expect("second line present");
        assert!(!truncated2);
        match parse_machine_host_line(&raw2, truncated2, total2) {
            Some(HostFrame::Resize { id, rows, columns }) => {
                assert_eq!(id.as_deref(), Some("r1"));
                assert_eq!(rows, 24);
                assert_eq!(columns, 80);
            }
            other => panic!("expected valid Resize after recovery, got {other:?}"),
        }

        let zero_rows = br#"{"jsonrpc":"2.0","id":"z1","method":"term.resize","params":{"rows":0,"columns":80}}"#;
        match parse_machine_host_line(zero_rows, false, zero_rows.len()) {
            Some(HostFrame::ErrorReply(resp)) => {
                assert_eq!(resp.id.as_deref(), Some("z1"));
                assert_eq!(resp.error.unwrap().code, JSONRPC_INVALID_PARAMS);
            }
            other => panic!("expected INVALID_PARAMS for rows=0, got {other:?}"),
        }

        let bad_b64 = br#"{"jsonrpc":"2.0","id":"b1","method":"term.input","params":{"bytesBase64":"!bad!"}}"#;
        match parse_machine_host_line(bad_b64, false, bad_b64.len()) {
            Some(HostFrame::ErrorReply(resp)) => {
                assert_eq!(resp.id.as_deref(), Some("b1"));
                assert_eq!(resp.error.unwrap().code, JSONRPC_INVALID_PARAMS);
            }
            other => panic!("expected INVALID_PARAMS for bad base64, got {other:?}"),
        }
    }

    #[test]
    fn build_host_error_response_preserves_structured_contract_data() {
        let err = ClientError::Contract(ContractError::new(
            "LEASE_EXPIRED".to_owned(),
            "lease expired".to_owned(),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
        let resp = build_host_error_response("req-1".to_owned(), &err);
        assert_eq!(resp.id.as_deref(), Some("req-1"));
        let rpc_err = resp.error.expect("error payload present");
        let data = rpc_err
            .data
            .expect("structured ContractError data preserved");
        assert_eq!(data["code"], json!("LEASE_EXPIRED"));
    }
}
