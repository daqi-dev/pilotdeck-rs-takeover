//! S5 journey F3: 断网重连（远程）——同机模拟版 + 跨主机 `#[ignore]` 验收层
//! (`_tmp/20261119-s5/journeys-execution-plan.md` §3 F3)。
//!
//! 拓扑（J8 同机「模拟远程」形状 + W0 计划的可控 TCP proxy 插在客户端与
//! gateway 之间）：home B 经 `server serve` 显式承载；home A 的 target 档案
//! 指向 **proxy** 的 ws:// 端点；proxy 把 TCP 字节流双向转发到 B。注入 =
//! proxy `cut()`：切断（shutdown）所有已中继连接，并停止转发新连接（监听
//! socket 保持绑定，断网期间的引擎重连尝试立即失败）；恢复 = `restore()`。
//! proxy 只绑 127.0.0.1 + 系统分配临时端口（§5 安全边界），只作用于本测试
//! 自有的 socket/线程，绝不触碰 `~/.pilotdeck` 或按名字匹配的系统进程。
//! 本 crate 的 tokio 无 `net` feature，所以 proxy 用 `std::net` + 每方向
//! 一个读线程实现；该装置按计划本应落 `tests/common_v1/proxy.rs`，本任务
//! 只允许新建自己的两个文件，故文件内实现（W4 复用时再上提）。
//!
//! 判据（计划 §3 F3 口径，按实现核对后的实际形态断言）：
//! - `conn.state reconnecting`（引擎 400ms watchdog 检测到上游关闭后发出）；
//! - 在途读 `CONNECTION_FAILED`（not_applied）、在途写 `RESULT_UNKNOWN`
//!   （unknown + idempotencyKey 回显）。确定性做法沿用 t9 先例：SIGSTOP 冻结
//!   B 的 serve 子进程（只作用于本测试自有子进程），在途请求帧经 proxy 进入
//!   gateway 的内核接收缓冲；切断后应答必然回不来，而 SIGCONT 后 gateway
//!   会处理该缓冲帧（TCP 保证 FIN 之前投递），因此 `requests.lookup` 能给
//!   出确定回执——正是「RESULT_UNKNOWN + 幂等回执」要演示的恢复路径；
//! - 订阅 `sub.end disconnected` + resumeCursor（== 最后一条已送达事件的
//!   游标，t9 同款断言）；
//! - 恢复 proxy 后退避重连 `conn.state connected`（同一 instanceId），按
//!   resumeCursor 续订 run.events 不重不漏（与 `run events` 分页全量对账）；
//! - 断网期间新写请求直接 `CONNECTION_FAILED` 不排队（D9），其幂等键在
//!   服务端恒为 notObserved（从未到达、未被静默补投）；
//! - 在途写的显式重放（同键同输入）回放原回执 `deduplicated=true`，会话内
//!   run 数恰 2（睡眠基线 run + 在途 run）；
//! - proxy 全程关闭后新 CLI 进程直接失败（CONNECTION_FAILED，退出码非 0），
//!   不回落本机（D8/c002：A 侧无 local.json/v1/run，无自启 gateway），B 侧
//!   serve 与落盘无恙。
//!
//! 跨主机轮（真断网）按计划是 W4 验收层：`#[ignore]` + 环境坐标 + 远端
//! cut hook（ssh + socat/iptables，见下方占位测试的文档）。

// The suite drives the real embedded gateway through the real binary; without
// the feature there is nothing to test.
#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use common_v1::{
    CliOutcome, ConnectSession, QUIET_WINDOW, TestHome, await_cli, contract_code, expect_ok,
    init_home, rpc_request, seed_remote_project_session, serve_remote, signal_pid,
};

/// 满载预算（判据不变）：单帧等待从共享 `FRAME_TIMEOUT`（20s）放宽到 60s，
/// 覆盖本文件全部 in-flight 解析、断连终帧与续传帧等待。
const LOADED_FRAME_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

// ---------------------------------------------------------------------------
// The controllable loopback TCP proxy (W0 §5 device, file-local)
// ---------------------------------------------------------------------------

/// Shared proxy state: the forwarding flag, the cut generation, the accept
/// loop's run flag, and the registered live connections (for shutdown).
struct ProxyShared {
    forwarding: bool,
    generation: u64,
    running: bool,
    conns: Vec<(TcpStream, TcpStream)>,
}

/// A controllable loopback TCP proxy (the W0 `可控 TCP proxy`, §5 F3): binds
/// `127.0.0.1:0`, forwards bytes both ways to the gateway, and supports
/// [`FaultProxy::cut`] (sever every relayed connection and stop forwarding
/// new ones — the 断网 injection), [`FaultProxy::restore`] (forward again),
/// and [`FaultProxy::stop`] (close the listener for good — the D8 terminal
/// state). Only sockets this proxy itself owns are ever touched.
struct FaultProxy {
    addr: SocketAddr,
    shared: Arc<Mutex<ProxyShared>>,
    accept: Option<std::thread::JoinHandle<()>>,
}

