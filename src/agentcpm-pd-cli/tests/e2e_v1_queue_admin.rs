//! t010 e2e: 会话队列干预与转引导 + 非 TUI 逐行多轮输入编排
//! （`cli.case.c005`/`c006`/`c007` 的本任务切片，
//! `cli.case.operation.queue.pause/move/remove/promote_to_steer`、
//! `cli.case.operation.input.follow` 的本任务切片）。
//!
//! Every scenario drives the real `agentcpm-pd` binary over the real `/v1/ws`
//! transport against an isolated `AGENTCPM_PD_HOME`, using the shared
//! `common_v1` harness. Scenario -> acceptance mapping:
//!
//! - `queue_pause_move_remove_roundtrip`（operation.queue.pause/move/remove，
//!   c006/c007）: pause 后当前 run 继续、后续入队被拒（暂停边界）、
//!   INSTANCE_MISMATCH 零副作用、resume 恢复；move 重排（锚定）、running 项
//!   拒绝、输入不被复制；remove 把 queued 变 cancelled 且零模型请求（无
//!   run.started）、running 项拒绝而非取消、队列修订推进。
//! - `queue_promote_to_steer_roundtrip`（operation.queue.promote_to_steer）:
//!   排队项原子转 active run 的引导（源 run cancelled/CONVERTED_TO_STEER、
//!   不再独立执行、转换输入经 steer 消费到达目标 run）；目标结束后
//!   RUN_TERMINAL 且队列不变。
//! - `input_follow_lines_and_ndjson_modes`（operation.input.follow）: lines
//!   三行三轮同 session（帧序 0..n）、EOF 无末换行最后行仅提交一次、空行
//!   INPUT_EMPTY（默认停止 / continueOnError 继续）、ndjson 显式
//!   idempotencyKey 去重回放与 MALFORMED_FRAME、批次日志持久化。
//! - `input_follow_pacing_sigint`: 每行等待本轮终态（挂起 run 阻塞下一
//!   行，steer 后继续）、Ctrl-C 只停止读取且不取消已受理 run（SIGINT 后
//!   steer 完成该 run）。
//!
//! 本地未跑（2026-11-18 构建冻结令），待集成者 cb8 验证。

// The suite spawns real gateways through the CLI's embedded-gateway feature;
// without it there is nothing to test.
#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio::sync::mpsc;

use common_v1::*;

