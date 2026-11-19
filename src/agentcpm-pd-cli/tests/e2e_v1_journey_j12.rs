//! S5 journey J12: 队列干预与运行中引导（queue / steer / run / subagent）。
//!
//! 一条完整业务旅程驱动真实 `agentcpm-pd` 二进制（隔离 `AGENTCPM_PD_HOME`、
//! 内嵌 gateway 经真实 `/v1/ws` 传输、`--output json` envelope 逐步过
//! schema 校验），对应 `_tmp/20261119-s5/journeys-execution-plan.md` 的
//! J12 步骤序列：
//!
//! 1. 同会话连提 3 run（1 个 `[pd-driver:fail]` + 2 个 sleep，头 run 先到
//!    running）→ `queue get` 断言成员序与 revision（R0）。
//! 2. `queue pause`（user_hold）：当前 run 继续、暂停边界生效（后续
//!    `run submit` 被 `QUEUE_PAUSED` 拒绝且零 run 落地）。
//! 3. 暂停中干预：`queue move`（重排队断言序）→ `queue promote-to-steer`
//!    （排队项原子转 active run 的引导）。
//! 4. `steer submit`（另一会话 `[pd-driver:await_steer]` 的 run 消费引导）
//!    → `steer get` 报 consumed；回到主会话对被转换的 pending steer 走
//!    撤销分支（`steer withdraw`，运行中 run 不受影响）。
//! 5. `queue resume` → `queue remove`（queued → cancelled，重排队断言序）
//!    → `run cancel` 头 run → 失败 run 触发 `pauseReason=run_failed`。
//! 6. 队列状态机落盘：SIGKILL gateway 后经下一命令自动重启，`queue get`
//!    的 paused/pauseReason/revision 与进程死亡前逐字段一致（状态只能来
//!    自 `v1/state.sqlite`）；`v1/state.sqlite` 存在。
//! 7. 故障负例（F7 的队列 CAS 切片）：用旅程最初的过期 expectedRevision
//!    `queue resume` → 非零退出 + `REVISION_CONFLICT` + 零副作用，再以
//!    当前 revision 正确 resume。
//! 8. 取消后重提交：新 run 正常受理并跑至 succeeded。
//! 9. `subagent types/list/history`：以存储 API 的联调点（285 操作合同无
//!    spawn 操作）在父 run 下登记子代理后经 CLI 观察。
//!
//! 判据：每步 stdout envelope 过 `expect_ok`；队列修订在每次合法变更后
//! 严格单调递增；旅程末尾对全部 run 的终态逐一核对（终态一致）。
//!
//! 本地未跑（2026-11-18 起本机构建冻结令），待集成者 cb8 验证。

// The suite spawns real gateways through the CLI's embedded-gateway feature;
// without it there is nothing to test.
#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

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
        .unwrap_or_else(|e| panic!("`{method}` stdout is not JSON ({e}): {}", out.stdout));
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

async fn run_state_of(home: &Path, run_id: &str) -> String {
    let out = await_cli(
        home,
        &["run", "get", "--output", "json", "--run-id", run_id],
    );
    let got = expect_ok("run.get", &out);
    got["run"]["state"].as_str().unwrap().to_owned()
}