impl FaultProxy {
    fn start(upstream: SocketAddr) -> FaultProxy {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the loopback proxy");
        listener
            .set_nonblocking(true)
            .expect("nonblocking proxy listener");
        let addr = listener.local_addr().expect("proxy local addr");
        let shared = Arc::new(Mutex::new(ProxyShared {
            forwarding: true,
            generation: 0,
            running: true,
            conns: Vec::new(),
        }));
        let accept_shared = shared.clone();
        let accept = std::thread::spawn(move || accept_loop(listener, upstream, accept_shared));
        FaultProxy {
            addr,
            shared,
            accept: Some(accept),
        }
    }

    /// The gateway's TCP address behind its ws:// URL (loopback only).
    fn gateway_addr(gateway_ws_url: &str) -> SocketAddr {
        let rest = gateway_ws_url
            .strip_prefix("ws://")
            .unwrap_or(gateway_ws_url);
        let (authority, _) = rest.split_once('/').unwrap_or((rest, ""));
        authority
            .parse::<SocketAddr>()
            .unwrap_or_else(|e| panic!("parse gateway address `{authority}`: {e}"))
    }

    /// The ws:// endpoint that routes through this proxy (the gateway URL's
    /// path is preserved; only the authority is replaced).
    fn ws_endpoint(&self, gateway_ws_url: &str) -> String {
        let rest = gateway_ws_url
            .strip_prefix("ws://")
            .unwrap_or(gateway_ws_url);
        let (_, path) = rest.split_once('/').unwrap_or((rest, ""));
        if path.is_empty() {
            format!("ws://{}", self.addr)
        } else {
            format!("ws://{}/{}", self.addr, path)
        }
    }

    /// The injection: sever every relayed connection (both peers see the
    /// socket die) and stop forwarding new ones while the listener stays
    /// bound — the engine's reconnect attempts during the dark window fail
    /// fast against the port instead of hanging on connects.
    fn cut(&self) {
        let mut guard = self.shared.lock().unwrap();
        guard.forwarding = false;
        guard.generation += 1;
        for (client, upstream) in guard.conns.drain(..) {
            let _ = client.shutdown(Shutdown::Both);
            let _ = upstream.shutdown(Shutdown::Both);
        }
    }

    /// The recovery: forward again. The relays severed by [`FaultProxy::cut`]
    /// stay dead (the engine re-subscribes with resume cursors); the next
    /// backoff attempt connects through the proxy once more.
    fn restore(&self) {
        self.shared.lock().unwrap().forwarding = true;
    }