/// Parses a failed invocation: one `outcome=failed` envelope line, a non-zero
/// exit, and the contract error code.
fn expect_fail(method: &str, out: &CliOutcome) -> String {
    assert!(
        out.code != 0,
        "`{method}` must exit non-zero on a contract failure (got {}); stderr: {}",
        out.code,
        out.stderr
    );
    let lines: Vec<&str> = out
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "`{method}` must print exactly one envelope line; stdout:\n{}",
        out.stdout
    );
    let value: Value = serde_json::from_str(lines[0])
        .unwrap_or_else(|e| panic!("`{method}` stdout is not JSON: {e}\n{}", out.stdout));
    assert_eq!(value["outcome"], "failed", "`{method}` outcome: {value}");
    value["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

fn queue_cmd(home: &Path, sub: &str, args: &[&str]) -> CliOutcome {
    let mut full = vec!["queue", sub, "--output", "json"];
    full.extend_from_slice(args);
    await_cli(home, &full)
}

async fn queue_of(home: &Path, session_id: &str) -> Value {
    let out = queue_cmd(home, "get", &["--session-id", session_id]);
    expect_ok("queue.get", &out)["queue"].clone()
}

fn queue_item_ids(queue: &Value) -> Vec<String> {
    queue["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["runId"].as_str().unwrap().to_owned())
        .collect()
}

async fn wait_running(home: &Path, run_id: &str) {
    for _ in 0..300 {
        if run_state_of(home, run_id).await == "running" {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("run `{run_id}` never reached running");
}

async fn run_state_of(home: &Path, run_id: &str) -> String {
    let out = await_cli(
        home,
        &["run", "get", "--output", "json", "--run-id", run_id],
    );
    let got = expect_ok("run.get", &out);
    got["run"]["state"].as_str().unwrap().to_owned()
}

async fn session_run_ids(home: &Path, project_id: &str, session_id: &str) -> Vec<String> {
    let out = await_cli(
        home,
        &[
            "run",
            "list",
            "--output",
            "json",
            "--scope",
            &format!(
                "{{\"kind\":\"session\",\"projectId\":\"{project_id}\",\"sessionId\":\"{session_id}\"}}"
            ),
        ],
    );
    let page = expect_ok("run.list", &out);
    page["items"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|r| r["runId"].as_str().unwrap().to_owned())
        .collect()
}

/// run 的事件类型序列（终局证据：零模型请求 = 无 run.started）。
async fn run_event_types(home: &Path, run_id: &str) -> Vec<String> {
    let out = await_cli(
        home,
        &[
            "run",
            "events",
            "--output",
            "json",
            "--run-id",
            run_id,
            "--page",
            "{\"limit\":500}",
        ],
    );
    let page = expect_ok("run.events", &out);
    page["frame"]["events"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|e| e["type"].as_str().map(str::to_owned))
        .collect()
}

async fn steer_submit_cli(home: &Path, run_id: &str, steer_id: &str, text: &str) -> Value {
    let out = await_cli(
        home,
        &[
            "steer",
            "submit",
            "--output",
            "json",
            "--run-id",
            run_id,
            "--steer-id",
            steer_id,
            "--input",
            &format!("[{{\"kind\": \"text\", \"text\": \"{text}\"}}]"),
        ],
    );
    expect_ok("steer.submit", &out)
}

// ---------------------------------------------------------------------------
// `input follow` 交互式挂具（piped stdin + 行式 stdout reader）
// ---------------------------------------------------------------------------

struct FollowCli {
    child: Child,
    stdin: Option<tokio::process::ChildStdin>,
    rx: mpsc::Receiver<String>,
}

impl FollowCli {
    async fn start(home: &Path, extra: &[&str]) -> Self {
        let mut cmd = base_command(home);
        let mut args = vec!["input", "follow", "--output", "ndjson"];
        args.extend_from_slice(extra);
        cmd.args(&args).stdin(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn input follow");
        let stdout = child.stdout.take().expect("piped stdout");
        let stdin = child.stdin.take();
        let (tx, rx) = mpsc::channel::<String>(64);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if tx.send(line).await.is_err() {
                    break;
                }
            }
        });
        Self { child, stdin, rx }
    }

    async fn write_line(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("follow stdin open");
        stdin
            .write_all(line.as_bytes())
            .await
            .expect("write follow stdin");
        stdin.write_all(b"\n").await.expect("newline");
        stdin.flush().await.expect("flush");
    }

    async fn close_stdin(&mut self) {
        self.stdin = None; // dropping the handle closes the pipe (EOF)
    }

    /// 下一帧（envelope 行）的 `result.frame`（过合同 schema）。
    async fn next_frame(&mut self, what: &str) -> Value {
        let line = tokio::time::timeout(FRAME_TIMEOUT, self.rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for follow frame ({what})"))
            .unwrap_or_else(|| panic!("follow stdout closed while waiting for {what}"));
        let envelope: Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("follow frame is not JSON ({e}): {line}"));
        registry()
            .validate_result("input.follow", &envelope["result"])
            .unwrap_or_else(|e| panic!("frame failed schema validation: {e:?}"));
        envelope["result"]["frame"].clone()
    }

    /// 静默窗口内不得出现新帧（只能用于「唯一能推进状态的事件被测试自己
    /// 扣住」的确定性场景——挂起的 await_steer run）。
    async fn expect_no_frame(&mut self, what: &str) {
        tokio::time::sleep(QUIET_WINDOW).await;
        match self.rx.try_recv() {
            Ok(line) => panic!("expected no follow frame ({what}), got: {line}"),
            Err(mpsc::error::TryRecvError::Disconnected) => {
                panic!("follow stdout closed unexpectedly ({what})")
            }
            Err(mpsc::error::TryRecvError::Empty) => {}
        }
    }

    async fn wait_exit(&mut self) -> i32 {
        tokio::time::timeout(CLI_TIMEOUT, self.child.wait())
            .await
            .expect("follow cli exit timed out")
            .expect("wait follow cli")
            .code()
            .unwrap_or(-1)
    }
}

/// 跑一整个 `input follow`（按字节喂入 stdin 即 EOF），返回全部帧与退出码。
/// stdin 按原文写入：末尾无换行即「EOF 无末换行」形态。
async fn follow_once(home: &Path, extra: &[&str], stdin: &str) -> (Vec<Value>, i32) {
    let mut cli = FollowCli::start(home, extra).await;
    {
        let mut handle = cli.stdin.take().expect("follow stdin open");
        handle
            .write_all(stdin.as_bytes())
            .await
            .expect("write follow stdin");
        handle.flush().await.expect("flush follow stdin");
        // drop(handle) 即 EOF。
    }
    let mut frames = Vec::new();
    loop {
        match tokio::time::timeout(FRAME_TIMEOUT, cli.rx.recv()).await {
            Ok(Some(line)) => {
                let envelope: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("follow frame is not JSON ({e}): {line}"));
                registry()
                    .validate_result("input.follow", &envelope["result"])
                    .unwrap_or_else(|e| panic!("frame failed schema validation: {e:?}"));
                frames.push(envelope["result"]["frame"].clone());
            }
            Ok(None) => break,
            Err(_) => panic!("timed out waiting for follow frames"),
        }
    }
    let code = cli.wait_exit().await;
    (frames, code)
}

// ---------------------------------------------------------------------------
// 场景 1：pause / move / remove
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_pause_move_remove_roundtrip() {
    let home = TestHome::new("t010-qpm");
    let root = home.path();
    init_home(root);
    let (_, session_id) = seed_project_session(root, "t010a").await;

    // 头 run 运行中 + 两个排队项（c006 串行队列）。
    let head = submit_run(root, &session_id, "[pd-driver:sleep=60000]", "t010-h").await;
    let head = head["runId"].as_str().unwrap().to_owned();
    wait_running(root, &head).await;
    let alpha = submit_run(root, &session_id, "alpha", "t010-a").await;
    let alpha = alpha["runId"].as_str().unwrap().to_owned();
    let bravo = submit_run(root, &session_id, "bravo", "t010-b").await;
    let bravo = bravo["runId"].as_str().unwrap().to_owned();

    let queue = queue_of(root, &session_id).await;
    assert_eq!(queue["paused"], false);
    assert_eq!(
        queue_item_ids(&queue),
        vec![head.clone(), alpha.clone(), bravo.clone()]
    );
    let qrev = queue["revision"].as_str().unwrap().to_owned();

    // acceptance（三个操作共有的 INSTANCE_MISMATCH 面，以 pause 为代表）：
    // envelope.target 与 params 实例不一致 → 副作用前拒绝，两端零副作用。
    {
        let mut conn = ConnectSession::start(root).await;
        let _ = conn.initialize().await;
        let mismatch = conn
            .call(
                "queue.pause",
                json!({
                    "instanceId": "local-00000000-0000-7000-8000-00000000dead",
                    "sessionId": session_id,
                    "expectedRevision": qrev,
                    "reason": "should never pause",
                }),
                None,
            )
            .await;
        assert!(
            contract_code(&mismatch) == "INSTANCE_MISMATCH",
            "mismatched instanceId must be refused: {mismatch}"
        );
        assert_eq!(mismatch["error"]["data"]["effect"], "not_applied");
        let _ = conn.child.kill().await;
    }
    let after_mismatch = queue_of(root, &session_id).await;
    assert_eq!(after_mismatch["paused"], false, "零业务副作用");
    assert_eq!(after_mismatch["revision"].as_str().unwrap(), qrev);

    // queue.pause：当前 run 继续、暂停边界生效。
    let out = queue_cmd(
        root,
        "pause",
        &[
            "--session-id",
            &session_id,
            "--expected-revision",
            &qrev,
            "--reason",
            "user_hold",
        ],
    );
    let paused = expect_ok("queue.pause", &out);
    assert_eq!(paused["queue"]["paused"], true);
    assert_eq!(paused["queue"]["pauseReason"], "user_hold");
    let paused_rev = paused["queue"]["revision"].as_str().unwrap().to_owned();
    assert_ne!(paused_rev, qrev);
    assert_eq!(run_state_of(root, &head).await, "running");

    // 后续并发入队不越过暂停边界。
    let out = await_cli(
        root,
        &[
            "run",
            "submit",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--request-key",
            "t010-blocked",
            "--text",
            "nope",
        ],
    );
    assert_eq!(expect_fail("run.submit", &out), "QUEUE_PAUSED");

    // queue.resume 恢复调度面（并推进修订）。
    let out = queue_cmd(
        root,
        "resume",
        &[
            "--session-id",
            &session_id,
            "--expected-revision",
            &paused_rev,
            "--acknowledged-pause-reason",
            "user_hold",
        ],
    );
    expect_ok("queue.resume", &out);
    let resumed_rev = queue_of(root, &session_id).await["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(resumed_rev, paused_rev);

    // queue.move：bravo 移到 alpha 之前。
    let out = queue_cmd(
        root,
        "move",
        &[
            "--session-id",
            &session_id,
            "--run-id",
            &bravo,
            "--expected-revision",
            &resumed_rev,
            "--before-run-id",
            &alpha,
        ],
    );
    let moved = expect_ok("queue.move", &out);
    assert_eq!(
        queue_item_ids(&moved["queue"]),
        vec![head.clone(), bravo.clone(), alpha.clone()]
    );
    let moved_rev = moved["queue"]["revision"].as_str().unwrap().to_owned();

    // running 项拒绝（move 与 remove 同面），拒绝而非取消。
    let out = queue_cmd(
        root,
        "move",
        &[
            "--session-id",
            &session_id,
            "--run-id",
            &head,
            "--expected-revision",
            &moved_rev,
        ],
    );
    assert_eq!(expect_fail("queue.move", &out), "RUN_ALREADY_STARTED");
    let out = queue_cmd(
        root,
        "remove",
        &[
            "--session-id",
            &session_id,
            "--run-id",
            &head,
            "--expected-revision",
            &moved_rev,
        ],
    );
    assert_eq!(expect_fail("queue.remove", &out), "RUN_ALREADY_STARTED");
    assert_eq!(run_state_of(root, &head).await, "running");

    // queue.remove：queued → cancelled，零模型请求。
    let out = queue_cmd(
        root,
        "remove",
        &[
            "--session-id",
            &session_id,
            "--run-id",
            &alpha,
            "--expected-revision",
            &moved_rev,
        ],
    );
    let removed = expect_ok("queue.remove", &out);
    assert_eq!(removed["run"]["state"], "cancelled");
    assert_eq!(removed["run"]["terminal"], true);
    assert_eq!(removed["run"]["outcomeCode"], "CANCELLED");
    let removed_rev = removed["queueRevision"].as_str().unwrap().to_owned();
    assert_ne!(removed_rev, moved_rev);
    let types = run_event_types(root, &alpha).await;
    assert_eq!(
        types,
        vec!["run.queued".to_owned(), "run.cancelled".to_owned()],
        "零模型请求：无 run.started {types:?}"
    );

    // 已移除（终态出队）的项不可再干预。
    let out = queue_cmd(
        root,
        "remove",
        &[
            "--session-id",
            &session_id,
            "--run-id",
            &alpha,
            "--expected-revision",
            &removed_rev,
        ],
    );
    assert_eq!(expect_fail("queue.remove", &out), "NOT_FOUND");

    // 移动不复制输入：bravo 仍 queued。合同面 Run/QueueItem 都不外发输入
    // 字段，「移动不得改动排队项的输入」由执行回放复核（见下方
    // run.output.delta 断言：驱动把提交原文回放进输出增量）。
    assert_eq!(run_state_of(root, &bravo).await, "queued");

    // 顺序被串行 worker 遵守：取消头后 bravo 接续执行。
    let out = await_cli(
        root,
        &[
            "run",
            "cancel",
            "--output",
            "json",
            "--run-id",
            &head,
            "--request-key",
            "t010-cancel-head",
        ],
    );
    expect_ok("run.cancel", &out);
    let waited = expect_ok("run.wait", &wait_run(root, &bravo));
    assert_eq!(waited["conditionMet"], true);
    assert_eq!(waited["run"]["state"], "succeeded");
    // bravo 以原输入执行：输出增量回放提交原文「bravo」（终局证据：
    // 移动不得改动排队项的输入）。
    let out = await_cli(
        root,
        &[
            "run",
            "events",
            "--output",
            "json",
            "--run-id",
            &bravo,
            "--page",
            "{\"limit\":500}",
        ],
    );
    let page = expect_ok("run.events", &out);
    let outputs: Vec<String> = page["frame"]["events"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter(|event| event["type"] == "run.output.delta")
        .filter_map(|event| event["payload"]["text"].as_str().map(str::to_owned))
        .collect();
    assert!(
        outputs.contains(&"completed: bravo".to_owned()),
        "bravo 必须以原输入执行（输出增量 {outputs:?}）"
    );
}

// ---------------------------------------------------------------------------
// 场景 2：promote_to_steer
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_promote_to_steer_roundtrip() {
    let home = TestHome::new("t010-pts");
    let root = home.path();
    init_home(root);
    let (_, session_id) = seed_project_session(root, "t010b").await;

    // 头 run 挂起等待引导；一个排队项（await_steer 头不会被串行 worker 推进，
    // 排队项稳定 queued）。
    let head = submit_run(root, &session_id, "[pd-driver:await_steer]", "t010-p-h").await;
    let head = head["runId"].as_str().unwrap().to_owned();
    wait_running(root, &head).await;
    let queued = submit_run(root, &session_id, "promote me", "t010-p-q").await;
    let queued = queued["runId"].as_str().unwrap().to_owned();

    // 另一会话里一个已终态的 run（「目标结束」场景）。
    let (_, other) = seed_project_session(root, "t010bo").await;
    let finished = submit_run(root, &other, "quick", "t010-p-f").await;
    let finished = finished["runId"].as_str().unwrap().to_owned();
    let done = expect_ok("run.wait", &wait_run(root, &finished));
    assert_eq!(done["run"]["state"], "succeeded");

    let queue = queue_of(root, &session_id).await;
    let qrev = queue["revision"].as_str().unwrap().to_owned();
    assert_eq!(queue_item_ids(&queue), vec![head.clone(), queued.clone()]);

    // acceptance「目标结束…队列不变」：RUN_TERMINAL，修订与成员零变化。
    let out = queue_cmd(
        root,
        "promote-to-steer",
        &[
            "--session-id",
            &session_id,
            "--queued-run-id",
            &queued,
            "--active-run-id",
            &finished,
            "--expected-revision",
            &qrev,
            "--steer-id",
            "st-dead",
        ],
    );
    assert_eq!(expect_fail("queue.promote_to_steer", &out), "RUN_TERMINAL");
    let after = queue_of(root, &session_id).await;
    assert_eq!(after["revision"].as_str().unwrap(), qrev);
    assert_eq!(queue_item_ids(&after), vec![head.clone(), queued.clone()]);

    // 主线：排队项原子转为头 run 的引导。
    let out = queue_cmd(
        root,
        "promote-to-steer",
        &[
            "--session-id",
            &session_id,
            "--queued-run-id",
            &queued,
            "--active-run-id",
            &head,
            "--expected-revision",
            &qrev,
            "--steer-id",
            "st-e2e-1",
        ],
    );
    let promoted = expect_ok("queue.promote_to_steer", &out);
    assert_eq!(promoted["steerId"], "st-e2e-1");
    assert_eq!(promoted["queuedRun"]["state"], "cancelled");
    assert_eq!(promoted["queuedRun"]["terminal"], true);
    assert_eq!(promoted["queuedRun"]["outcomeCode"], "CONVERTED_TO_STEER");
    let promoted_rev = promoted["queueRevision"].as_str().unwrap().to_owned();
    assert_ne!(promoted_rev, qrev);

    // 原项不再独立执行：出队（头 run 是否已消费完毕与断言无关）、事件流
    // 无 run.started。
    let items_now = queue_item_ids(&queue_of(root, &session_id).await);
    assert!(
        !items_now.contains(&queued),
        "转换后的排队项必须出队: {items_now:?}"
    );
    let types = run_event_types(root, &queued).await;
    assert_eq!(
        types,
        vec!["run.queued".to_owned(), "run.cancelled".to_owned()],
        "{types:?}"
    );

    // 目标 run 消费转换输入并完成；steer.get 报 consumed。
    let done = expect_ok("run.wait", &wait_run(root, &head));
    assert_eq!(done["run"]["state"], "succeeded");
    let out = await_cli(
        root,
        &[
            "steer",
            "get",
            "--output",
            "json",
            "--run-id",
            &head,
            "--steer-id",
            "st-e2e-1",
        ],
    );
    let steer = expect_ok("steer.get", &out);
    assert_eq!(steer["phase"], "consumed");
    assert_eq!(steer["state"], "succeeded");

    // 转换输入到达目标 run（run.output.delta）。
    let out = await_cli(
        root,
        &[
            "run",
            "events",
            "--output",
            "json",
            "--run-id",
            &head,
            "--page",
            "{\"limit\":500}",
        ],
    );
    let page = expect_ok("run.events", &out);
    let texts: Vec<String> = page["frame"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "run.output.delta")
        .filter_map(|e| e["data"]["text"].as_str().map(str::to_owned))
        .collect();
    assert!(
        texts.iter().any(|t| t.contains("promote me")),
        "转换输入必须到达目标 run: {texts:?}"
    );

    // 已转换（终态出队）的项不可再作为排队项。
    let out = queue_cmd(
        root,
        "promote-to-steer",
        &[
            "--session-id",
            &session_id,
            "--queued-run-id",
            &queued,
            "--active-run-id",
            &head,
            "--expected-revision",
            &promoted_rev,
            "--steer-id",
            "st-e2e-2",
        ],
    );
    assert_eq!(expect_fail("queue.promote_to_steer", &out), "NOT_FOUND");
}

// ---------------------------------------------------------------------------
// 场景 3：input.follow（lines / ndjson / 空行 / EOF 无末换行）
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_follow_lines_and_ndjson_modes() {
    let home = TestHome::new("t010-if");
    let root = home.path();
    init_home(root);
    let (project_id, session_id) = seed_project_session(root, "t010c").await;

    // lines 主线：三行三轮、同 session、帧序 0..=3（含 end）；末尾无换行。
    let (frames, code) = follow_once(
        root,
        &["--session-id", &session_id],
        "first turn\nsecond turn\nthird turn",
    )
    .await;
    assert_eq!(code, 0);
    assert_eq!(frames.len(), 4, "{frames:?}");
    let run_ids: Vec<String> = frames[..3]
        .iter()
        .map(|f| f["receipt"]["runId"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(run_ids.len(), 3);
    assert_ne!(run_ids[0], run_ids[1]);
    assert_ne!(run_ids[1], run_ids[2]);
    for (idx, frame) in frames.iter().enumerate() {
        assert_eq!(frame["sequence"].as_u64().unwrap(), idx as u64);
    }
    for frame in &frames[..3] {
        assert_eq!(frame["type"], "accepted");
        assert_eq!(frame["receipt"]["sessionId"], session_id.as_str());
        assert_eq!(frame["receipt"]["accepted"], "durable");
    }
    assert_eq!(frames[3]["type"], "end");
    // 同一会话恰好三轮（EOF 无末换行的最后行仅提交一次）。
    assert_eq!(
        session_run_ids(root, &project_id, &session_id).await.len(),
        3
    );
    // 全部到达终态（等待本轮终态的编排语义）。
    for run_id in &run_ids {
        let waited = expect_ok("run.wait", &wait_run(root, run_id));
        assert_eq!(waited["run"]["state"], "succeeded");
    }

    // 空行：INPUT_EMPTY；默认 continueOnError=false → 停止后续输入。
    let (frames, code) = follow_once(
        root,
        &["--session-id", &session_id],
        "good line\n\nnever submitted",
    )
    .await;
    assert_ne!(code, 0, "error 帧必须非零退出");
    assert_eq!(frames.len(), 3, "{frames:?}");
    assert_eq!(frames[0]["type"], "accepted");
    assert_eq!(frames[1]["type"], "error");
    assert_eq!(frames[1]["error"]["code"], "INPUT_EMPTY");
    assert_eq!(frames[2]["type"], "end");
    assert_eq!(
        session_run_ids(root, &project_id, &session_id).await.len(),
        4,
        "空行后的行不得提交"
    );

    // 空行 + continueOnError：继续提交后续行。
    let (frames, code) = follow_once(
        root,
        &["--session-id", &session_id, "--continue-on-error"],
        "kept line\n\nafter blank",
    )
    .await;
    assert_eq!(code, 1, "出现过 error 帧仍非零退出");
    assert_eq!(frames.len(), 4, "{frames:?}");
    assert_eq!(frames[1]["error"]["code"], "INPUT_EMPTY");
    assert_eq!(frames[2]["type"], "accepted");
    assert_eq!(frames[3]["type"], "end");
    assert_eq!(
        session_run_ids(root, &project_id, &session_id).await.len(),
        6
    );

    // ndjson：显式 idempotencyKey 去重回放 + MALFORMED_FRAME。
    let (frames, code) = follow_once(
        root,
        &["--session-id", &session_id, "--mode", "ndjson"],
        concat!(
            "{\"input\": [{\"kind\": \"text\", \"text\": \"nd one\"}], \"idempotencyKey\": \"t010-k1\"}\n",
            "{\"input\": [{\"kind\": \"text\", \"text\": \"nd one\"}], \"idempotencyKey\": \"t010-k1\"}\n",
            "not json"
        ),
    )
    .await;
    assert_eq!(code, 1);
    assert_eq!(frames.len(), 4, "{frames:?}");
    assert_eq!(frames[0]["type"], "accepted");
    assert_eq!(frames[1]["type"], "accepted");
    // 同 key 同输入：去重回放返回同一 runId（不重复执行）。
    assert_eq!(
        frames[0]["receipt"]["runId"].as_str().unwrap(),
        frames[1]["receipt"]["runId"].as_str().unwrap(),
        "显式 idempotencyKey 的重复记录必须去重到同一 run"
    );
    assert_eq!(frames[2]["type"], "error");
    assert_eq!(frames[2]["error"]["code"], "MALFORMED_FRAME");
    assert_eq!(frames[3]["type"], "end");
    // 两条同 key 记录只产生一个 run（6 + 1）。
    assert_eq!(
        session_run_ids(root, &project_id, &session_id).await.len(),
        7
    );

    // 幂等账本 + 批次日志持久化（恢复依赖逐项回执）。
    let journal_dir = root.join("client").join("input-follow");
    let mut journal_lines = 0usize;
    for entry in std::fs::read_dir(&journal_dir).expect("input-follow journal dir") {
        let entry = entry.expect("journal entry");
        let raw = std::fs::read_to_string(entry.path()).unwrap_or_default();
        journal_lines += raw.lines().filter(|l| !l.trim().is_empty()).count();
    }
    assert!(
        journal_lines >= 6,
        "批次日志必须持久化逐项 submitted+outcome 记录（{journal_lines}）"
    );
}

// ---------------------------------------------------------------------------
// 场景 4：input.follow 的等待终态 / Ctrl-C
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_follow_pacing_sigint() {
    let home = TestHome::new("t010-if2");
    let root = home.path();
    init_home(root);
    let (project_id, session_id) = seed_project_session(root, "t010d").await;

    // 等待本轮终态：第一行是挂起 run，第二行必须等它终态后才提交。
    let mut cli = FollowCli::start(root, &["--session-id", &session_id]).await;
    cli.write_line("[pd-driver:await_steer]").await;
    let first = cli.next_frame("first accepted frame").await;
    assert_eq!(first["type"], "accepted");
    let hanging = first["receipt"]["runId"].as_str().unwrap().to_owned();

    cli.write_line("second line").await;
    // 挂起 run 未终态：第二行不得提交（无帧、无 run）。
    cli.expect_no_frame("second line waits for the hanging turn")
        .await;
    assert_eq!(
        session_run_ids(root, &project_id, &session_id).await.len(),
        1
    );

    // Ctrl-C：只停止读取，不取消已受理 run（end 帧后以 130 退出）。
    let pid = cli.child.id().expect("follow cli pid");
    signal_pid(pid, rustix::process::Signal::INT);
    let end = cli.next_frame("end frame after SIGINT").await;
    assert_eq!(end["type"], "end");
    assert_eq!(cli.wait_exit().await, 130, "SIGINT 退出码 130");
    // 已受理 run 继续并可完成（无人取消它）。
    steer_submit_cli(root, &hanging, "st-after-sigint", "finish").await;
    let done = expect_ok("run.wait", &wait_run(root, &hanging));
    assert_eq!(
        done["run"]["state"], "succeeded",
        "Ctrl-C 不得取消已受理 run"
    );

    // 等待终态的推进面：steer 完成挂起 run 后下一行继续。
    let mut cli2 = FollowCli::start(root, &["--session-id", &session_id]).await;
    cli2.write_line("[pd-driver:await_steer]").await;
    let first = cli2.next_frame("pacing first frame").await;
    let hanging2 = first["receipt"]["runId"].as_str().unwrap().to_owned();
    cli2.write_line("after hang").await;
    cli2.expect_no_frame("pacing holds the second line").await;
    steer_submit_cli(root, &hanging2, "st-pace", "go").await;
    let second = cli2.next_frame("second line after turn terminal").await;
    assert_eq!(second["type"], "accepted");
    assert_eq!(second["sequence"], 1);
    cli2.close_stdin().await;
    let end = cli2.next_frame("end frame").await;
    assert_eq!(end["type"], "end");
    assert_eq!(cli2.wait_exit().await, 0);
}
