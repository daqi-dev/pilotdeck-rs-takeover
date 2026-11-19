//! S5 journey F7: 队列暂停恢复（多会话并发）（`_tmp/20261119-s5/journeys-execution-plan.md` §3 F7）。
//!
//! J12 §9 已有单会话的队列 CAS 切片;本文件做旅程化的两会话并发组合:
//!
//! 1. 会话 A、B 各提一个 `[pd-driver:fail]` run → 两边各自触发
//!    `pauseReason=run_failed`（R0/A0/B0 三点修订记录,暂停动作本身推进修订）。
//! 2. A 会话恢复:先用会话创建时的过期 expectedRevision `queue resume` →
//!    非零退出 + `REVISION_CONFLICT` + `effect=not_applied` 零副作用（A 仍暂停、
//!    修订不动）;再用正确修订 resume → 恢复调度,补投一个 run 跑至 succeeded。
//!    额外负例:正确修订但 acknowledgedPauseReason 报错值 → `INVALID_ARGUMENT`
//!    零副作用（恢复动作必须显式对上暂停原因）。
//! 3. B 会话走 F1 的 gateway 重启路径恢复:SIGKILL gateway（TestHome 自有子进程）
//!    → 下一命令自动重启 → B 的队列从盘上读回,pauseReason/修订/成员与死亡前
//!    逐字段一致（此刻两会话全部 run 均终态,启动恢复路径不覆写 run_failed）;
//!    A 的已恢复态同样不受重启扰动（双向不串扰）→ 再正确 resume B。
//! 4. 判据「两会话互不串扰」:A 的每次 CAS 尝试/恢复前后,B 的
//!    (paused, pauseReason, revision) 三元组保持不变,反之亦然;每次合法变更后
//!    各会话修订严格单调递增;旅程末尾对全部 run 的终态逐一核对;
//!    `v1/state.sqlite` 存在（队列状态机只能从盘上跨进程死亡存活）。
//!
//! 注入只作用于测试自有的 gateway 子进程（TestHome 拉起,gateway_pid 直读
//! run/ready.json）,绝不触碰 `~/.pilotdeck` 或按名字匹配的系统进程。
//!
//! 本地未跑（本机禁跑 cargo）,待集成者在 cb8 验证。

// The suite drives the real embedded gateway through the real binary; without
// the feature there is nothing to test.
#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use common_v1::*;

/// Parses a failed invocation: one `outcome=failed` envelope line, a non-zero
/// exit, and the contract error code（J12 同形）.
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

/// Like [`expect_fail`] but returns the whole envelope, so side-effect
/// evidence（`error.effect`）can be asserted on the same frame.
fn expect_fail_value(method: &str, out: &CliOutcome) -> Value {
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
    value
}

async fn queue_of(home: &Path, session_id: &str) -> Value {
    let out = await_cli(
        home,
        &[
            "queue",
            "get",
            "--output",
            "json",
            "--session-id",
            session_id,
        ],
    );
    expect_ok("queue.get", &out)["queue"].clone()
}

async fn run_of(home: &Path, run_id: &str) -> Value {
    let out = await_cli(
        home,
        &["run", "get", "--output", "json", "--run-id", run_id],
    );
    expect_ok("run.get", &out)["run"].clone()
}

/// One `queue resume` attempt through argv.
async fn queue_resume(home: &Path, session_id: &str, expected: &str, reason: &str) -> CliOutcome {
    await_cli(
        home,
        &[
            "queue",
            "resume",
            "--output",
            "json",
            "--session-id",
            session_id,
            "--expected-revision",
            expected,
            "--acknowledged-pause-reason",
            reason,
        ],
    )
}

/// The numeric sequence inside a `rev-%08d` revision（monotonicity basis）.
fn revision_seq(revision: &str) -> u64 {
    revision
        .trim_start_matches("rev-")
        .parse()
        .unwrap_or_else(|e| panic!("unparseable queue revision `{revision}`: {e}"))
}

/// Records one queue-revision observation per session; every recorded revision
/// after the first must be strictly greater than the previous one.
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