    /// Close the proxy for good (listener included): the D8 terminal state.
    /// Joins the accept loop so the port is provably released before the
    /// unreachable-target assertion runs.
    fn stop(&mut self) {
        {
            let mut guard = self.shared.lock().unwrap();
            guard.running = false;
            guard.forwarding = false;
            guard.generation += 1;
            for (client, upstream) in guard.conns.drain(..) {
                let _ = client.shutdown(Shutdown::Both);
                let _ = upstream.shutdown(Shutdown::Both);
            }
        }
        if let Some(handle) = self.accept.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for FaultProxy {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The proxy's accept loop: relays while forwarding, accepts-and-closes while
/// cut (a fast, honest "network down" for reconnect attempts), and exits when
/// stopped.
fn accept_loop(listener: TcpListener, upstream: SocketAddr, shared: Arc<Mutex<ProxyShared>>) {
    loop {
        let forwarding = {
            let guard = shared.lock().unwrap();
            if !guard.running {
                break;
            }
            guard.forwarding
        };
        match listener.accept() {
            Ok((client, _peer)) => {
                if !forwarding {
                    continue;
                }
                let Ok(up) = TcpStream::connect(upstream) else {
                    continue;
                };
                let _ = client.set_nodelay(true);
                let _ = up.set_nodelay(true);
                // Two direction threads, each owning clones of the socket pair.
                let Ok(c_read) = client.try_clone() else {
                    continue;
                };
                let Ok(c_write) = client.try_clone() else {
                    continue;
                };
                let Ok(u_read) = up.try_clone() else {
                    continue;
                };
                let Ok(u_write) = up.try_clone() else {
                    continue;
                };
                let generation = {
                    let mut guard = shared.lock().unwrap();
                    if !guard.forwarding {
                        // Raced with cut(): the just-accepted pair dies now.
                        let _ = client.shutdown(Shutdown::Both);
                        let _ = up.shutdown(Shutdown::Both);
                        continue;
                    }
                    guard.conns.push((client, up));
                    guard.generation
                };
                let shared_a = shared.clone();
                let shared_b = shared.clone();
                std::thread::spawn(move || relay_direction(c_read, u_write, shared_a, generation));
                std::thread::spawn(move || relay_direction(u_read, c_write, shared_b, generation));
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
}

/// One relay direction: copy bytes reader -> writer until the connection ends
/// or the cut generation changes. The bounded read timeout keeps the
/// generation check live; `shutdown` from cut/stop unblocks both directions
/// immediately (shutdown is socket-level, so it reaches every clone).
fn relay_direction(
    mut reader: TcpStream,
    mut writer: TcpStream,
    shared: Arc<Mutex<ProxyShared>>,
    generation: u64,
) {
    let _ = reader.set_read_timeout(Some(Duration::from_millis(50)));
    let mut buf = [0u8; 16 * 1024];
    loop {
        if shared.lock().unwrap().generation != generation {
            return;
        }
        match reader.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                if writer.write_all(&buf[..n]).is_err() {
                    return;
                }
            }
            Err(err)
                if matches!(
                    err.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(_) => return,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parses a failed invocation: one `outcome=failed` envelope line, a non-zero
/// exit, and the contract error code (the F6 journey's shape).
fn failure_code(method: &str, out: &CliOutcome) -> String {
    assert_ne!(
        out.code, 0,
        "`{method}` was expected to fail, got: {}",
        out.stdout
    );
    let lines: Vec<&str> = out
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 1, "`{method}` must print one envelope line");
    let value: Value = serde_json::from_str(lines[0])
        .unwrap_or_else(|e| panic!("`{method}` stdout is not JSON: {e}\n{}", out.stdout));
    assert_eq!(value["outcome"], "failed", "`{method}` outcome: {value}");
    value["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// The paged `run events` history through the remote target (the pagination
/// contract of common_v1's local helper, with `--target`).
async fn all_remote_run_event_cursors(home: &Path, target: &str, run_id: &str) -> Vec<String> {
    let mut cursors = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..64 {
        let mut args = vec![
            "run",
            "events",
            "--output",
            "json",
            "--target",
            target,
            "--run-id",
            run_id,
            "--page",
            "{\"limit\":500}",
        ];
        if let Some(cursor) = &after {
            args.push("--after-cursor");
            args.push(cursor);
        }
        let page = await_cli(home, &args);
        let page = expect_ok("run.events", &page);
        let frame = &page["frame"];
        let events = frame["events"].as_array().cloned().unwrap_or_default();
        let count = events.len();
        for event in events {
            cursors.push(event["cursor"].as_str().unwrap().to_owned());
        }
        if frame["caughtUp"] == true {
            return cursors;
        }
        after = frame["nextCursor"].as_str().map(str::to_owned);
        assert!(count > 0, "pagination stalled: {frame}");
    }
    panic!("run.events pagination did not converge for {run_id}");
}

/// `requests.lookup` for a run.submit idempotency key, through the target.
fn lookup_remote(home: &Path, target: &str, key: &str) -> Value {
    let out = await_cli(
        home,
        &[
            "requests",
            "lookup",
            "--output",
            "json",
            "--target",
            target,
            "--method-id",
            "run.submit",
            "--idempotency-key",
            key,
        ],
    );
    expect_ok("requests.lookup", &out)
}

// ---------------------------------------------------------------------------
// The same-machine journey (the normal nextest test)
// ---------------------------------------------------------------------------

/// F3 same-machine: with a live remote-style connection routed through the
/// controllable proxy, a network cut must fail in-flight reads
/// `CONNECTION_FAILED` and in-flight writes `RESULT_UNKNOWN`, end the
/// subscription `disconnected` with its resume cursor, and refuse new
/// requests while reconnecting (never queueing them); restoring the proxy
/// lets the engine reconnect with backoff and re-subscribe from the resume
/// cursor without duplicates or loss, the idempotent key resolves the
/// severed write, and a fully-closed proxy makes fresh CLI processes fail
/// loudly without ever falling back to a local gateway (D8).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f3_proxy_cut_reconnect_resume_without_dup_or_loss() {
    // Home B: the server data root, initialized and served (J8 shape).
    let home_b = TestHome::new("f3-b");
    let init = init_home(home_b.path());
    let instance_b = init["identity"]["instanceId"]
        .as_str()
        .expect("instanceId")
        .to_owned();
    let serve = serve_remote(home_b.path());
    let ws_b = serve.ws_url();
    assert!(
        ws_b.starts_with("ws://127.0.0.1:"),
        "loopback serve: {ws_b}"
    );

    // The proxy between A and B, plus home A (the client, left
    // uninitialized — the remote hop carries B's identity).
    let mut proxy = FaultProxy::start(FaultProxy::gateway_addr(&ws_b));
    let endpoint = proxy.ws_endpoint(&ws_b);
    assert!(endpoint.starts_with("ws://127.0.0.1:"), "{endpoint}");
    let home_a = TestHome::new("f3-a");
    let token = std::fs::read_to_string(home_b.path().join("local.token"))
        .expect("read home B local.token")
        .trim()
        .to_owned();
    std::fs::write(
        home_a.path().join("credentials.json"),
        serde_json::to_vec(&json!({"tokens": {"f3-cred": token}})).unwrap(),
    )
    .expect("write home A credentials.json");
    let wrote = expect_ok(
        "target.write",
        &await_cli(
            home_a.path(),
            &[
                "target",
                "write",
                "--output",
                "json",
                "--target-id",
                "f3-rt",
                "--name",
                "F3 through the proxy",
                "--kind",
                "https",
                "--endpoint",
                &endpoint,
                "--expected-instance-id",
                &instance_b,
                "--credential-ref",
                "f3-cred",
            ],
        ),
    );
    expect_ok(
        "target.select",
        &await_cli(
            home_a.path(),
            &[
                "target",
                "select",
                "--output",
                "json",
                "--target-id",
                "f3-rt",
                "--expected-revision",
                wrote["target"]["revision"].as_str().unwrap(),
            ],
        ),
    );

    // The identity over the wire through the proxy (the J8 connection.open
    // step): B's instance, a server-assigned connectionId, and no credential
    // material in the output.
    let opened = expect_ok(
        "connection.open",
        &await_cli(
            home_a.path(),
            &[
                "connection",
                "open",
                "--output",
                "json",
                "--target-id",
                "f3-rt",
            ],
        ),
    );
    assert_eq!(opened["identity"]["instanceId"], instance_b, "{opened}");
    assert!(
        opened["connectionId"]
            .as_str()
            .is_some_and(|c| !c.is_empty()),
        "{opened}"
    );
    assert!(
        !opened.to_string().contains(&token),
        "credential material must never appear in connection.open output"
    );

    // Journey prefix: project + session + a sleeping run, all through the
    // proxy (every argv step schema-validated by expect_ok).
    let (project_id, session_id) =
        seed_remote_project_session(home_a.path(), "f3-rt", &instance_b, "f3").await;
    let sleeper = expect_ok(
        "run.submit",
        &await_cli(
            home_a.path(),
            &[
                "run",
                "submit",
                "--output",
                "json",
                "--target",
                "f3-rt",
                "--session-id",
                &session_id,
                "--request-key",
                "f3-sleeper",
                "--text",
                "[pd-driver:sleep=600000]",
            ],
        ),
    );
    let sleeper_id = sleeper["runId"].as_str().unwrap().to_owned();

    // The live session and its run.events subscription.
    let mut conn = ConnectSession::start(home_a.path()).await;
    conn.initialize().await;
    let open = conn
        .sub_open("run.events", json!({"runId": sleeper_id}), 64)
        .await;
    let sub_id = open["result"]["subscriptionId"]
        .as_str()
        .expect("subscriptionId")
        .to_owned();
    let first = conn
        .wait_for(
            |v| {
                v.get("method").and_then(Value::as_str) == Some("sub.event")
                    && v["params"]["subscriptionId"].as_str() == Some(sub_id.as_str())
            },
            LOADED_FRAME_TIMEOUT,
            "the first run.events frame",
        )
        .await;
    let mut first_batch = vec![
        first["params"]["event"]["cursor"]
            .as_str()
            .expect("cursor")
            .to_owned(),
    ];
    let mut last_cursor = first_batch[0].clone();
    // Drain the quiet tail so `last_cursor` is the newest cursor the gateway
    // actually pushed before the injection (t9's pattern; frames read here
    // are journaled by the reader at arrival).
    while let Ok(Some(line)) = tokio::time::timeout(QUIET_WINDOW, conn.rx.recv()).await {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("method").and_then(Value::as_str) == Some("sub.event")
            && value["params"]["subscriptionId"].as_str() == Some(sub_id.as_str())
        {
            last_cursor = value["params"]["event"]["cursor"]
                .as_str()
                .expect("cursor")
                .to_owned();
            first_batch.push(last_cursor.clone());
        }
    }
    assert!(!first_batch.is_empty(), "the subscription delivered events");

    // Freeze the serving gateway (the test's own serve child, t9's precedent)
    // so the in-flight probes below are deterministically unanswered, then
    // send one write (with its idempotency key) and one read.
    let serve_pid = serve.pid();
    signal_pid(serve_pid, rustix::process::Signal::STOP);
    let inflight_input = json!({
        "sessionId": session_id,
        "input": [{"kind": "text", "text": "f3 inflight write"}]
    });
    let write_id = conn.fresh_id("w");
    conn.send(&rpc_request(
        &write_id,
        "run.submit",
        json!({"input": inflight_input.clone(), "idempotencyKey": "f3-inflight"}),
    ))
    .await;
    let read_id = conn.fresh_id("r");
    conn.send(&rpc_request(
        &read_id,
        "run.get",
        json!({"input": {"runId": sleeper_id}}),
    ))
    .await;
    // Give both frames time to reach the (frozen) gateway's socket buffers.
    tokio::time::sleep(Duration::from_millis(600)).await;

    // THE INJECTION: the proxy severs every relayed connection.
    proxy.cut();
    // The gateway resumes: it processes the buffered write frame (TCP delivers
    // it before the FIN the proxy's close produced), commits the run, and
    // fails to answer on the dead socket — exactly the "result unknown" state
    // the idempotency key resolves.
    signal_pid(serve_pid, rustix::process::Signal::CONT);

    // The in-flight write resolves RESULT_UNKNOWN with the key echoed; the
    // in-flight read resolves CONNECTION_FAILED with no effect applied. The
    // two responses complete in unpredictable order, so both are caught with
    // one predicate (a sequential wait would discard the other's frame).
    let mut write_resp: Option<Value> = None;
    let mut read_resp: Option<Value> = None;
    while write_resp.is_none() || read_resp.is_none() {
        let frame = conn
            .wait_for(
                |v| {
                    v.get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| id == write_id.as_str() || id == read_id.as_str())
                },
                LOADED_FRAME_TIMEOUT,
                "the in-flight write/read resolutions",
            )
            .await;
        if frame["id"].as_str() == Some(write_id.as_str()) {
            write_resp = Some(frame);
        } else {
            read_resp = Some(frame);
        }
    }
    let write_resp = write_resp.expect("the write resolution");
    let read_resp = read_resp.expect("the read resolution");
    assert_eq!(contract_code(&write_resp), "RESULT_UNKNOWN", "{write_resp}");
    assert_eq!(
        write_resp["error"]["data"]["effect"], "unknown",
        "{write_resp}"
    );
    assert_eq!(
        write_resp["error"]["data"]["details"]["idempotencyKey"], "f3-inflight",
        "{write_resp}"
    );
    assert_eq!(
        contract_code(&read_resp),
        "CONNECTION_FAILED",
        "{read_resp}"
    );
    assert_eq!(
        read_resp["error"]["data"]["effect"], "not_applied",
        "{read_resp}"
    );

    // The subscription ends `disconnected` with the last delivered cursor.
    let end = conn
        .wait_for(
            |v| {
                v.get("method").and_then(Value::as_str) == Some("sub.end")
                    && v["params"]["subscriptionId"].as_str() == Some(sub_id.as_str())
            },
            // 满载预算（判据不变）：等引擎发出的断连终帧，5s→60s。
            Duration::from_secs(60),
            "the disconnected terminal",
        )
        .await;
    assert_eq!(end["params"]["terminal"], "disconnected", "{end}");
    assert_eq!(
        end["params"]["resumeCursor"].as_str(),
        Some(last_cursor.as_str()),
        "{end}"
    );

    // The engine announces the reconnect.
    conn.wait_for(
        |v| {
            v.get("method").and_then(Value::as_str) == Some("conn.state")
                && v["params"]["state"] == "reconnecting"
        },
        // 满载预算（判据不变）：等引擎的 reconnecting 通告帧，5s→60s。
        Duration::from_secs(60),
        "conn.state reconnecting after the cut",
    )
    .await;

    // A new write during the dark window is refused client-side (D9: no
    // automatic replay, never queued) — its key must never reach the server.
    let dark = conn
        .call(
            "run.submit",
            json!({
                "sessionId": session_id,
                "input": [{"kind": "text", "text": "f3 must never land"}]
            }),
            Some("f3-dark"),
        )
        .await;
    assert_eq!(
        contract_code(&dark),
        "CONNECTION_FAILED",
        "writes while reconnecting must be refused, not queued: {dark}"
    );
    assert_eq!(dark["error"]["data"]["effect"], "not_applied", "{dark}");

    // THE RECOVERY: restore the proxy; the engine reconnects with backoff and
    // re-binds the same instance.
    proxy.restore();
    let connected = conn
        .wait_for(
            |v| {
                v.get("method").and_then(Value::as_str) == Some("conn.state")
                    && v["params"]["state"] == "connected"
            },
            // 满载预算（判据不变）：代理恢复后引擎带退避重连，30s→60s。
            Duration::from_secs(60),
            "conn.state connected after the restore",
        )
        .await;
    assert_eq!(
        connected["params"]["instanceId"], instance_b,
        "the reconnect must bind the same instance: {connected}"
    );

    // The severed write DID reach the server (kernel-buffer delivery before
    // the FIN): the idempotency ledger answers definitively, and the explicit
    // replay (same key, same input) returns the original receipt.
    let lookup = lookup_remote(home_a.path(), "f3-rt", "f3-inflight");
    assert!(
        matches!(
            lookup["requestState"].as_str(),
            Some("accepted") | Some("completed")
        ),
        "the severed write's key must resolve: {lookup}"
    );
    let inflight_run_id = lookup["resource"]["runId"]
        .as_str()
        .expect("resource.runId")
        .to_owned();
    let replay = conn
        .call("run.submit", inflight_input, Some("f3-inflight"))
        .await;
    let replay_receipt = replay["result"]["result"].clone();
    assert_eq!(replay_receipt["deduplicated"], true, "{replay}");
    assert_eq!(
        replay_receipt["runId"].as_str(),
        Some(inflight_run_id.as_str()),
        "{replay}"
    );

    // The dark-window write never arrived and was never silently delivered.
    let dark_lookup = lookup_remote(home_a.path(), "f3-rt", "f3-dark");
    assert_eq!(
        dark_lookup["requestState"], "result_unknown",
        "the refused write must never have been observed: {dark_lookup}"
    );
    assert_eq!(dark_lookup["reason"], "notObserved", "{dark_lookup}");

    // Resume the subscription from the disconnect's resume cursor; cancel
    // the sleeper through the reconnected session (the control plane is live
    // again), then drain to the completed terminal. The cancel happens BEFORE
    // the resumed sub.open so its response wait cannot discard the resumed
    // stream's events — the cancelling/cancelled events land in the history
    // and are replayed after the resume cursor.
    let cancel = conn
        .call(
            "run.cancel",
            json!({"runId": sleeper_id}),
            Some("f3-cancel-sleeper"),
        )
        .await;
    assert_eq!(
        cancel["result"]["result"]["receipt"]["disposition"], "requested",
        "{cancel}"
    );
    let open2 = conn
        .sub_open(
            "run.events",
            json!({"runId": sleeper_id, "afterCursor": last_cursor}),
            64,
        )
        .await;
    let sub2_id = open2["result"]["subscriptionId"]
        .as_str()
        .expect("subscriptionId")
        .to_owned();
    let mut tail: Vec<String> = Vec::new();
    let end2 = loop {
        let frame = conn
            .wait_for(
                |v| {
                    let is_ours = v["params"]["subscriptionId"].as_str() == Some(sub2_id.as_str());
                    is_ours
                        && matches!(
                            v.get("method").and_then(Value::as_str),
                            Some("sub.event") | Some("sub.end")
                        )
                },
                // 满载预算（判据不变）：续传流的逐帧等待，20s→60s。
                Duration::from_secs(60),
                "the resumed run.events tail",
            )
            .await;
        match frame.get("method").and_then(Value::as_str) {
            Some("sub.event") => tail.push(
                frame["params"]["event"]["cursor"]
                    .as_str()
                    .expect("cursor")
                    .to_owned(),
            ),
            _ => break frame,
        }
    };
    assert_eq!(end2["params"]["terminal"], "completed", "{end2}");
    assert!(
        tail.iter().all(|c| c != &last_cursor),
        "the resumed stream must not repeat the last delivered cursor"
    );

    // No duplicates, nothing missing: first batch + resumed tail == the full
    // paged history (read back through the target after the fact).
    let page_cursors = all_remote_run_event_cursors(home_a.path(), "f3-rt", &sleeper_id).await;
    let mut delivered: Vec<&str> = first_batch.iter().map(String::as_str).collect();
    delivered.extend(tail.iter().map(String::as_str));
    let delivered_set: std::collections::HashSet<&str> = delivered.iter().copied().collect();
    assert_eq!(
        delivered_set.len(),
        delivered.len(),
        "no cursor may be delivered twice across the disconnect"
    );
    let missing: Vec<&String> = page_cursors
        .iter()
        .filter(|c| !delivered_set.contains(c.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "the disconnected resume must cover the run's events exactly; missing {} of {}: \
         first={:?} last={:?}",
        missing.len(),
        page_cursors.len(),
        missing.first(),
        missing.last()
    );

    // Session bookkeeping: exactly the sleeper and the severed write's run.
    let scope = format!(
        "{{\"kind\":\"session\",\"projectId\":\"{project_id}\",\"sessionId\":\"{session_id}\"}}"
    );
    let runs = expect_ok(
        "run.list",
        &await_cli(
            home_a.path(),
            &[
                "run", "list", "--output", "json", "--target", "f3-rt", "--scope", &scope,
            ],
        ),
    );
    let run_ids: Vec<String> = runs["items"]
        .as_array()
        .or_else(|| runs["runs"].as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|item| item["runId"].as_str().unwrap_or_default().to_owned())
        .collect();
    let mut expected_ids = vec![sleeper_id.clone(), inflight_run_id.clone()];
    expected_ids.sort();
    let mut got_ids = run_ids;
    got_ids.sort();
    assert_eq!(
        got_ids, expected_ids,
        "the session must hold exactly the sleeper and the severed write's run"
    );

    conn.drain_channel_to_journal();
    conn.assert_subscription_accounting();
    conn.close_stdin().await;
    conn.wait_exit(0, "clean exit after F3").await;

    // D8: with the proxy closed for good, a fresh CLI process against the
    // same profile fails loudly (CONNECTION_FAILED) and never falls back to
    // a local gateway. B's serve is untouched by the injection.
    proxy.stop();
    let failed = await_cli(
        home_a.path(),
        &[
            "run", "list", "--output", "json", "--target", "f3-rt", "--scope", &scope,
        ],
    );
    assert_eq!(
        failure_code("run.list", &failed),
        "CONNECTION_FAILED",
        "a closed network path must fail the command directly: {}",
        failed.stderr
    );
    assert!(!home_a.path().join("local.json").exists());
    assert!(!home_a.path().join("v1").exists());
    assert!(!home_a.path().join("run").exists());
    assert_eq!(
        home_a.gateway_pid(),
        None,
        "no gateway was ever auto-started in home A (D8: 不回落本机)"
    );
    assert!(
        home_b.path().join("run").join("ready.json").is_file(),
        "B's serve survived the network cut"
    );
    assert!(
        home_b.path().join("v1").join("state.sqlite").is_file(),
        "B's state store is intact"
    );
}

// ---------------------------------------------------------------------------
// The cross-host round (W4 acceptance evidence layer)
// ---------------------------------------------------------------------------

/// F3 跨主机轮（验收证据层，W4 shell 驱动）：注入点与同机版相同（网络切断），
/// 区别只在 B 是另一台真实主机的 serve，且「断网」由驱动注入：hook 是一个
/// 可执行路径（ssh + 远端 socat/iptables 之类的封装，§5 F3 行），本测试以
/// `cut` / `restore` 参数调用。驱动需设置：
/// - `F3_CROSSHOST_WS_URL`：B 的 ws:// 端点；
/// - `F3_CROSSHOST_TOKEN_FILE`：含 B 的 local token 的文件路径；
/// - `F3_CROSSHOST_INSTANCE_ID`：B 的 instanceId；
/// - `F3_CROSSHOST_CUT_HOOK`：切断/恢复 hook 可执行文件路径。
/// 未设置时诚实早退（`#[ignore]` + 环境坐标双保险，不进常规 nextest）。
/// 确定性的「在途写 RESULT_UNKNOWN + 幂等回执」矩阵留在同机版（远端 gateway
/// 无法从本机 SIGSTOP，竞态切断无法给出确定断言）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "cross-host device: the W4 journeys remote-gate driver provides the second machine and the network cut hook via F3_CROSSHOST_{WS_URL,TOKEN_FILE,INSTANCE_ID,CUT_HOOK}"]
async fn f3_crosshost_network_cut_reconnect_resume() {
    let vars = [
        "F3_CROSSHOST_WS_URL",
        "F3_CROSSHOST_TOKEN_FILE",
        "F3_CROSSHOST_INSTANCE_ID",
        "F3_CROSSHOST_CUT_HOOK",
    ];
    let mut coords = Vec::new();
    for name in vars {
        match std::env::var(name).ok().filter(|s| !s.trim().is_empty()) {
            Some(value) => coords.push((name, value)),
            None => {
                eprintln!("skipping: {name} is not set (the W4 cross-host driver provides it)");
                return;
            }
        }
    }
    let ws_url = coords[0].1.trim().to_owned();
    let token_file = coords[1].1.trim().to_owned();
    let instance_id = coords[2].1.trim().to_owned();
    let cut_hook = coords[3].1.trim().to_owned();
    let run_hook = |phase: &str| {
        let status = std::process::Command::new(&cut_hook)
            .arg(phase)
            .status()
            .unwrap_or_else(|e| panic!("run the cut hook `{cut_hook} {phase}`: {e}"));
        assert!(
            status.success(),
            "the cut hook `{cut_hook} {phase}` failed: {status}"
        );
    };

    // A 侧：隔离客户端 home + 凭据引用 + 远程档案（remote_pair 第④步形态）。
    let home_a = TestHome::new("f3x-a");
    let token = std::fs::read_to_string(&token_file)
        .unwrap_or_else(|e| panic!("read {token_file}: {e}"))
        .trim()
        .to_owned();
    std::fs::write(
        home_a.path().join("credentials.json"),
        serde_json::to_vec(&json!({"tokens": {"f3x-cred": token}})).unwrap(),
    )
    .expect("write home A credentials.json");
    let wrote = expect_ok(
        "target.write",
        &await_cli(
            home_a.path(),
            &[
                "target",
                "write",
                "--output",
                "json",
                "--target-id",
                "f3x",
                "--name",
                "F3 cross-host server",
                "--kind",
                "https",
                "--endpoint",
                &ws_url,
                "--expected-instance-id",
                &instance_id,
                "--credential-ref",
                "f3x-cred",
            ],
        ),
    );
    expect_ok(
        "target.select",
        &await_cli(
            home_a.path(),
            &[
                "target",
                "select",
                "--output",
                "json",
                "--target-id",
                "f3x",
                "--expected-revision",
                wrote["target"]["revision"].as_str().unwrap(),
            ],
        ),
    );

    // 旅程前缀：project + session + 睡眠 run，全部经真实跨主机跳。
    let (_project_id, session_id) =
        seed_remote_project_session(home_a.path(), "f3x", &instance_id, "f3x").await;
    let sleeper = expect_ok(
        "run.submit",
        &await_cli(
            home_a.path(),
            &[
                "run",
                "submit",
                "--output",
                "json",
                "--target",
                "f3x",
                "--session-id",
                &session_id,
                "--request-key",
                "f3x-sleeper",
                "--text",
                "[pd-driver:sleep=600000]",
            ],
        ),
    );
    let sleeper_id = sleeper["runId"].as_str().unwrap().to_owned();

    let mut conn = ConnectSession::start(home_a.path()).await;
    conn.initialize().await;
    let open = conn
        .sub_open("run.events", json!({"runId": sleeper_id}), 64)
        .await;
    let sub_id = open["result"]["subscriptionId"]
        .as_str()
        .expect("subscriptionId")
        .to_owned();
    let first = conn
        .wait_for(
            |v| {
                v.get("method").and_then(Value::as_str) == Some("sub.event")
                    && v["params"]["subscriptionId"].as_str() == Some(sub_id.as_str())
            },
            LOADED_FRAME_TIMEOUT,
            "the first run.events frame",
        )
        .await;
    let mut delivered = vec![
        first["params"]["event"]["cursor"]
            .as_str()
            .expect("cursor")
            .to_owned(),
    ];
    let mut last_cursor = delivered[0].clone();
    while let Ok(Some(line)) = tokio::time::timeout(QUIET_WINDOW, conn.rx.recv()).await {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("method").and_then(Value::as_str) == Some("sub.event")
            && value["params"]["subscriptionId"].as_str() == Some(sub_id.as_str())
        {
            last_cursor = value["params"]["event"]["cursor"]
                .as_str()
                .expect("cursor")
                .to_owned();
            delivered.push(last_cursor.clone());
        }
    }

    // THE INJECTION: the driver severs the real network path.
    run_hook("cut");
    let end = conn
        .wait_for(
            |v| {
                v.get("method").and_then(Value::as_str) == Some("sub.end")
                    && v["params"]["subscriptionId"].as_str() == Some(sub_id.as_str())
            },
            Duration::from_secs(10),
            "the disconnected terminal",
        )
        .await;
    assert_eq!(end["params"]["terminal"], "disconnected", "{end}");
    assert_eq!(
        end["params"]["resumeCursor"].as_str(),
        Some(last_cursor.as_str()),
        "{end}"
    );
    conn.wait_for(
        |v| {
            v.get("method").and_then(Value::as_str) == Some("conn.state")
                && v["params"]["state"] == "reconnecting"
        },
        Duration::from_secs(10),
        "conn.state reconnecting after the cut",
    )
    .await;
    let dark = conn
        .call(
            "run.submit",
            json!({
                "sessionId": session_id,
                "input": [{"kind": "text", "text": "f3x must never land"}]
            }),
            Some("f3x-dark"),
        )
        .await;
    assert_eq!(contract_code(&dark), "CONNECTION_FAILED", "{dark}");
    assert_eq!(dark["error"]["data"]["effect"], "not_applied", "{dark}");

    // THE RECOVERY: the driver restores the path; the engine reconnects with
    // backoff and re-binds the same instance.
    run_hook("restore");
    let connected = conn
        .wait_for(
            |v| {
                v.get("method").and_then(Value::as_str) == Some("conn.state")
                    && v["params"]["state"] == "connected"
            },
            Duration::from_secs(45),
            "conn.state connected after the restore",
        )
        .await;
    assert_eq!(
        connected["params"]["instanceId"], instance_id,
        "{connected}"
    );

    // Resume from the resume cursor; the cancel happens BEFORE the resumed
    // sub.open (same ordering rule as the same-machine round: the response
    // wait must not be able to discard the resumed stream's events).
    let cancel = conn
        .call(
            "run.cancel",
            json!({"runId": sleeper_id}),
            Some("f3x-cancel"),
        )
        .await;
    assert_eq!(
        cancel["result"]["result"]["receipt"]["disposition"], "requested",
        "{cancel}"
    );
    let open2 = conn
        .sub_open(
            "run.events",
            json!({"runId": sleeper_id, "afterCursor": last_cursor}),
            64,
        )
        .await;
    let sub2_id = open2["result"]["subscriptionId"]
        .as_str()
        .expect("subscriptionId")
        .to_owned();
    let mut tail: Vec<String> = Vec::new();
    let end2 = loop {
        let frame = conn
            .wait_for(
                |v| {
                    let is_ours = v["params"]["subscriptionId"].as_str() == Some(sub2_id.as_str());
                    is_ours
                        && matches!(
                            v.get("method").and_then(Value::as_str),
                            Some("sub.event") | Some("sub.end")
                        )
                },
                Duration::from_secs(20),
                "the resumed run.events tail",
            )
            .await;
        match frame.get("method").and_then(Value::as_str) {
            Some("sub.event") => tail.push(
                frame["params"]["event"]["cursor"]
                    .as_str()
                    .expect("cursor")
                    .to_owned(),
            ),
            _ => break frame,
        }
    };
    assert_eq!(end2["params"]["terminal"], "completed", "{end2}");

    // 不重不漏对账（分页全量）。断网期间的拒收写从未到达服务端。
    let page_cursors = all_remote_run_event_cursors(home_a.path(), "f3x", &sleeper_id).await;
    let mut all: Vec<&str> = delivered.iter().map(String::as_str).collect();
    all.extend(tail.iter().map(String::as_str));
    let set: std::collections::HashSet<&str> = all.iter().copied().collect();
    assert_eq!(set.len(), all.len(), "no cursor delivered twice");
    let missing: Vec<&String> = page_cursors
        .iter()
        .filter(|c| !set.contains(c.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "the cross-host resume must cover the run's events; missing {missing:?}"
    );
    let dark_lookup = lookup_remote(home_a.path(), "f3x", "f3x-dark");
    assert_eq!(
        dark_lookup["requestState"], "result_unknown",
        "{dark_lookup}"
    );
    assert_eq!(dark_lookup["reason"], "notObserved", "{dark_lookup}");

    conn.drain_channel_to_journal();
    conn.assert_subscription_accounting();
    conn.close_stdin().await;
    conn.wait_exit(0, "clean exit after the cross-host F3 round")
        .await;
}
