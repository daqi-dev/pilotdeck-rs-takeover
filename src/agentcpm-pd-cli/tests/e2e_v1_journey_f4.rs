//! S5 journey F4: 慢消费者（terminals / notifications 流）——t6 的 run.events
//! 版本之外的另外两条流 (`_tmp/20261119-s5/journeys-execution-plan.md` §3 F4)。
//!
//! 水位（W0 待核实项，本文件落定）：慢消费者护栏在 gateway 的订阅泵里
//! (`pilotdeck-gateway/src/public_v1/ws.rs` 的 subscription_pump)，阈值为
//! `SUBSCRIPTION_BUFFER_ITEMS=1024` 条 或 `SUBSCRIPTION_BUFFER_BYTES=4 MiB`
//! (`agentcpm-pd-protocol/src/wire.rs:23-24`)。两条流按各自的帧形态踩不同的
//! 阈值：
//! - `terminals.output` 每帧 ≤512 KiB 原始字节（OUTPUT_FRAME_BYTES，base64 后
//!   ~700 KiB），4 MiB 保留环内 ~8 帧就跨过 **字节** 阈值——注入 = 终端内
//!   `dd|tr` 大输出（~3.95 MiB，有界，不驱逐保留环）+ 订阅 credit=1 饿死；
//! - `notifications.events` 每帧 ~0.5 KiB，字节阈值不可达（需 ~8000 条），
//!   **条数** 阈值需要 >1024 条待投递通知——注入 = 1100 个睡眠 run 里取消
//!   1099 个（每个 run.cancelled 事件恰派生一条 stop 通知，
//!   notifications_stream.rs 的 family_of）+ 1 条 question 交互的
//!   action_required，凑 1100 条待投递，订阅 credit=1 饿死后泵缓冲跨过 1024。
//!
//! 判据（计划 §3 F4 口径）：
//! - `sub.end slow_consumer` + resumeCursor。terminals 流的 resumeCursor 语义
//!   已核对：它指向最后送达帧的 **起始** 字节偏移（流游标 "epoch:offset"），
//!   不重不漏的续订点是该帧的 `nextCursor`（产品自身消费端 terminal_attach
//!   的既定做法，terminal_attach.rs:252-257）；本测试按产品做法断言续订
//!   无重无漏（字节区间精确铺满，byteOffset 单调，内容校验）。
//! - 续订后字节/事件不重不漏：terminals 侧用字节区间铺放 + 'x' 计数 + 标记
//!   恰现一次；notifications 侧用游标序列严格递增 + notificationId 唯一 +
//!   期望集合精确对账（1102 stop + 1 action_required）。
//! - 期间 `run cancel`/`interaction respond` 控制类延迟仍在上限（T7 旅程化）：
//!   恢复排流期间按里程碑发 run.get / run.cancel / interaction.respond 探针，
//!   全部 ≤2s（t7 的 in-flood 守卫口径）。
//!
//! 租约令牌是秘密：所有带 leaseToken 的调用走 `api connect` 会话的 stdin，
//! 不经 argv。注入只作用于本测试自有的子进程与临时 home。
//!
//! 满载预算修订（2026-11-19，final38 失败复盘）：本文件的两条旅程在
//! 「仅旅程子集」的 w12 快速验证下全过，在全量套件（9932 测试并行、机器
//! 整轮饱和）下挂在预算上——终端大输出 settle 门与通知洪泛注入被同级
//! 测试挤压变慢。修订只放宽墙钟预算与轮询节奏：settle 判据（覆盖
//! bulk + 连续两次轮询一致）、不重不漏对账、探针 ≤2s 守卫全部原样。
//! 轮询统一退避（150ms 起、×1.5、封顶 1s）：每次 argv 探针都 spawn 一个
//! CLI 进程，饱和机器上固定 150ms 节奏是自找的负载。

// The suite drives the real embedded gateway through the real binary; without
// the feature there is nothing to test.
#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{Value, json};

use common_v1::{
    ConnectSession, QUIET_WINDOW, TestHome, await_cli, expect_ok, init_home, init_home_instance,
    rpc_request, seed_project_session, submit_run,
};

/// The bounded bulk the terminal produces: 7 full 512 KiB frames plus a large
/// partial one, staying inside the 4 MiB retained ring so nothing evicts
/// (deterministic byte accounting) while the starved subscription's buffer
/// still crosses the 4 MiB slow-consumer threshold after one delivered frame.
const F4_BULK_BYTES: usize = 4_140_032;
/// The end marker the bulk command echoes after the bytes (content check).
const F4_BULK_MARKER: &str = "__F4_BULK_DONE__";
/// The in-flood control probe guard (t7's 2 s).
const PROBE_LATENCY_GUARD: Duration = Duration::from_secs(2);

