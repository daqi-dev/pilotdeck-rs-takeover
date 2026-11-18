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
                if renew_lease(session, &opts.terminal_id).await.is_err() {
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

async fn renew_lease(session: &AttachSession, terminal_id: &str) -> Result<(), ClientError> {
    session
        .ws
        .call_operation(
            "terminals.control",
            json!({
                "terminalId": terminal_id,
                "action": "renew",
                "expectedControlEpoch": session.lease.control_epoch,
                "leaseToken": session.lease.token,
            }),
            None,
            None,
            None,
        )
        .await?;
    Ok(())
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
        let expected = terminal
            .get("controlEpoch")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        match acquire_lease(&ws, &opts.terminal_id, expected).await {
            // A takeover while we were away: the explicit conflict path.
            Ok((lease, watermark)) => {
                match ws
                    .open_subscription(
                        "terminals.output",
                        json!({"terminalId": opts.terminal_id, "follow": true}),
                        Some(32),
                        true,
                    )
                    .await
                {
                    Ok(new_sub) => {
                        session.ws = ws;
                        session.lease = lease;
                        session.next_input_seq = watermark + 1;
                        if let Some(old) = session.subscription.replace(new_sub) {
                            let _ = old.close().await;
                        }
                        return ReconnectOutcome::Retried;
                    }
                    Err(_) => continue,
                }
            }
            Err(err) => {
                if matches!(&err, ClientError::Contract(c) if c.code == "CONTROL_CONFLICT") {
                    return ReconnectOutcome::Failed(
                        "another controller took the terminal over".to_owned(),
                    );
                }
            }
        }
    }
    ReconnectOutcome::Failed("reconnect attempts exhausted".to_owned())
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