async fn run_of(home: &Path, run_id: &str) -> Value {
    let out = await_cli(
        home,
        &["run", "get", "--output", "json", "--run-id", run_id],
    );
    expect_ok("run.get", &out)["run"].clone()
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

/// The session's run inventory through `run list` (zero-side-effect
/// accounting for the paused-boundary rejection).
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

/// The numeric sequence inside a `rev-%08d` revision (monotonicity basis).
fn revision_seq(revision: &str) -> u64 {
    revision
        .trim_start_matches("rev-")
        .parse()
        .unwrap_or_else(|e| panic!("unparseable queue revision `{revision}`: {e}"))
}

/// Records one queue-revision observation; every recorded revision after the
/// first must be strictly greater than the previous one (mutation monotonic
/// face of the「队列状态机落盘」criterion — same-value reads are simply not
/// recorded).
fn note_revision(revisions: &mut Vec<u64>, revision: &str, what: &str) {
    let seq = revision_seq(revision);
    if let Some(last) = revisions.last() {
        assert!(
            seq > *last,
            "queue revision must advance at `{what}`: rev-{last:08} -> {revision}"
        );
    }
    revisions.push(seq);
}

/// Opens the instance's store in-process to register the subagent (the
/// documented联调点: no spawn operation exists in the 285-op contract, so
/// subagent rows land through the store API the future spawn path will call).
/// Opened only while every run in the instance is terminal, so the startup
/// recovery pass of `open_and_recover` observes nothing to interrupt.
fn seeding_store(root: &Path) -> std::sync::Arc<pilotdeck_gateway::public_v1::store::V1Store> {
    use pilotdeck_gateway::public_v1::store::{InstanceMeta, V1Store};

    let raw = std::fs::read_to_string(root.join("local.json")).expect("local.json");
    let local: Value = serde_json::from_str(&raw).expect("parse local.json");
    let ready = std::fs::read_to_string(root.join("run").join("ready.json")).expect("ready.json");
    let ready: Value = serde_json::from_str(&ready).expect("parse ready.json");
    let meta = InstanceMeta {
        instance_id: local["instanceId"].as_str().unwrap().to_owned(),
        owner_id: local["ownerId"].as_str().unwrap().to_owned(),
        product: local["product"].as_str().unwrap().to_owned(),
        protocol_version: local["protocolVersion"].as_str().unwrap().to_owned(),
        node_id: local["nodeId"].as_str().unwrap().to_owned(),
        generation: ready["generation"].as_str().unwrap().to_owned(),
        server_revision: ready["serverRevision"].as_str().unwrap().to_owned(),
        initialization_revision: local["initializationRevision"].as_str().unwrap().to_owned(),
    };
    V1Store::open_and_recover(root, meta).expect("open seeding store")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_intervention_and_mid_run_steering_journey() {
    let home = TestHome::new("j12-queue-steer");
    let root = home.path();
    init_home(root);
    let (project_id, session_id) = seed_project_session(root, "j12a").await;
    // The steer-consumption face needs a run that starts immediately, so the
    // awaiting run lives in its own session (a busy session would queue it).
    let (_, other_session) = seed_project_session(root, "j12b").await;
    let mut revisions: Vec<u64> = Vec::new();

    // ------------------------------------------------------------------
    // 1. 同会话连提 3 run：头 run 运行中，1 个 fail + 2 个 sleep 排队。
    // ------------------------------------------------------------------
    let head = submit_run(root, &session_id, "[pd-driver:sleep=60000]", "j12-h").await;
    let head_id = head["runId"].as_str().unwrap().to_owned();
    wait_running(root, &head_id).await;

    let fail_run = submit_run(root, &session_id, "[pd-driver:fail]", "j12-f").await;
    let fail_id = fail_run["runId"].as_str().unwrap().to_owned();
    let sleep_run = submit_run(root, &session_id, "[pd-driver:sleep=30000]", "j12-s").await;
    let sleep_id = sleep_run["runId"].as_str().unwrap().to_owned();

    let queue = queue_of(root, &session_id).await;
    assert_eq!(queue["paused"], false);
    assert_eq!(queue["activeRunId"], json!(head_id));
    assert_eq!(
        queue_item_ids(&queue),
        vec![head_id.clone(), fail_id.clone(), sleep_id.clone()]
    );
    let r0 = queue["revision"].as_str().unwrap().to_owned();
    revisions.push(revision_seq(&r0));

    // ------------------------------------------------------------------
    // 2. queue.pause：当前 run 继续、暂停边界生效。
    // ------------------------------------------------------------------
    let out = queue_cmd(
        root,
        "pause",
        &[
            "--session-id",
            &session_id,
            "--expected-revision",
            &r0,
            "--reason",
            "user_hold",
        ],
    );
    let paused = expect_ok("queue.pause", &out);
    assert_eq!(paused["queue"]["paused"], true);
    assert_eq!(paused["queue"]["pauseReason"], "user_hold");
    let r1 = paused["queue"]["revision"].as_str().unwrap().to_owned();
    note_revision(&mut revisions, &r1, "queue.pause");
    assert_eq!(run_state_of(root, &head_id).await, "running");

    // 暂停边界：后续 run.submit 被拒且零 run 落地。
    let before_boundary = session_run_ids(root, &project_id, &session_id).await;
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
            "j12-blocked",
            "--text",
            "must not land while paused",
        ],
    );
    assert_eq!(expect_fail("run.submit", &out), "QUEUE_PAUSED");
    assert_eq!(
        session_run_ids(root, &project_id, &session_id).await,
        before_boundary,
        "the rejected submit must not create a run"
    );
    assert_eq!(
        queue_of(root, &session_id).await["revision"]
            .as_str()
            .unwrap(),
        r1,
        "the rejected submit must not touch the queue revision"
    );

    // ------------------------------------------------------------------
    // 3. 暂停中的干预：move 重排，promote-to-steer 原子转引导。
    // ------------------------------------------------------------------
    let out = queue_cmd(
        root,
        "move",
        &[
            "--session-id",
            &session_id,
            "--run-id",
            &sleep_id,
            "--expected-revision",
            &r1,
            "--before-run-id",
            &fail_id,
        ],
    );
    let moved = expect_ok("queue.move", &out);
    assert_eq!(
        queue_item_ids(&moved["queue"]),
        vec![head_id.clone(), sleep_id.clone(), fail_id.clone()],
        "move 必须把 sleep 提到 fail 之前"
    );
    let r2 = moved["queue"]["revision"].as_str().unwrap().to_owned();
    note_revision(&mut revisions, &r2, "queue.move");

    // promote：排队项原子转为头 run 的引导输入。
    let out = queue_cmd(
        root,
        "promote-to-steer",
        &[
            "--session-id",
            &session_id,
            "--queued-run-id",
            &sleep_id,
            "--active-run-id",
            &head_id,
            "--expected-revision",
            &r2,
            "--steer-id",
            "st-j12-promoted",
        ],
    );
    let promoted = expect_ok("queue.promote_to_steer", &out);
    assert_eq!(promoted["steerId"], "st-j12-promoted");
    assert_eq!(promoted["queuedRun"]["state"], "cancelled");
    assert_eq!(promoted["queuedRun"]["terminal"], true);
    assert_eq!(promoted["queuedRun"]["outcomeCode"], "CONVERTED_TO_STEER");
    let r3 = promoted["queueRevision"].as_str().unwrap().to_owned();
    note_revision(&mut revisions, &r3, "queue.promote_to_steer");
    assert_eq!(
        queue_item_ids(&queue_of(root, &session_id).await),
        vec![head_id.clone(), fail_id.clone()],
        "转换后的排队项必须出队"
    );

    // ------------------------------------------------------------------
    // 4. steer.submit：await_steer 的 run 消费引导；steer.get 报 consumed。
    // ------------------------------------------------------------------
    let awaiting = submit_run(root, &other_session, "[pd-driver:await_steer]", "j12-await").await;
    let awaiting_id = awaiting["runId"].as_str().unwrap().to_owned();
    wait_running(root, &awaiting_id).await;

    let out = await_cli(
        root,
        &[
            "steer",
            "submit",
            "--output",
            "json",
            "--run-id",
            &awaiting_id,
            "--steer-id",
            "st-j12-consumed",
            "--request-key",
            "j12-steer-consume",
            "--input",
            "[{\"kind\":\"text\",\"text\":\"redirect the awaiting run\"}]",
        ],
    );
    let steered = expect_ok("steer.submit", &out);
    assert_eq!(steered["phase"], "pending");
    let waited = expect_ok("run.wait", &wait_run(root, &awaiting_id));
    assert_eq!(waited["conditionMet"], true);
    assert_eq!(waited["run"]["state"], "succeeded");
    let out = await_cli(
        root,
        &[
            "steer",
            "get",
            "--output",
            "json",
            "--run-id",
            &awaiting_id,
            "--steer-id",
            "st-j12-consumed",
        ],
    );
    let consumed = expect_ok("steer.get", &out);
    assert_eq!(consumed["phase"], "consumed");
    assert_eq!(consumed["state"], "succeeded");

    // ------------------------------------------------------------------
    // 5. 撤销分支：被转换的 steer 仍 pending（头 run 在 sleep，不消费），
    //    steer.withdraw 撤销它且运行中 run 不受影响。
    // ------------------------------------------------------------------
    let out = await_cli(
        root,
        &[
            "steer",
            "get",
            "--output",
            "json",
            "--run-id",
            &head_id,
            "--steer-id",
            "st-j12-promoted",
        ],
    );
    let pending = expect_ok("steer.get", &out);
    assert_eq!(pending["phase"], "pending");
    let steer_rev = pending["revision"].as_str().unwrap().to_owned();

    let out = await_cli(
        root,
        &[
            "steer",
            "withdraw",
            "--output",
            "json",
            "--run-id",
            &head_id,
            "--steer-id",
            "st-j12-promoted",
            "--expected-revision",
            &steer_rev,
        ],
    );
    let withdrawn = expect_ok("steer.withdraw", &out);
    assert_eq!(withdrawn["phase"], "withdrawn");
    assert_eq!(withdrawn["state"], "cancelled");
    assert_eq!(run_state_of(root, &head_id).await, "running");

    // ------------------------------------------------------------------
    // 6. queue.resume → queue.remove：恢复调度面后移除一个排队项。
    // ------------------------------------------------------------------
    let out = queue_cmd(
        root,
        "resume",
        &[
            "--session-id",
            &session_id,
            "--expected-revision",
            &r3,
            "--acknowledged-pause-reason",
            "user_hold",
        ],
    );
    let resumed = expect_ok("queue.resume", &out);
    assert_eq!(resumed["queue"]["paused"], false);
    let r4 = resumed["queue"]["revision"].as_str().unwrap().to_owned();
    note_revision(&mut revisions, &r4, "queue.resume");

    // remove 的对象：一个尚未启动的排队项（resume 后新入队）。
    let extra = submit_run(root, &session_id, "extra queued turn", "j12-x").await;
    let extra_id = extra["runId"].as_str().unwrap().to_owned();
    assert_eq!(
        queue_item_ids(&queue_of(root, &session_id).await),
        vec![head_id.clone(), fail_id.clone(), extra_id.clone()]
    );

    let out = queue_cmd(
        root,
        "remove",
        &[
            "--session-id",
            &session_id,
            "--run-id",
            &extra_id,
            "--expected-revision",
            &r4,
        ],
    );
    let removed = expect_ok("queue.remove", &out);
    assert_eq!(removed["run"]["state"], "cancelled");
    assert_eq!(removed["run"]["terminal"], true);
    assert_eq!(removed["run"]["outcomeCode"], "CANCELLED");
    let r5 = removed["queueRevision"].as_str().unwrap().to_owned();
    note_revision(&mut revisions, &r5, "queue.remove");
    assert_eq!(
        queue_item_ids(&queue_of(root, &session_id).await),
        vec![head_id.clone(), fail_id.clone()]
    );

    // ------------------------------------------------------------------
    // 7. run.cancel 头 run → 失败 run 接续执行并触发 run_failed 暂停。
    // ------------------------------------------------------------------
    let out = await_cli(
        root,
        &[
            "run",
            "cancel",
            "--output",
            "json",
            "--run-id",
            &head_id,
            "--request-key",
            "j12-cancel-head",
        ],
    );
    expect_ok("run.cancel", &out);

    let failed_wait = wait_run(root, &fail_id);
    assert_eq!(failed_wait.code, 1, "run.wait on a failed run exits 1");

    let queue = queue_of(root, &session_id).await;
    assert_eq!(queue["paused"], true, "{queue}");
    assert_eq!(queue["pauseReason"], "run_failed");
    assert_eq!(queue["activeRunId"], Value::Null);
    assert!(
        queue_item_ids(&queue).is_empty(),
        "全部成员终态出队: {queue}"
    );
    let r6 = queue["revision"].as_str().unwrap().to_owned();
    note_revision(&mut revisions, &r6, "failed run pauses the queue");

    let failed = run_of(root, &fail_id).await;
    assert_eq!(failed["state"], "failed");
    assert_eq!(failed["terminal"], true);
    assert_eq!(failed["outcomeCode"], "DRIVER_INJECTED_FAILURE");

    // ------------------------------------------------------------------
    // 8. 队列状态机落盘：SIGKILL gateway → 自动重启 → 逐字段一致。
    //    （此刻全部 run 均终态、run_queue 为空，恢复路径不会覆写
    //    run_failed，正是「暂停态从盘上读回」的干净观测点。）
    // ------------------------------------------------------------------
    assert!(
        root.join("v1").join("state.sqlite").is_file(),
        "the gateway state store must exist on disk"
    );
    let pid = home.gateway_pid().expect("gateway pid");
    signal_pid(pid, rustix::process::Signal::KILL);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let queue = queue_of(root, &session_id).await;
    assert_eq!(queue["paused"], true, "暂停态跨进程死亡存活");
    assert_eq!(queue["pauseReason"], "run_failed");
    assert_eq!(
        queue["revision"].as_str().unwrap(),
        r6,
        "队列修订跨进程死亡不变"
    );
    assert_eq!(queue["activeRunId"], Value::Null);

    // ------------------------------------------------------------------
    // 9. 故障负例（F7 的队列 CAS 切片）：过期 expectedRevision 的
    //    queue.resume → REVISION_CONFLICT 且零副作用；再正确 resume。
    // ------------------------------------------------------------------
    let out = queue_cmd(
        root,
        "resume",
        &[
            "--session-id",
            &session_id,
            "--expected-revision",
            &r0,
            "--acknowledged-pause-reason",
            "run_failed",
        ],
    );
    assert_eq!(expect_fail("queue.resume", &out), "REVISION_CONFLICT");
    let after_stale = queue_of(root, &session_id).await;
    assert_eq!(after_stale["paused"], true, "零副作用：仍暂停");
    assert_eq!(after_stale["pauseReason"], "run_failed");
    assert_eq!(after_stale["revision"].as_str().unwrap(), r6);

    let out = queue_cmd(
        root,
        "resume",
        &[
            "--session-id",
            &session_id,
            "--expected-revision",
            &r6,
            "--acknowledged-pause-reason",
            "run_failed",
        ],
    );
    let resumed = expect_ok("queue.resume", &out);
    assert_eq!(resumed["queue"]["paused"], false);
    let r7 = resumed["queue"]["revision"].as_str().unwrap().to_owned();
    note_revision(&mut revisions, &r7, "queue.resume after run_failed");

    // ------------------------------------------------------------------
    // 10. 取消后重提交：新 run 受理并跑至 succeeded。
    // ------------------------------------------------------------------
    let after = submit_run(root, &session_id, "after the interventions", "j12-after").await;
    let after_id = after["runId"].as_str().unwrap().to_owned();
    let waited = expect_ok("run.wait", &wait_run(root, &after_id));
    assert_eq!(waited["conditionMet"], true);
    assert_eq!(waited["run"]["state"], "succeeded");

    // ------------------------------------------------------------------
    // 11. 终态一致：旅程涉及的全部 run 逐一核对。
    // ------------------------------------------------------------------
    let expected: [(String, &str, &str); 5] = [
        (head_id.clone(), "cancelled", "CANCELLED"),
        (sleep_id.clone(), "cancelled", "CONVERTED_TO_STEER"),
        (extra_id.clone(), "cancelled", "CANCELLED"),
        (fail_id.clone(), "failed", "DRIVER_INJECTED_FAILURE"),
        (after_id.clone(), "succeeded", "SUCCEEDED"),
    ];
    for (run_id, state, outcome) in expected {
        let run = run_of(root, &run_id).await;
        assert_eq!(run["state"], state, "run {run_id}: {run}");
        assert_eq!(run["terminal"], true, "run {run_id}: {run}");
        assert_eq!(run["outcomeCode"], outcome, "run {run_id}: {run}");
    }

    // ------------------------------------------------------------------
    // 12. subagent types/list/history：父 run 观察子代理。
    // ------------------------------------------------------------------
    let out = await_cli(
        root,
        &[
            "subagent",
            "types",
            "--output",
            "json",
            "--project-id",
            &project_id,
        ],
    );
    let types = expect_ok("subagent.types", &out);
    let type_ids: Vec<&str> = types["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["typeId"].as_str().unwrap())
        .collect();
    assert_eq!(
        type_ids,
        vec!["general-purpose", "explore", "plan", "verify"],
        "服务端固定类型目录"
    );
    assert_eq!(types["pageInfo"]["complete"], true);

    // 联调点：285 操作合同无 spawn 操作，子代理行经存储 API 登记在
    // 未来 spawn 将调用的同一写入口（此刻全部 run 终态，安全）。
    let store = seeding_store(root);
    store
        .register_subagent("sub-j12-1", &session_id, &after_id, "explore", "succeeded")
        .unwrap();
    store
        .append_subagent_entries(
            "sub-j12-1",
            &[
                json!({"turnId": "t-1", "role": "user", "content": [{"kind": "text", "text": "scan the queue state"}]}),
                json!({"turnId": "t-1", "role": "assistant", "content": [{"kind": "text", "text": "queue drained"}]}),
                json!({"turnId": "t-2", "role": "tool", "content": [{"kind": "text", "text": "grep ok"}], "runId": after_id, "toolCallId": "j12-call-1"}),
            ],
        )
        .unwrap();
    drop(store);

    let out = await_cli(
        root,
        &[
            "subagent",
            "list",
            "--output",
            "json",
            "--parent-session-id",
            &session_id,
            "--parent-run-id",
            &after_id,
        ],
    );
    let listed = expect_ok("subagent.list", &out);
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);
    assert_eq!(listed["items"][0]["subagentId"], "sub-j12-1");
    assert_eq!(listed["items"][0]["typeId"], "explore");
    assert_eq!(listed["items"][0]["state"], "succeeded");

    let out = await_cli(
        root,
        &[
            "subagent",
            "history",
            "--output",
            "json",
            "--parent-session-id",
            &session_id,
            "--parent-run-id",
            &after_id,
            "--subagent-id",
            "sub-j12-1",
        ],
    );
    let history = expect_ok("subagent.history", &out);
    let entries = history["items"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[2]["role"], "tool");
    assert_eq!(entries[2]["runId"], json!(after_id));
    assert_eq!(entries[2]["toolCallId"], "j12-call-1");
}