// Wall-clock headroom sized for the FULL-suite profile (final38: the whole
// workspace runs in parallel and the machine stays saturated for the whole
// run; the same journeys pass in the journeys-only w12 subset). None of
// these carry assertion semantics — the settle criteria (coverage plus two
// agreeing polls), the no-dup/no-loss reconciliation and the probe latency
// guard above are exactly as the journey defines them.
/// The terminals bulk settle window: shell/dd/tr startup plus ~3.95 MiB
/// through the PTY behind thousands of sibling tests.
const F4_BULK_SETTLE_BUDGET: Duration = Duration::from_secs(600);
/// The pending-question resolution window (the offline driver must run the
/// question run before its interaction appears).
const F4_RESOLVE_BUDGET: Duration = Duration::from_secs(90);
/// One connect-session frame wait (first credited event / slow_consumer
/// terminal): the pump keeps working, just slower.
const F4_SUB_WAIT_BUDGET: Duration = Duration::from_secs(60);
/// The longest gap allowed between two frames of the resumed terminals
/// drain (base64 frames of ~700 KiB each under saturation).
const F4_DRAIN_FRAME_BUDGET: Duration = Duration::from_secs(60);
/// The resumed notifications drain's overall window (1103 notifications,
/// credit-paced).
const F4_FLOOD_DRAIN_BUDGET: Duration = Duration::from_secs(300);
/// One flood-phase RPC (run.submit / run.cancel — each is a small fsync'd
/// transaction). The shared harness pins single calls at `FRAME_TIMEOUT`
/// (20 s), which saturation can blow through mid-flood; `call_with_budget`
/// re-issues the same request shape with this headroom instead.
const F4_FLOOD_CALL_BUDGET: Duration = Duration::from_secs(60);
/// The argv-probe cadence: start tight, back off to 1 s so a saturated
/// machine is not hammered by probe processes.
const F4_POLL_MAX: Duration = Duration::from_secs(1);

/// Sleeps the current probe cadence, then backs it off (x1.5, capped).
async fn back_off(gap: &mut Duration) {
    tokio::time::sleep(*gap).await;
    *gap = (*gap * 3 / 2).min(F4_POLL_MAX);
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn decode_b64(text: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .expect("decode bytesBase64")
}

/// `ConnectSession::call` with a full-suite-sized per-RPC budget. The
/// request shape (id, optional idempotency key, response wait) is identical;
/// only the single-call wait grows from the shared harness's `FRAME_TIMEOUT`
/// (20 s) to [`F4_FLOOD_CALL_BUDGET`] — a saturated machine can blow through
/// 20 s on one fsync-backed flood submit, and the flood phase is 2199 such
/// calls back to back.
async fn call_with_budget(
    conn: &mut ConnectSession,
    method: &str,
    input: Value,
    key: Option<&str>,
) -> Value {
    let id = conn.fresh_id("r");
    let mut params = json!({"input": input});
    if let Some(k) = key {
        params["idempotencyKey"] = json!(k);
    }
    conn.send(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params
    }))
    .await;
    conn.wait_response_for(&id, F4_FLOOD_CALL_BUDGET, &format!("{method} response"))
        .await
}

/// Registers project `p-f4t` on the default authorized root so
/// `terminals.create` can resolve the working directory (the t015 shape).
async fn seed_terminal_project(home: &Path) -> String {
    let instance_id = init_home_instance(home);
    let out = await_cli(
        home,
        &[
            "projects",
            "register",
            "--output",
            "json",
            "--project-id",
            "p-f4t",
            "--name",
            "F4 terminals journey",
            "--environment-id",
            "local",
            "--root",
            &format!(
                "{{\"instanceId\":\"{instance_id}\",\"rootId\":\"default\",\"relativePath\":\"term\"}}"
            ),
        ],
    );
    expect_ok("project.register", &out)["project"]["projectId"]
        .as_str()
        .expect("projectId")
        .to_owned()
}

async fn create_terminal(home: &Path, project_id: &str) -> Value {
    let out = await_cli(
        home,
        &[
            "terminal",
            "create",
            "--output",
            "json",
            "--project-id",
            project_id,
            "--rows",
            "24",
            "--columns",
            "80",
        ],
    );
    expect_ok("terminals.create", &out)["terminal"].clone()
}

async fn get_terminal(home: &Path, terminal_id: &str) -> Value {
    let out = await_cli(
        home,
        &[
            "terminal",
            "get",
            "--output",
            "json",
            "--terminal-id",
            terminal_id,
        ],
    );
    expect_ok("terminals.get", &out)["terminal"].clone()
}