/// The comparable queue face for「互不串扰」: (paused, pauseReason, revision,
/// activeRunId). Items are deliberately excluded（成员变化由本会话的 run 驱动,
/// 与另一会话无关）.
fn queue_face(queue: &Value) -> (bool, Value, String, Value) {
    (
        queue["paused"] == true,
        queue["pauseReason"].clone(),
        queue["revision"].as_str().unwrap_or_default().to_owned(),
        queue["activeRunId"].clone(),
    )
}

/// F7: two sessions each paused by a failed run recover independently — A via
/// the CAS-guarded `queue resume`（stale revision first, then the correct
/// one）, B through the F1 gateway-restart path（SIGKILL → auto-restart →
/// state read back from `v1/state.sqlite` → resume）. Neither session's
/// recovery may disturb the other's queue face.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f7_two_session_pause_recovery_via_cas_and_gateway_restart() {
    let home = TestHome::new("f7-queue-recovery");
    let root = home.path();
    init_home(root);
    let (_project_a, session_a) = seed_project_session(root, "f7a").await;
    let (_project_b, session_b) = seed_project_session(root, "f7b").await;
    let mut revisions_a: Vec<u64> = Vec::new();
    let mut revisions_b: Vec<u64> = Vec::new();

    // ------------------------------------------------------------------
    // 1. 前置:暂停前的基线修订（确定性过期 CAS 来源——run_failed 暂停会推进
    //    修订,故会话创建时读到的修订此后必然过期）。
    // ------------------------------------------------------------------
    let a_pre = queue_of(root, &session_a).await;
    assert_eq!(a_pre["paused"], false, "{a_pre}");
    let r_a_pre = a_pre["revision"].as_str().unwrap().to_owned();
    revisions_a.push(revision_seq(&r_a_pre));
    let b_pre = queue_of(root, &session_b).await;
    assert_eq!(b_pre["paused"], false, "{b_pre}");
    let r_b_pre = b_pre["revision"].as_str().unwrap().to_owned();
    revisions_b.push(revision_seq(&r_b_pre));

    // ------------------------------------------------------------------
    // 2. A 会话失败 run → run_failed 暂停。
    // ------------------------------------------------------------------
    let fail_a = submit_run(root, &session_a, "[pd-driver:fail]", "f7a-fail").await;
    let fail_a_id = fail_a["runId"].as_str().unwrap().to_owned();
    let failed_wait = wait_run(root, &fail_a_id);
    assert_eq!(failed_wait.code, 1, "run.wait on a failed run exits 1");

    let a_paused = queue_of(root, &session_a).await;
    assert_eq!(a_paused["paused"], true, "{a_paused}");
    assert_eq!(a_paused["pauseReason"], "run_failed");
    assert_eq!(a_paused["activeRunId"], Value::Null, "{a_paused}");
    assert!(
        a_paused["items"].as_array().is_some_and(Vec::is_empty),
        "失败 run 终态出队: {a_paused}"
    );
    let r_a_paused = a_paused["revision"].as_str().unwrap().to_owned();
    note_revision(&mut revisions_a, &r_a_paused, "run_failed pauses session A");
    assert!(
        revision_seq(&r_a_paused) > revision_seq(&r_a_pre),
        "the pause must advance the revision, or the stale CAS below is vacuous"
    );
    let failed_a = run_of(root, &fail_a_id).await;
    assert_eq!(failed_a["state"], "failed");
    assert_eq!(failed_a["terminal"], true);
    assert_eq!(failed_a["outcomeCode"], "DRIVER_INJECTED_FAILURE");

    // ------------------------------------------------------------------
    // 3. B 会话失败 run → run_failed 暂停;A 的队列面不受 B 的失败扰动
    //    （互不串扰,正向第 1 面）。
    // ------------------------------------------------------------------
    let fail_b = submit_run(root, &session_b, "[pd-driver:fail]", "f7b-fail").await;
    let fail_b_id = fail_b["runId"].as_str().unwrap().to_owned();
    let failed_wait = wait_run(root, &fail_b_id);
    assert_eq!(failed_wait.code, 1, "run.wait on a failed run exits 1");

    let b_paused = queue_of(root, &session_b).await;
    assert_eq!(b_paused["paused"], true, "{b_paused}");
    assert_eq!(b_paused["pauseReason"], "run_failed");
    assert_eq!(b_paused["activeRunId"], Value::Null, "{b_paused}");
    assert!(
        b_paused["items"].as_array().is_some_and(Vec::is_empty),
        "失败 run 终态出队: {b_paused}"
    );
    let r_b_paused = b_paused["revision"].as_str().unwrap().to_owned();
    note_revision(&mut revisions_b, &r_b_paused, "run_failed pauses session B");
    assert_eq!(
        queue_face(&queue_of(root, &session_a).await),
        queue_face(&a_paused),
        "B 的失败不得改动 A 的队列面"
    );

    // ------------------------------------------------------------------
    // 4. A 会话恢复,第 1 跳:过期 expectedRevision 的 queue.resume →
    //    REVISION_CONFLICT 且零副作用。
    // ------------------------------------------------------------------
    let stale = queue_resume(root, &session_a, &r_a_pre, "run_failed").await;
    let stale_envelope = expect_fail_value("queue.resume", &stale);
    assert_eq!(
        stale_envelope["error"]["code"], "REVISION_CONFLICT",
        "{stale_envelope}"
    );
    assert_eq!(
        stale_envelope["error"]["effect"], "not_applied",
        "the stale resume must be refused before any side effect: {stale_envelope}"
    );
    assert_eq!(
        queue_face(&queue_of(root, &session_a).await),
        queue_face(&a_paused),
        "零副作用:A 仍按原修订暂停"
    );
    assert_eq!(
        queue_face(&queue_of(root, &session_b).await),
        queue_face(&b_paused),
        "A 的过期 CAS 不得扰动 B"
    );

    // 额外负例:修订正确但 acknowledgedPauseReason 报错值 → 恢复动作必须
    // 显式对上暂停原因,同样零副作用。
    let wrong_reason = queue_resume(root, &session_a, &r_a_paused, "gateway_restart").await;
    assert_eq!(
        expect_fail("queue.resume", &wrong_reason),
        "INVALID_ARGUMENT",
        "a mismatched acknowledgedPauseReason must be refused"
    );
    assert_eq!(
        queue_face(&queue_of(root, &session_a).await),
        queue_face(&a_paused),
        "零副作用:错误的暂停原因申报不改变队列"
    );

    // ------------------------------------------------------------------
    // 5. A 会话恢复,第 2 跳:正确修订 + 正确暂停原因 → 恢复调度面,
    //    补投 run 跑至 succeeded;B 全程保持暂停。
    // ------------------------------------------------------------------
    let resumed_a = expect_ok(
        "queue.resume",
        &queue_resume(root, &session_a, &r_a_paused, "run_failed").await,
    );
    assert_eq!(resumed_a["queue"]["paused"], false, "{resumed_a}");
    let r_a_resumed = resumed_a["queue"]["revision"].as_str().unwrap().to_owned();
    note_revision(&mut revisions_a, &r_a_resumed, "queue.resume on session A");

    let after_a = submit_run(root, &session_a, "f7a after cas recovery", "f7a-after").await;
    let after_a_id = after_a["runId"].as_str().unwrap().to_owned();
    let waited = expect_ok("run.wait", &wait_run(root, &after_a_id));
    assert_eq!(waited["conditionMet"], true);
    assert_eq!(waited["run"]["state"], "succeeded");

    let a_resumed_face = queue_face(&queue_of(root, &session_a).await);
    assert_eq!(a_resumed_face.0, false, "A 保持恢复态");
    assert_eq!(
        queue_face(&queue_of(root, &session_b).await),
        queue_face(&b_paused),
        "A 的恢复与补投不得扰动 B（互不串扰,正向第 2 面）"
    );

    // ------------------------------------------------------------------
    // 6. B 会话走 F1 的 gateway 重启路径恢复:SIGKILL → 自动重启 → 落盘读回。
    //    此刻两会话全部 run 均终态,启动恢复路径不覆写 run_failed,正是
    //    「暂停态从盘上读回」的干净观测点。
    // ------------------------------------------------------------------
    assert!(
        root.join("v1").join("state.sqlite").is_file(),
        "the gateway state store must exist on disk"
    );
    let a_pre_kill = queue_face(&queue_of(root, &session_a).await);
    let pid_before = home.gateway_pid().expect("gateway pid before the kill");
    signal_pid(pid_before, rustix::process::Signal::KILL);
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 下一命令触发 auto-start;B 的队列与进程死亡前逐字段一致（只能来自盘）。
    let b_after_restart = queue_of(root, &session_b).await;
    assert_eq!(b_after_restart["paused"], true, "B 暂停态跨进程死亡存活");
    assert_eq!(b_after_restart["pauseReason"], "run_failed");
    assert_eq!(b_after_restart["activeRunId"], Value::Null);
    assert!(
        b_after_restart["items"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "{b_after_restart}"
    );
    assert_eq!(
        b_after_restart["revision"].as_str().unwrap(),
        r_b_paused,
        "B 队列修订跨进程死亡不变（状态机与落盘一致）"
    );

    // A 的已恢复态同样不受重启扰动（双向不串扰:重启是 B 的恢复车辆,
    // 不得顺带暂停或扰动 A）。
    assert_eq!(
        queue_face(&queue_of(root, &session_a).await),
        a_pre_kill,
        "gateway 重启不得扰动 A 的队列面"
    );
    let pid_after = home.gateway_pid().expect("gateway pid after the restart");
    assert_ne!(pid_after, pid_before, "a fresh gateway process came up");

    // ------------------------------------------------------------------
    // 7. B 会话恢复,第 2 跳:重启后正确 resume → 补投 run 跑至 succeeded;
    //    A 不受影响。
    // ------------------------------------------------------------------
    let resumed_b = expect_ok(
        "queue.resume",
        &queue_resume(root, &session_b, &r_b_paused, "run_failed").await,
    );
    assert_eq!(resumed_b["queue"]["paused"], false, "{resumed_b}");
    let r_b_resumed = resumed_b["queue"]["revision"].as_str().unwrap().to_owned();
    note_revision(&mut revisions_b, &r_b_resumed, "queue.resume on session B");

    let after_b = submit_run(root, &session_b, "f7b after restart recovery", "f7b-after").await;
    let after_b_id = after_b["runId"].as_str().unwrap().to_owned();
    let waited = expect_ok("run.wait", &wait_run(root, &after_b_id));
    assert_eq!(waited["conditionMet"], true);
    assert_eq!(waited["run"]["state"], "succeeded");

    let b_resumed_face = queue_face(&queue_of(root, &session_b).await);
    assert_eq!(b_resumed_face.0, false, "B 保持恢复态");
    assert_eq!(
        queue_face(&queue_of(root, &session_a).await),
        a_pre_kill,
        "B 的恢复与补投不得扰动 A（互不串扰,反向面）"
    );

    // ------------------------------------------------------------------
    // 8. 终态一致:旅程涉及的全部 run 逐一核对。
    // ------------------------------------------------------------------
    let expected: [(String, &str, &str); 4] = [
        (fail_a_id.clone(), "failed", "DRIVER_INJECTED_FAILURE"),
        (fail_b_id.clone(), "failed", "DRIVER_INJECTED_FAILURE"),
        (after_a_id.clone(), "succeeded", "SUCCEEDED"),
        (after_b_id.clone(), "succeeded", "SUCCEEDED"),
    ];
    for (run_id, state, outcome) in expected {
        let run = run_of(root, &run_id).await;
        assert_eq!(run["state"], state, "run {run_id}: {run}");
        assert_eq!(run["terminal"], true, "run {run_id}: {run}");
        assert_eq!(run["outcomeCode"], outcome, "run {run_id}: {run}");
    }
}