/// F4 terminals half: a big bounded output inside the terminal starves a
/// credit-1 `terminals.output` subscription until the gateway's byte
/// watermark (4 MiB buffered) ends it `slow_consumer` with a resume cursor;
/// the resumed subscription (from the last delivered frame's `nextCursor`,
/// the product consumer's resume point) delivers the remaining bytes without
/// duplicates or loss, control probes stay fast during the byte flood, and
/// closing the terminal ends the stream through the `exit` frame +
/// `completed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn f4_terminal_output_slow_consumer_resume_without_dup_or_loss() {
    let home = TestHome::new("f4-term");
    let root = home.path();
    init_home(root);
    let project_id = seed_terminal_project(root).await;

    // The probe run for the T7-ized control checks (cancelled during the
    // resumed drain).
    let (_, probe_session) = seed_project_session(root, "f4tp").await;
    let p1 = submit_run(
        root,
        &probe_session,
        "[pd-driver:sleep=600000]",
        "f4-term-p1",
    )
    .await;
    let p1_id = p1["runId"].as_str().expect("runId").to_owned();

    let terminal = create_terminal(root, &project_id).await;
    let terminal_id = terminal["terminalId"]
        .as_str()
        .expect("terminalId")
        .to_owned();
    let server_epoch = terminal["serverEpoch"]
        .as_str()
        .expect("serverEpoch")
        .to_owned();

    let mut conn = ConnectSession::start(root).await;
    conn.initialize().await;

    // The control lease; the token stays off argv (session stdin carries it).
    let acquired = conn
        .call(
            "terminals.control",
            json!({"terminalId": terminal_id, "action": "acquire", "expectedControlEpoch": 0}),
            None,
        )
        .await;
    let lease = acquired["result"]["result"]["lease"].clone();
    assert_eq!(lease["controlEpoch"], 1, "{acquired}");
    let token = lease["leaseToken"].as_str().expect("leaseToken").to_owned();

    // The big bounded output: ~3.95 MiB of 'x' plus the end marker.
    // The trailing newline submits the line (a user pressing Enter); without
    // it the PTY shell echoes the command but never executes it — the exact
    // 162-byte stall final38-40 showed.
    let command = format!(
        "dd if=/dev/zero bs=1024 count={} 2>/dev/null | tr '\\0' x; echo {F4_BULK_MARKER}\n",
        F4_BULK_BYTES / 1024
    );
    let ack = conn
        .call(
            "terminals.input",
            json!({
                "terminalId": terminal_id,
                "serverEpoch": server_epoch,
                "controlEpoch": 1,
                "leaseToken": token,
                "inputSeq": 1,
                "bytesBase64": b64(command.as_bytes()),
            }),
            None,
        )
        .await;
    assert_eq!(ack["result"]["result"]["acceptedThroughSeq"], 1, "{ack}");

    // Determinism gate: the bulk and marker are fully inside the retained
    // ring (the live bounds settle) before the starved subscription opens.
    // The argv `terminal get` reads the live ring bounds (schema-validated).
    // The settle CRITERION is unchanged (latest covers the bulk AND two
    // consecutive polls agree); only the window is full-suite-sized — under
    // final38-class saturation the shell/dd/tr pipeline plus the ~3.95 MiB
    // PTY drain take far longer than an idle machine's seconds, while the
    // reader provably pushes every byte before EOF (terminals_pty), so the
    // bulk always settles, just slower.
    let (earliest0, latest0) = {
        let mut prev: Option<u64> = None;
        let deadline = Instant::now() + F4_BULK_SETTLE_BUDGET;
        let mut poll_gap = Duration::from_millis(150);
        loop {
            let got = get_terminal(root, &terminal_id).await;
            assert_eq!(got["state"], "running", "{got}");
            let latest = got["latestOutputOffset"]
                .as_u64()
                .expect("latestOutputOffset");
            let earliest = got["earliestOutputOffset"]
                .as_u64()
                .expect("earliestOutputOffset");
            if latest >= F4_BULK_BYTES as u64 && prev == Some(latest) {
                break (earliest, latest);
            }
            prev = Some(latest);
            assert!(
                Instant::now() < deadline,
                "the terminal bulk never settled: {got}"
            );
            back_off(&mut poll_gap).await;
        }
    };
    assert!(
        latest0 - earliest0 <= 4 * 1024 * 1024,
        "the bounded bulk must not evict the ring: earliest={earliest0} latest={latest0}"
    );

    // THE INJECTION: one credit against the whole retained backlog.
    let open = conn
        .sub_open("terminals.output", json!({"terminalId": terminal_id}), 1)
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
            F4_SUB_WAIT_BUDGET,
            "the one credited terminals.output frame",
        )
        .await;
    let first_data = first["params"]["event"]["data"].clone();
    assert_eq!(first_data["type"], "bytes", "{first}");
    let from0 = first_data["cursor"]["byteOffset"].as_u64().expect("from");
    let next0 = first_data["nextCursor"]["byteOffset"]
        .as_u64()
        .expect("next");
    let epoch = first_data["cursor"]["outputEpoch"]
        .as_str()
        .expect("outputEpoch")
        .to_owned();
    assert_eq!(from0, earliest0, "the stream starts at the ring's earliest");
    let mut all_bytes = decode_b64(first_data["bytesBase64"].as_str().expect("bytesBase64"));

    let end = conn
        .wait_for(
            |v| {
                v.get("method").and_then(Value::as_str) == Some("sub.end")
                    && v["params"]["subscriptionId"].as_str() == Some(sub_id.as_str())
            },
            F4_SUB_WAIT_BUDGET,
            "the slow_consumer terminal",
        )
        .await;
    assert_eq!(end["params"]["terminal"], "slow_consumer", "{end}");
    let resume = end["params"]["resumeCursor"]
        .as_str()
        .expect("slow_consumer carries a resumeCursor")
        .to_owned();
    // The terminals stream's cursor names the last delivered frame's START
    // offset; the no-dup resume point is that frame's nextCursor (the
    // terminal_attach consumer's rule).
    assert_eq!(
        resume,
        format!("{epoch}:{from0}"),
        "the resume cursor must name the last delivered frame's start"
    );

    // The resumed subscription first (its frames queue while the probes are
    // fired), then the T7-ized control probes, then the manual drain loop.
    let open2 = conn
        .sub_open(
            "terminals.output",
            json!({
                "terminalId": terminal_id,
                "cursor": {"outputEpoch": epoch, "byteOffset": next0}
            }),
            64,
        )
        .await;
    let sub2_id = open2["result"]["subscriptionId"]
        .as_str()
        .expect("subscriptionId")
        .to_owned();
    let get_id = conn.fresh_id("ctl");
    let get_sent = Instant::now();
    conn.send(&rpc_request(
        &get_id,
        "run.get",
        json!({"input": {"runId": p1_id}}),
    ))
    .await;
    let cancel_id = conn.fresh_id("ctl");
    let cancel_sent = Instant::now();
    conn.send(&rpc_request(
        &cancel_id,
        "run.cancel",
        json!({"input": {"runId": p1_id}, "idempotencyKey": "f4-term-cancel-p1"}),
    ))
    .await;
    let mut probes: Vec<(String, Instant, bool)> =
        vec![(get_id, get_sent, false), (cancel_id, cancel_sent, true)];

    let mut rx = conn.take_rx();
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    let mut exit_data: Option<Value> = None;
    let mut closed = false;
    let end2 = loop {
        let frame = tokio::time::timeout(F4_DRAIN_FRAME_BUDGET, rx.recv_frame())
            .await
            .expect("the resumed terminal stream must keep flowing")
            .unwrap_or_else(|| {
                panic!(
                    "api connect stdout closed; stderr:\n{}",
                    conn.stderr_log.lock().unwrap()
                )
            });
        // Probe responses ride the control lane; they must never queue
        // behind the byte flood.
        if let Some(id) = frame.get("id").and_then(Value::as_str) {
            if let Some(pos) = probes.iter().position(|(pid, _, _)| pid == id) {
                let (_, sent, is_cancel) = probes.remove(pos);
                let elapsed = sent.elapsed();
                assert!(
                    elapsed <= PROBE_LATENCY_GUARD,
                    "control probe latency during the byte flood: {elapsed:?}"
                );
                assert!(
                    frame.get("error").is_none() && frame["result"].is_object(),
                    "control probe failed: {frame}; stderr:\n{}",
                    conn.stderr_log.lock().unwrap()
                );
                if is_cancel {
                    assert_eq!(
                        frame["result"]["result"]["receipt"]["disposition"], "requested",
                        "{frame}"
                    );
                } else {
                    assert!(
                        frame["result"]["result"]["run"]["state"].as_str().is_some(),
                        "{frame}"
                    );
                }
            }
            continue;
        }
        let is_sub2 = frame["params"]["subscriptionId"].as_str() == Some(sub2_id.as_str());
        match frame.get("method").and_then(Value::as_str) {
            Some("sub.event") if is_sub2 => {
                let data = frame["params"]["event"]["data"].clone();
                match data["type"].as_str().unwrap_or_default() {
                    "bytes" => {
                        let from = data["cursor"]["byteOffset"].as_u64().expect("from");
                        let next = data["nextCursor"]["byteOffset"].as_u64().expect("next");
                        assert!(from < next, "empty bytes frame: {data}");
                        ranges.push((from, next));
                        all_bytes.extend(decode_b64(
                            data["bytesBase64"].as_str().expect("bytesBase64"),
                        ));
                        // All retained bytes delivered: close the terminal so
                        // the stream ends through the exit frame (the exit
                        // frame follows the last retained byte).
                        if next >= latest0 && !closed {
                            closed = true;
                            let receipt = expect_ok(
                                "terminals.close",
                                &await_cli(
                                    root,
                                    &[
                                        "terminal",
                                        "close",
                                        "--output",
                                        "json",
                                        "--terminal-id",
                                        &terminal_id,
                                        "--expected-server-epoch",
                                        &server_epoch,
                                        "--mode",
                                        "force",
                                    ],
                                ),
                            );
                            assert_eq!(receipt["kind"], "operation", "{receipt}");
                        }
                    }
                    "exit" => {
                        exit_data = Some(data);
                    }
                    "heartbeat" => {}
                    "gap" => panic!("the static retained window must not gap mid-drain: {data}"),
                    other => panic!("unexpected terminals.output frame `{other}`: {data}"),
                }
            }
            Some("sub.end") if is_sub2 => break frame,
            _ => {}
        }
    };
    assert_eq!(end2["params"]["terminal"], "completed", "{end2}");
    let exit = exit_data.expect("the exit frame must precede the completed terminal");
    assert_eq!(exit["type"], "exit", "{exit}");
    assert!(
        exit["exitCode"].is_number() || exit["exitSignal"].is_string(),
        "the exit frame carries the exit facts: {exit}"
    );
    assert!(
        probes.is_empty(),
        "every control probe must have been answered"
    );

    // No dup / no loss, byte-exact: the delivered ranges tile the retained
    // window [earliest0, latest0] with the exit frame at its end.
    assert_eq!(
        exit["cursor"]["byteOffset"].as_u64(),
        Some(latest0),
        "{exit}"
    );
    let mut expected_from = next0;
    for (from, next) in &ranges {
        assert_eq!(
            *from, expected_from,
            "the resumed stream must continue exactly where the delivered one stopped"
        );
        expected_from = *next;
    }
    assert_eq!(
        expected_from, latest0,
        "the delivered bytes must cover the retained window"
    );
    let delivered_bytes = all_bytes.len() as u64;
    assert_eq!(
        delivered_bytes,
        latest0 - from0,
        "byte counts must match the delivered ranges exactly"
    );
    // Content integrity across the two subscription generations: the bulk is
    // all 'x' and the marker appears exactly once.
    let text = String::from_utf8_lossy(&all_bytes);
    assert_eq!(
        text.matches(F4_BULK_MARKER).count(),
        1,
        "the marker must survive the slow-consumer cycle exactly once"
    );
    assert!(
        all_bytes.iter().filter(|&&b| b == b'x').count() >= F4_BULK_BYTES,
        "the bulk bytes must all be delivered: {} of {}",
        all_bytes.iter().filter(|&&b| b == b'x').count(),
        F4_BULK_BYTES
    );

    conn.drain_channel_to_journal();
    conn.assert_subscription_accounting();
    conn.close_stdin().await;
    conn.wait_exit(0, "clean exit after F4 terminals").await;
}

// ---------------------------------------------------------------------------
// The notifications half
// ---------------------------------------------------------------------------

/// One T7-ized control probe fired at a delivery milestone of the resumed
/// notifications drain.
#[derive(Clone, Debug)]
enum ProbeKind {
    Get {
        run_id: String,
    },
    Cancel {
        run_id: String,
    },
    Respond {
        interaction_id: String,
        revision: String,
    },
}

fn ncur_seq(cursor: &str) -> i64 {
    cursor
        .strip_prefix("ncur-")
        .and_then(|rest| rest.parse::<i64>().ok())
        .unwrap_or(-1)
}

/// F4 notifications half: manufacture >1024 pending notifications (1099
/// cancelled sleepers + 1 pending question interaction), starve a credit-1
/// `notifications.events` subscription until the gateway's item watermark
/// (1024 buffered) ends it `slow_consumer` with a resume cursor, then drain
/// the resumed subscription with milestone control probes (run.get /
/// run.cancel / interaction.respond, all within the 2 s guard) and reconcile
/// the delivered set exactly: 1102 stop + 1 action_required, unique
/// notificationIds, strictly increasing cursors, nothing pending afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn f4_notifications_flood_slow_consumer_control_latency_resume() {
    let home = TestHome::new("f4-ntf");
    let root = home.path();
    init_home(root);

    // Three sessions: the notification flood (1100 sleepers, one executing),
    // the probe sleepers (one running, one queued), and the question run the
    // respond probe answers.
    let (_, flood_session) = seed_project_session(root, "f4n").await;
    let (_, probe_session) = seed_project_session(root, "f4np").await;
    let (_, question_session) = seed_project_session(root, "f4nq").await;

    let p1 = submit_run(root, &probe_session, "[pd-driver:sleep=600000]", "f4n-p1").await;
    let p1_id = p1["runId"].as_str().expect("runId").to_owned();
    let p2 = submit_run(root, &probe_session, "[pd-driver:sleep=600000]", "f4n-p2").await;
    let p2_id = p2["runId"].as_str().expect("runId").to_owned();
    let question = submit_run(root, &question_session, "[pd-driver:question]", "f4n-q").await;
    let question_id = question["runId"].as_str().expect("runId").to_owned();

    // Resolve the pending question interaction before the flood machinery.
    // The resolution window is full-suite headroom only: the criterion (the
    // question run must be waiting on its interaction) is unchanged.
    let mut interaction: Option<(String, String)> = None;
    let resolve_deadline = Instant::now() + F4_RESOLVE_BUDGET;
    let mut poll_gap = Duration::from_millis(150);
    while Instant::now() < resolve_deadline && interaction.is_none() {
        let listed = await_cli(
            root,
            &[
                "interaction",
                "list",
                "--output",
                "json",
                "--kind",
                "question",
                "--state",
                "pending",
            ],
        );
        let Ok(parsed) = serde_json::from_str::<Value>(listed.stdout.trim()) else {
            back_off(&mut poll_gap).await;
            continue;
        };
        for item in parsed["result"]["items"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            if item["runId"].as_str() == Some(question_id.as_str()) {
                interaction = Some((
                    item["interactionId"]
                        .as_str()
                        .expect("interactionId")
                        .to_owned(),
                    item["revision"].as_str().expect("revision").to_owned(),
                ));
            }
        }
        if interaction.is_none() {
            back_off(&mut poll_gap).await;
        }
    }
    let (interaction_id, interaction_rev) =
        interaction.expect("the question run must be waiting on its interaction");

    let mut conn = ConnectSession::start(root).await;
    conn.initialize().await;

    // The flood: 1100 sleepers through the persistent session (run 0
    // executes and sleeps; the rest queue behind it), then 1099 queued-run
    // cancels — each cancellation appends run.cancelled synchronously, which
    // the notifications stream derives into exactly one stop notification.
    // Flood calls use the full-suite per-RPC budget (see `call_with_budget`).
    const FLOOD_RUNS: usize = 1100;
    let mut flood_ids: Vec<String> = Vec::with_capacity(FLOOD_RUNS);
    for index in 0..FLOOD_RUNS {
        let key = format!("f4n-s-{index}");
        let receipt = call_with_budget(
            &mut conn,
            "run.submit",
            json!({
                "sessionId": flood_session,
                "input": [{"kind": "text", "text": "[pd-driver:sleep=600000]"}]
            }),
            Some(key.as_str()),
        )
        .await;
        assert!(
            receipt.get("result").is_some(),
            "flood submit {index}: {receipt}"
        );
        flood_ids.push(
            receipt["result"]["result"]["runId"]
                .as_str()
                .expect("runId")
                .to_owned(),
        );
    }
    for (index, run_id) in flood_ids.iter().enumerate().skip(1) {
        let receipt = call_with_budget(
            &mut conn,
            "run.cancel",
            json!({"runId": run_id}),
            Some(&format!("f4-flood-cancel-{index}")),
        )
        .await;
        assert!(
            receipt.get("result").is_some(),
            "flood cancel {index}: {receipt}"
        );
    }

    // THE INJECTION: one credit against 1100 pending notifications. The
    // gateway's pump buffers 256 per fetch; after the fifth fetch the buffer
    // crosses the 1024-item watermark and ends the subscription.
    let open = conn
        .sub_open(
            "notifications.events",
            json!({"scope": {"kind": "global"}}),
            1,
        )
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
            F4_SUB_WAIT_BUDGET,
            "the one credited notification",
        )
        .await;
    let first_event = first["params"]["event"].clone();
    let first_cursor = first_event["cursor"].as_str().expect("cursor").to_owned();
    let first_data = first_event["data"].clone();
    let first_seq = ncur_seq(&first_cursor);
    assert!(
        first_seq > 0,
        "notification cursors are ncur-<seq>: {first_event}"
    );
    assert_eq!(
        first_data["cursor"].as_str(),
        Some(first_cursor.as_str()),
        "{first_data}"
    );

    let end = conn
        .wait_for(
            |v| {
                v.get("method").and_then(Value::as_str) == Some("sub.end")
                    && v["params"]["subscriptionId"].as_str() == Some(sub_id.as_str())
            },
            F4_SUB_WAIT_BUDGET,
            "the slow_consumer terminal",
        )
        .await;
    assert_eq!(end["params"]["terminal"], "slow_consumer", "{end}");
    let resume = end["params"]["resumeCursor"]
        .as_str()
        .expect("slow_consumer carries a resumeCursor")
        .to_owned();
    assert_eq!(resume, first_cursor, "{end}");

    // The resumed drain with milestone control probes (T7 旅程化). Expected
    // total: 1099 flood cancels + p1 + p2 + the question completion (all
    // stop) + the question's action_required = 1103, of which 1 was already
    // delivered before the slow consumer.
    const EXPECTED_TOTAL: usize = 1099 + 3 + 1;
    let milestones: Vec<(usize, ProbeKind)> = vec![
        (
            50,
            ProbeKind::Get {
                run_id: p1_id.clone(),
            },
        ),
        (
            200,
            ProbeKind::Cancel {
                run_id: p2_id.clone(),
            },
        ),
        (
            450,
            ProbeKind::Get {
                run_id: p1_id.clone(),
            },
        ),
        (
            700,
            ProbeKind::Cancel {
                run_id: p1_id.clone(),
            },
        ),
        (
            950,
            ProbeKind::Respond {
                interaction_id: interaction_id.clone(),
                revision: interaction_rev.clone(),
            },
        ),
    ];
    let open2 = conn
        .sub_open(
            "notifications.events",
            json!({"scope": {"kind": "global"}, "afterCursor": resume}),
            300,
        )
        .await;
    let sub2_id = open2["result"]["subscriptionId"]
        .as_str()
        .expect("subscriptionId")
        .to_owned();
    let mut rx = conn.take_rx();

    // The first-credit event counts toward the delivered totals.
    let mut delivered = 0usize;
    let mut last_seq = first_seq;
    let mut notification_ids: HashSet<String> = HashSet::new();
    let mut families: Vec<String> = Vec::new();
    let mut stop_subjects: HashSet<String> = HashSet::new();
    let mut action_subjects: Vec<String> = Vec::new();
    {
        let family = first_data["eventType"].as_str().expect("eventType");
        families.push(family.to_owned());
        assert!(
            notification_ids.insert(
                first_data["notificationId"]
                    .as_str()
                    .expect("notificationId")
                    .to_owned()
            ),
            "no notification may be delivered twice"
        );
        match family {
            "stop" => {
                stop_subjects.insert(
                    first_data["subjectId"]
                        .as_str()
                        .expect("subjectId")
                        .to_owned(),
                );
            }
            "action_required" => {
                action_subjects.push(
                    first_data["subjectId"]
                        .as_str()
                        .expect("subjectId")
                        .to_owned(),
                );
            }
            other => panic!("unexpected notification family `{other}`: {first_data}"),
        }
    }
    let mut next_milestone = 0usize;
    let mut pending: Vec<(String, Instant, ProbeKind)> = Vec::new();
    // Full-suite headroom for the whole credit-paced drain (w12 drained this
    // in well under a minute on an idle machine; the criterion — every
    // expected notification delivered exactly once — is unchanged).
    let drain_deadline = Instant::now() + F4_FLOOD_DRAIN_BUDGET;
    loop {
        if delivered + 1 >= EXPECTED_TOTAL && pending.is_empty() {
            break;
        }
        let remaining = drain_deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "the notifications drain timed out");
        let frame = tokio::time::timeout(remaining, rx.recv_frame())
            .await
            .expect("the resumed notifications stream must keep flowing")
            .unwrap_or_else(|| {
                panic!(
                    "api connect stdout closed; stderr:\n{}",
                    conn.stderr_log.lock().unwrap()
                )
            });
        if let Some(id) = frame.get("id").and_then(Value::as_str) {
            if let Some(pos) = pending.iter().position(|(pid, _, _)| pid == id) {
                let (_, sent, probe) = pending.remove(pos);
                let elapsed = sent.elapsed();
                assert!(
                    elapsed <= PROBE_LATENCY_GUARD,
                    "control probe {probe:?} latency during the notification flood: {elapsed:?}"
                );
                assert!(
                    frame.get("error").is_none() && frame["result"].is_object(),
                    "control probe failed: {frame}; stderr:\n{}",
                    conn.stderr_log.lock().unwrap()
                );
                match probe {
                    ProbeKind::Get { .. } => {
                        assert!(
                            frame["result"]["result"]["run"]["state"].as_str().is_some(),
                            "{frame}"
                        );
                    }
                    ProbeKind::Cancel { .. } => {
                        assert_eq!(
                            frame["result"]["result"]["receipt"]["disposition"], "requested",
                            "{frame}"
                        );
                    }
                    ProbeKind::Respond { .. } => {
                        assert_eq!(
                            frame["result"]["result"]["interaction"]["state"], "answered",
                            "{frame}"
                        );
                    }
                }
            }
            continue;
        }
        let is_sub2 = frame["params"]["subscriptionId"].as_str() == Some(sub2_id.as_str());
        match frame.get("method").and_then(Value::as_str) {
            Some("sub.event") if is_sub2 => {
                delivered += 1;
                let event = &frame["params"]["event"];
                let cursor = event["cursor"].as_str().expect("cursor");
                let seq = ncur_seq(cursor);
                assert!(
                    seq > last_seq,
                    "notification cursors must strictly increase: {cursor} after {last_seq}"
                );
                last_seq = seq;
                let data = &event["data"];
                assert_eq!(data["cursor"].as_str(), Some(cursor), "{data}");
                assert_eq!(data["deliveryState"], "available", "{data}");
                let notification_id = data["notificationId"].as_str().expect("notificationId");
                assert!(
                    notification_ids.insert(notification_id.to_owned()),
                    "no notification may be delivered twice: {notification_id}"
                );
                let family = data["eventType"].as_str().expect("eventType");
                match family {
                    "stop" => {
                        stop_subjects
                            .insert(data["subjectId"].as_str().expect("subjectId").to_owned());
                    }
                    "action_required" => {
                        action_subjects
                            .push(data["subjectId"].as_str().expect("subjectId").to_owned());
                    }
                    other => panic!("unexpected notification family `{other}`: {data}"),
                }
                families.push(family.to_owned());
                // Pace the drain: top the credit up with exactly what was
                // consumed (the t6/t7 rule).
                if delivered.is_multiple_of(100) {
                    conn.grant_credit(&sub2_id, 100).await;
                }
                // Milestone dispatch by delivered count.
                while next_milestone < milestones.len() && delivered >= milestones[next_milestone].0
                {
                    let (_, probe) = milestones[next_milestone].clone();
                    let id = conn.fresh_id("ctl");
                    let request = match &probe {
                        ProbeKind::Get { run_id } => {
                            rpc_request(&id, "run.get", json!({"input": {"runId": run_id}}))
                        }
                        ProbeKind::Cancel { run_id } => rpc_request(
                            &id,
                            "run.cancel",
                            json!({
                                "input": {"runId": run_id},
                                "idempotencyKey": format!("f4n-probe-cancel-{next_milestone}")
                            }),
                        ),
                        ProbeKind::Respond {
                            interaction_id,
                            revision,
                        } => rpc_request(
                            &id,
                            "interaction.respond",
                            json!({
                                "input": {
                                    "interactionId": interaction_id,
                                    "expectedRevision": revision,
                                    "answer": {"text": "f4 notifications probe answer"}
                                }
                            }),
                        ),
                    };
                    let sent = Instant::now();
                    conn.send(&request).await;
                    pending.push((id, sent, probe));
                    next_milestone += 1;
                }
            }
            Some("sub.end") if is_sub2 => {
                panic!("the notification log must not terminate on its own: {frame}");
            }
            _ => {}
        }
    }

    // The full delivered set (first credit + resumed drain) reconciles
    // exactly: 1102 stop + 1 action_required, the expected subjects, and
    // nothing else pending.
    assert_eq!(
        delivered + 1,
        EXPECTED_TOTAL,
        "the delivered total must be exactly the expected notification set"
    );
    assert_eq!(
        families.iter().filter(|f| f.as_str() == "stop").count(),
        EXPECTED_TOTAL - 1,
        "stop family count"
    );
    assert_eq!(action_subjects, vec![interaction_id.clone()]);
    let mut expected_subjects: HashSet<String> = flood_ids.iter().skip(1).cloned().collect();
    expected_subjects.insert(p1_id.clone());
    expected_subjects.insert(p2_id.clone());
    expected_subjects.insert(question_id.clone());
    assert_eq!(
        stop_subjects, expected_subjects,
        "the stop subjects must be exactly the cancelled/completed runs"
    );

    // Nothing else is pending: a bounded quiet window over the taken channel
    // (`expect_no_frames` cannot be used after `take_rx` — the session's own
    // channel was swapped out).
    let quiet = tokio::time::timeout(QUIET_WINDOW, rx.recv_frame()).await;
    if let Ok(Some(frame)) = quiet {
        panic!("expected no further notifications, got: {frame}");
    }

    conn.drain_channel_to_journal();
    conn.assert_subscription_accounting();
    conn.close_stdin().await;
    conn.wait_exit(0, "clean exit after F4 notifications").await;
}
