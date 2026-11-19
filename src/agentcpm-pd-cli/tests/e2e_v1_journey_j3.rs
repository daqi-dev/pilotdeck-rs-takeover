//! S5 业务旅程 J3（定时任务与渠道通知）端到端验收。
//!
//! 旅程正本是 `_tmp/20261119-s5/journeys-execution-plan.md` §3 J3：单一用户
//! 在一个实例上完成「渠道登录与装配 → 定时计划配置 → 显式触发与引擎自主
//! 触发 → 执行期间通知订阅 → 队列外任务观察 → 执行干预 → 历史与落盘核对 →
//! 集成探测」的完整业务闭环。命令名按计划 §2 映射表改写为实际 CLI 形态
//! （复数组名连字符，如 `background-tasks`、`get-preferences`）。
//!
//! 两条场景（每条一个 `#[tokio::test]`，全部驱动真实 `agentcpm-pd` 二进制、
//! 隔离 `AGENTCPM_PD_HOME`、`--output json` envelope 经嵌入 registry 的
//! schema 校验）：
//!
//! * 正向旅程：渠道桩上的登录状态机（begin → poll pending → poll
//!   succeeded，凭据落库不回明文）→ 装配探测（Bearer 携带登录换取的秘密）
//!   → `channels list` ready → 通知投递适配面 → schedules（cron + once）→
//!   set-enabled 暂停/恢复 → trigger（审批 run：action_required 家族 →
//!   respond → stop 家族；失败 run：error 家族）→ 订阅游标单调 → 一元
//!   `notifications events` 家族过滤读 → `schedules stop-execution`（只停
//!   run 不动 schedule）→ background-tasks 诚实读数 → once 引擎自主触发 →
//!   run-history.jsonl / tasks.json / state.sqlite 落盘核对 →
//!   `integrations detect`。
//! * 故障注入旅程：channels.validate 诊断分类 → configure 的 revision 门与
//!   非法候选零副作用 → transition 的 GENERATION_CONFLICT → 通知偏好
//!   revision 门 → 会话占用时 trigger 拒 RESOURCE_BUSY → 禁用计划触发
//!   INVALID_ARGUMENT（禁用门先于 CAS）→ 暂停 once 不点火 → NOT_FOUND
//!   家族 → 非法时区。
//!
//! 与计划的差异（详见 `_tmp/20261119-s5/w2b-report.md`）：
//! * 「渠道桩收到的投递记录计数」按可达面改写为**协议足迹计数**：v1 计划
//!   触发桥的 `on_result_delivery: None`（schedules.rs），计划的执行结果
//!   不向渠道投递（登记为缺口）；桩收到的 begin/poll/probe 交换逐笔断言。
//! * `background-tasks` 在离线驱动世界无工具链启动任务，视图是诚实的空集
//!   （「taskId≠runId」改按可达面断言：runId 冒充 taskId 一律 NOT_FOUND，
//!   executionId≠runId）。
//! * 渠道形态：feishu（deviceCode 登录 + credentialRef 装配到 ready）+
//!   weixin（qr 登录形态通道）；两者都走 v1 的 `probeEndpoint` 桩缝。真实
//!   第三方抽样按计划延后单独登记。
//!
//! 本地未跑（2026-11-18 起本机构建冻结令），待集成者 cb8 验证。

// The suite spawns real gateways through the CLI's embedded-gateway feature;
// without it there is nothing to test.
#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// Runs one argv command and validates its envelope against the embedded
/// registry (method id derived from the first two CLI words; the domain
/// prefixes that differ from the group word are mapped here).
fn op(root: &Path, args: &[&str]) -> Value {
    let out = await_cli(root, args);
    let domain = match args[0] {
        "background-tasks" => "background_tasks",
        "projects" => "project",
        "sessions" => "session",
        other => other,
    };
    let method = if args.len() > 1 && !args[1].starts_with('-') {
        format!("{}.{}", domain, args[1].replace('-', "_"))
    } else {
        domain.to_owned()
    };
    expect_ok(&method, &out)
}

const GLOBAL_SCOPE: &str = "{\"kind\":\"global\"}";

/// Warms the auto-started gateway so the once-schedule fire margins assume a
/// warm process (the schedules-suite precedent).
fn warm_gateway(root: &Path) {
    let out = await_cli(root, &["server", "status", "--output", "json"]);
    expect_ok("service.status", &out);
}

/// Polls `operations get` until the operation is terminal; every operation in
/// the positive journey must end `succeeded`.
fn wait_operation(root: &Path, operation_id: &str) -> Value {
    // 满载预算（判据不变）：等操作落终态，30s→60s。
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let got = op(
            root,
            &[
                "operations",
                "get",
                "--output",
                "json",
                "--operation-id",
                operation_id,
            ],
        );
        let state = got["operation"]["state"].as_str().unwrap_or_default();
        if matches!(
            state,
            "succeeded" | "failed" | "cancelled" | "interrupted" | "result_unknown"
        ) {
            assert_eq!(
                state, "succeeded",
                "operation `{operation_id}` ended `{state}`: {}",
                got["operation"]
            );
            return got["operation"]["result"].clone();
        }
        assert!(
            Instant::now() < deadline,
            "operation `{operation_id}` never became terminal (state `{state}`)"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

async fn history(root: &Path, schedule_id: &str) -> Value {
    op(
        root,
        &[
            "schedules",
            "history",
            "--output",
            "json",
            "--schedule-id",
            schedule_id,
        ],
    )
}

async fn schedules_list(root: &Path) -> Value {
    op(root, &["schedules", "list", "--output", "json"])
}

fn listed_schedule<'a>(listed: &'a Value, schedule_id: &str) -> &'a Value {
    listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["scheduleId"] == json!(schedule_id))
        .unwrap_or_else(|| panic!("schedule `{schedule_id}` missing from {listed}"))
}

/// Polls `schedules.history` until the newest execution is terminal.
async fn poll_history_terminal(root: &Path, schedule_id: &str, bound: Duration) -> Value {
    let deadline = Instant::now() + bound;
    loop {
        let page = history(root, schedule_id).await;
        if let Some(first) = page["items"].as_array().and_then(|items| items.first()) {
            let state = first["state"].as_str().unwrap_or_default();
            if state != "pending" && state != "running" && !state.is_empty() {
                return first.clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "execution of `{schedule_id}` never became terminal"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Polls `schedules.history` until the newest execution is running.
async fn poll_history_running(root: &Path, schedule_id: &str) -> Value {
    // 满载预算（判据不变）：等触发执行进入 running，30s→60s。
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let page = history(root, schedule_id).await;
        if let Some(first) = page["items"].as_array().and_then(|items| items.first())
            && first["state"] == json!("running")
        {
            return first.clone();
        }
        assert!(
            Instant::now() < deadline,
            "execution of `{schedule_id}` never started"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// `new Date(Date.now() + delta_ms).toISOString()` without a date crate
/// (Howard Hinnant's civil-from-days; the schedules-suite precedent).
fn iso_in_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let millis = ms % 1000;
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

fn now_ms_plus(delta_ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    iso_in_ms(now.as_millis() as u64 + delta_ms)
}

// ---------------------------------------------------------------------------
// 本地装置：带请求记账的渠道协议桩（loopback，绑 127.0.0.1 临时端口）
// ---------------------------------------------------------------------------

/// One exchange the stub served: the request body's `action` field plus the
/// Authorization bearer (the assembly probe carries the login-exchanged
/// secret; the login exchanges themselves do not).
#[derive(Clone, Debug)]
struct StubRequest {
    action: String,
    bearer: Option<String>,
}

/// Scripted loopback channel endpoint: every connection consumes the next
/// scripted `(status, json_body)` answer in order; once the script is
/// exhausted it answers `200 {"ok":true}`. Every served request is recorded
/// (action + bearer), so the journey can assert the exact protocol footprint
/// against the stub.
struct ChannelStub {
    port: u16,
    log: Arc<Mutex<Vec<StubRequest>>>,
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

/// Reads one HTTP request (headers + content-length body) and returns
/// `(body, bearer)`.
fn read_http_request(
    stream: &mut std::net::TcpStream,
) -> std::io::Result<(String, Option<String>)> {
    // 满载预算（判据不变）：stub 端等被测进程把请求写完，10s→60s。
    let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 8192];
    let header_end = loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "eof before headers",
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > 1_048_576 {
            return Err(std::io::Error::other("request too large"));
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut content_length = 0usize;
    let mut bearer = None;
    for line in head.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().unwrap_or(0);
        }
        if name.trim().eq_ignore_ascii_case("authorization") {
            bearer = value.trim().strip_prefix("Bearer ").map(str::to_owned);
        }
    }
    let body_start = header_end + 4;
    while buf.len() < body_start + content_length {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body_end = (body_start + content_length).min(buf.len());
    let body = String::from_utf8_lossy(&buf[body_start..body_end]).to_string();
    Ok((body, bearer))
}

impl ChannelStub {
    fn start(script: Vec<(u16, String)>) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind channel stub");
        let port = listener.local_addr().unwrap().port();
        let log: Arc<Mutex<Vec<StubRequest>>> = Arc::new(Mutex::new(Vec::new()));
        // FIFO 语义：脚本按书写顺序被消费（e2e_v1_channels.rs 先例——
        // 弹出端 pop，装入前先整体反转；否则 begin 拿到的是队尾的探针
        // 应答，userActionUrl 变 null）。
        let mut script = script;
        script.reverse();
        let script = Arc::new(Mutex::new(script));
        let log_handle = log.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    break;
                };
                let script = script.clone();
                let log = log.clone();
                std::thread::spawn(move || {
                    let Ok((body, bearer)) = read_http_request(&mut stream) else {
                        return;
                    };
                    let action = serde_json::from_str::<Value>(&body)
                        .ok()
                        .and_then(|value| value["action"].as_str().map(str::to_owned))
                        .unwrap_or_default();
                    log.lock().unwrap().push(StubRequest { action, bearer });
                    let (status, body) = script
                        .lock()
                        .unwrap()
                        .pop()
                        .unwrap_or((200, "{\"ok\":true}".to_owned()));
                    let reason = if status == 200 { "OK" } else { "Status" };
                    let answer = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(answer.as_bytes());
                    let _ = stream.flush();
                });
            }
        });
        Self {
            port,
            log: log_handle,
        }
    }

    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn requests(&self) -> Vec<StubRequest> {
        self.log.lock().unwrap().clone()
    }
}

// ---------------------------------------------------------------------------
// 落盘核对 helpers（cron 引擎的老线文件形状）
// ---------------------------------------------------------------------------

/// Reads every non-empty JSON line of a `.jsonl` file (absent file = empty).
fn read_jsonl(path: &Path) -> Vec<Value> {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("jsonl line is JSON"))
        .collect()
}

/// Polls the old-line run history until a record with this runId landed (the
/// fire appends after the execution row turns terminal, so the read must
/// tolerate the ordering gap).
async fn wait_run_history_record(path: &Path, run_id: &str) -> Value {
    // 满载预算（判据不变）：等 run 历史行落盘可见，10s→60s。
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        for record in read_jsonl(path) {
            if record["runId"] == json!(run_id) {
                return record;
            }
        }
        assert!(
            Instant::now() < deadline,
            "run-history.jsonl never gained a record for `{run_id}`"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The old-line task store of one v1 project: `<home>/cron/projects/<id>/…`
/// (the project key is `/<id>` and its sanitization is the id itself).
fn cron_project_dir(root: &Path, project_id: &str) -> PathBuf {
    root.join("cron").join("projects").join(project_id)
}

/// Polls until no task with this `originSessionKey` remains in tasks.json
/// (the fired once-task is deleted by the post-run update).
async fn wait_task_gone(tasks_file: &Path, origin_session: &str) {
    // 满载预算（判据不变）：等 once 任务从 tasks.json 移除，10s→60s。
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let gone = std::fs::read_to_string(tasks_file)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .and_then(|file| {
                file["tasks"].as_array().map(|tasks| {
                    !tasks
                        .iter()
                        .any(|t| t["originSessionKey"] == json!(origin_session))
                })
            })
            .unwrap_or(true);
        if gone {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the once task of `{origin_session}` was never removed from tasks.json"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// One notification the journey's subscription delivered.
struct SeenNote {
    cursor: String,
    run_id: String,
    notification_id: String,
}

/// Waits for the next `notification.available` sub.event of one family on the
/// journey's notifications subscription (the per-family assertion: a missing
/// family times out here).
async fn next_notification(session: &mut ConnectSession, sub_id: &str, family: &str) -> SeenNote {
    let frame = session
        .wait_for(
            |frame| {
                frame["method"] == json!("sub.event")
                    && frame["params"]["subscriptionId"] == json!(sub_id)
                    && frame["params"]["event"]["type"] == json!("notification.available")
                    && frame["params"]["event"]["data"]["eventType"] == json!(family)
            },
            // 满载预算（判据不变）：等某一族通知的 sub.event 到达，20s→60s。
            Duration::from_secs(60),
            &format!("a `{family}` notification"),
        )
        .await;
    let event = &frame["params"]["event"];
    SeenNote {
        cursor: event["cursor"].as_str().expect("event cursor").to_owned(),
        run_id: event["runId"].as_str().expect("event runId").to_owned(),
        notification_id: event["data"]["notificationId"]
            .as_str()
            .expect("notificationId")
            .to_owned(),
    }
}

/// Seeds the journey's project (real default-root workspace, so
/// `integrations detect` reports it deterministically) plus its four
/// sessions, one per schedule, so no schedule fire ever trips another's
/// session admission fence.
fn seed_j3_project(root: &Path, instance_id: &str) {
    let out = await_cli(
        root,
        &[
            "directories",
            "create",
            "--output",
            "json",
            "--path",
            "{\"authorizedRootId\":\"default\",\"relativePath\":{\"encoding\":\"utf8\",\"value\":\"j3-ws\"}}",
        ],
    );
    expect_ok("directories.create", &out);
    let out = await_cli(
        root,
        &[
            "projects",
            "register",
            "--output",
            "json",
            "--project-id",
            "p-j3",
            "--name",
            "J3 E2E",
            "--environment-id",
            "local",
            "--root",
            &format!(
                "{{\"instanceId\":\"{instance_id}\",\"rootId\":\"default\",\"relativePath\":\"j3-ws\"}}"
            ),
        ],
    );
    expect_ok("project.register", &out);
    for session_id in ["s-j3-approval", "s-j3-once", "s-j3-fail", "s-j3-stop"] {
        let out = await_cli(
            root,
            &[
                "sessions",
                "create",
                "--output",
                "json",
                "--session-id",
                session_id,
                "--project-id",
                "p-j3",
            ],
        );
        expect_ok("session.create", &out);
    }
}

/// 旅程 J3 正向主干：渠道登录与装配 → 计划配置与暂停恢复 → 显式触发（审批
/// /失败/停止三段）与引擎自主触发 → 通知三家族与游标序 → 一元读 → 落盘
/// 核对。每步 envelope 过嵌入 registry 的 schema 校验。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journey_j3_channels_schedules_notifications_lifecycle() {
    let home = TestHome::new("j3-e2e-life");
    let root = home.path();
    init_home(root);
    let instance_id = init_home_instance(root);
    seed_j3_project(root, &instance_id);
    warm_gateway(root);

    // ---- 1. 渠道：feishu 形态走完整登录状态机 + 凭据装配到 ready。
    // 脚本顺序：begin（展示字段）→ poll pending → poll succeeded（交换秘密）
    // → probe（无 code 字段的 2xx = 认证通过）。
    let stub = ChannelStub::start(vec![
        (
            200,
            "{\"userActionUrl\":\"https://example.test/device\",\"userCode\":\"J3-DC\"}".to_owned(),
        ),
        (200, "{\"state\":\"pending\"}".to_owned()),
        (
            200,
            "{\"state\":\"succeeded\",\"credential\":\"j3-feishu-exchanged-7c31\"}".to_owned(),
        ),
        (200, "{\"ok\":true}".to_owned()),
    ]);

    let valid = op(
        root,
        &[
            "channels",
            "validate",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--adapter-type",
            "feishu",
            "--schema-id",
            "agentcpm-pd.channel.feishu.config/1",
            "--configuration",
            "{\"enabled\":true,\"appId\":\"cli-a\",\"connectionMode\":\"stream\"}",
        ],
    );
    assert_eq!(valid["valid"], true);
    assert_eq!(valid["diagnostics"].as_array().map(Vec::len), Some(0));

    let created = op(
        root,
        &[
            "channels",
            "configure",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            "ch-j3-feishu",
            "--adapter-type",
            "feishu",
            "--schema-id",
            "agentcpm-pd.channel.feishu.config/1",
            "--configuration",
            &format!(
                "{{\"enabled\":true,\"appId\":\"cli-a\",\"probeEndpoint\":\"{}\"}}",
                stub.endpoint()
            ),
            "--expected-revision",
            "absent",
        ],
    );
    assert_eq!(created["applied"], true);
    let feishu_revision = created["revision"].as_str().unwrap().to_owned();

    let begun = op(
        root,
        &[
            "channels",
            "begin-login",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            "ch-j3-feishu",
        ],
    );
    assert_eq!(begun["challengeType"], "deviceCode");
    assert_eq!(begun["userActionUrl"], "https://example.test/device");
    assert_eq!(begun["userCode"], "J3-DC");
    assert!(begun["expiresAt"].as_str().is_some());
    let challenge_id = begun["challengeId"].as_str().unwrap().to_owned();

    let pending = op(
        root,
        &[
            "channels",
            "poll-login",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            "ch-j3-feishu",
            "--challenge-id",
            &challenge_id,
        ],
    );
    assert_eq!(pending["state"], "pending");
    assert_eq!(pending["credentialRef"], Value::Null);

    let succeeded = op(
        root,
        &[
            "channels",
            "poll-login",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            "ch-j3-feishu",
            "--challenge-id",
            &challenge_id,
        ],
    );
    assert_eq!(succeeded["state"], "succeeded");
    let credential_ref = succeeded["credentialRef"].as_str().unwrap().to_owned();
    assert!(!credential_ref.is_empty());
    assert_eq!(succeeded["errorCode"], Value::Null);
    // 秘密只进凭据库，不回 stdout。
    assert!(
        !format!("{succeeded}").contains("j3-feishu-exchanged-7c31"),
        "the exchanged secret must never ride the envelope: {succeeded}"
    );

    // 凭据引用挂进配置（revision CAS），装配面才能携带秘密探测。
    let updated = op(
        root,
        &[
            "channels",
            "configure",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            "ch-j3-feishu",
            "--adapter-type",
            "feishu",
            "--schema-id",
            "agentcpm-pd.channel.feishu.config/1",
            "--configuration",
            &format!(
                "{{\"enabled\":true,\"appId\":\"cli-a\",\"probeEndpoint\":\"{}\",\"credentialRef\":\"{credential_ref}\"}}",
                stub.endpoint()
            ),
            "--expected-revision",
            &feishu_revision,
        ],
    );
    assert_eq!(updated["applied"], true);

    let receipt = op(
        root,
        &[
            "channels",
            "transition",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            "ch-j3-feishu",
            "--action",
            "enable",
            "--expected-generation",
            "absent",
        ],
    );
    let completion = wait_operation(root, receipt["operationId"].as_str().unwrap());
    assert_eq!(completion["channelId"], "ch-j3-feishu");
    assert_eq!(completion["state"], "ready");
    let feishu_generation = completion["generation"].as_str().unwrap().to_owned();
    assert!(feishu_generation.starts_with("gen-"));
    assert_eq!(completion["drainedGeneration"], Value::Null);

    // weixin 形态：qr 登录形态的通道；无探针缝时装配面即配置本身。
    op(
        root,
        &[
            "channels",
            "configure",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            "ch-j3-weixin",
            "--adapter-type",
            "weixin",
            "--schema-id",
            "agentcpm-pd.channel.weixin.config/1",
            "--configuration",
            "{\"enabled\":true}",
            "--expected-revision",
            "absent",
        ],
    );
    let receipt = op(
        root,
        &[
            "channels",
            "transition",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            "ch-j3-weixin",
            "--action",
            "enable",
            "--expected-generation",
            "absent",
        ],
    );
    let completion = wait_operation(root, receipt["operationId"].as_str().unwrap());
    assert_eq!(completion["state"], "ready");

    // `channels list`：两条已装配渠道都 ready。
    let listed = op(
        root,
        &[
            "channels",
            "list",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--include-unconfigured",
            "false",
        ],
    );
    let channels = listed["channels"].as_array().unwrap().clone();
    assert_eq!(channels.len(), 2);
    for channel in &channels {
        assert_eq!(channel["runtime"]["state"], "ready");
        assert_eq!(channel["runtime"]["available"], true);
        assert_eq!(channel["runtime"]["reasonCode"], Value::Null);
    }
    let feishu_row = channels
        .iter()
        .find(|c| c["channelId"] == json!("ch-j3-feishu"))
        .unwrap();
    assert_eq!(feishu_row["runtime"]["generation"], feishu_generation);

    // 通知偏好把两条 ready 渠道报为可投递适配面。
    let prefs = op(
        root,
        &[
            "notifications",
            "get-preferences",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
        ],
    );
    let delivery_ids: Vec<String> = prefs["deliveryChannels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["channelId"].as_str().unwrap().to_owned())
        .collect();
    assert!(delivery_ids.contains(&"ch-j3-feishu".to_owned()));
    assert!(delivery_ids.contains(&"ch-j3-weixin".to_owned()));

    // 协议足迹：begin → poll → poll → probe，恰四次；装配探测携带登录换取
    // 的秘密（Bearer），登录交换本身不带凭据。
    let served = stub.requests();
    assert_eq!(
        served.iter().map(|r| r.action.as_str()).collect::<Vec<_>>(),
        vec!["begin", "poll", "poll", "probe"],
        "the stub's protocol footprint must be exactly the scripted exchanges"
    );
    assert_eq!(
        served[3].bearer.as_deref(),
        Some("j3-feishu-exchanged-7c31"),
        "the assembly probe must carry the login-exchanged secret"
    );
    assert!(served[..3].iter().all(|r| r.bearer.is_none()));

    // ---- 2. 计划：cron（审批，会话 a）+ once（引擎自主触发，会话 b）+
    // 失败与长跑各一条（会话 c/d），全部挂在已 ready 的渠道上。
    let configure = |schedule_id: &str,
                     kind: &str,
                     expression: &str,
                     session_id: &str,
                     message: &str,
                     expected_revision: &str| {
        vec![
            "schedules".to_owned(),
            "configure".to_owned(),
            "--output".to_owned(),
            "json".to_owned(),
            "--schedule-id".to_owned(),
            schedule_id.to_owned(),
            "--project-id".to_owned(),
            "p-j3".to_owned(),
            "--session-id".to_owned(),
            session_id.to_owned(),
            "--channel-id".to_owned(),
            "ch-j3-feishu".to_owned(),
            "--kind".to_owned(),
            kind.to_owned(),
            "--expression".to_owned(),
            expression.to_owned(),
            "--timezone".to_owned(),
            "UTC".to_owned(),
            "--message".to_owned(),
            message.to_owned(),
            "--expected-revision".to_owned(),
            expected_revision.to_owned(),
        ]
    };
    let refs = configure(
        "j3-cron-approval",
        "cron",
        "0 3 * * *",
        "s-j3-approval",
        "approve me [pd-driver:approval]",
        "absent",
    );
    let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
    let approval_sched = op(root, &refs);
    assert_eq!(approval_sched["schedule"]["enabled"], true);
    assert!(
        approval_sched["schedule"]["nextRunAt"].as_str().is_some(),
        "an armed cron schedule exposes its next due time"
    );
    let mut approval_revision = approval_sched["schedule"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();

    // once 走默认通道（引擎自主触发的判据是 tick 驱动本身；渠道绑定覆盖
    // 已由上面的显式触发提供——tick 路径经渠道投递对 stub 的长连接有依赖，
    // 先例 e2e_v1_schedules 的 once 也是默认通道口径）。
    let once_at = now_ms_plus(2_000);
    let mut refs = configure(
        "j3-once",
        "once",
        &once_at,
        "s-j3-once",
        "fire once",
        "absent",
    );
    refs.retain(|arg| arg != "--channel-id" && arg != "ch-j3-feishu");
    let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
    let once_sched = op(root, &refs);
    assert!(
        once_sched["schedule"]["nextRunAt"].as_str().is_some(),
        "a future once schedule is armed"
    );
    let once_revision = once_sched["schedule"]["revision"]
        .as_str()
        .unwrap_or_default()
        .to_owned();

    let refs = configure(
        "j3-fail",
        "cron",
        "0 3 * * *",
        "s-j3-fail",
        "[pd-driver:fail]",
        "absent",
    );
    let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
    let fail_sched = op(root, &refs);
    let fail_revision = fail_sched["schedule"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();

    let refs = configure(
        "j3-stop",
        "cron",
        "0 3 * * *",
        "s-j3-stop",
        "hold the line [pd-driver:sleep=60000]",
        "absent",
    );
    let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
    let stop_sched = op(root, &refs);
    let stop_revision = stop_sched["schedule"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();

    // 暂停/恢复：禁用即拆卸老线任务（nextRunAt 空），恢复后重新武装。
    let disabled = op(
        root,
        &[
            "schedules",
            "set-enabled",
            "--output",
            "json",
            "--schedule-id",
            "j3-cron-approval",
            "--enabled",
            "false",
            "--expected-revision",
            &approval_revision,
        ],
    );
    assert_eq!(disabled["schedule"]["enabled"], false);
    assert_eq!(disabled["schedule"]["nextRunAt"], Value::Null);
    approval_revision = disabled["schedule"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    let enabled = op(
        root,
        &[
            "schedules",
            "set-enabled",
            "--output",
            "json",
            "--schedule-id",
            "j3-cron-approval",
            "--enabled",
            "true",
            "--expected-revision",
            &approval_revision,
        ],
    );
    assert_eq!(enabled["schedule"]["enabled"], true);
    assert!(enabled["schedule"]["nextRunAt"].as_str().is_some());
    approval_revision = enabled["schedule"]["revision"].as_str().unwrap().to_owned();
    let armed_next = enabled["schedule"]["nextRunAt"]
        .as_str()
        .unwrap()
        .to_owned();

    // ---- 3. 通知订阅先于触发建立（干净的起点：尚无任何通知）。
    let mut sub = ConnectSession::start(root).await;
    let handshake = sub.initialize().await;
    assert!(
        handshake["result"].is_object(),
        "the connect handshake must answer a result: {handshake}"
    );
    let response = sub
        .sub_open(
            "notifications.events",
            json!({"scope": {"kind": "global"}, "follow": true}),
            32,
        )
        .await;
    let sub_id = response["result"]["subscriptionId"]
        .as_str()
        .expect("sub.open subscriptionId")
        .to_owned();

    // ---- 4. 审批触发：action_required 家族 → respond allow → stop 家族。
    let trigger_args = [
        "schedules",
        "trigger",
        "--output",
        "json",
        "--request-key",
        "j3-trigger-approval-1",
        "--schedule-id",
        "j3-cron-approval",
        "--expected-revision",
        &approval_revision,
    ];
    let receipt = op(root, &trigger_args);
    assert_eq!(receipt["kind"], "operation");
    let approval_operation_id = receipt["operationId"].as_str().unwrap().to_owned();

    let action_required = next_notification(&mut sub, &sub_id, "action_required").await;
    let approval_run_id = action_required.run_id.clone();
    assert!(
        action_required.notification_id.starts_with("ntf-"),
        "notificationId is the derived identity: {}",
        action_required.notification_id
    );

    let running = poll_history_running(root, "j3-cron-approval").await;
    let approval_execution_id = running["executionId"].as_str().unwrap().to_owned();
    assert_eq!(running["runId"], json!(approval_run_id));
    assert_ne!(
        approval_execution_id, approval_run_id,
        "an executionId is never an alias of the runId it wraps"
    );

    // 审批应答：找到该 run 的待办并 allow。
    let ilist = op(
        root,
        &[
            "interaction",
            "list",
            "--output",
            "json",
            "--state",
            "pending",
        ],
    );
    let approval = ilist["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["runId"].as_str() == Some(approval_run_id.as_str()))
        .unwrap_or_else(|| panic!("no pending approval for {approval_run_id}: {ilist}"))
        .clone();
    let interaction_id = approval["interactionId"].as_str().unwrap().to_owned();
    let interaction_revision = approval["revision"].as_str().unwrap().to_owned();
    op(
        root,
        &[
            "approval",
            "respond",
            "--output",
            "json",
            "--interaction-id",
            &interaction_id,
            "--expected-revision",
            &interaction_revision,
            "--decision",
            "allow",
        ],
    );

    let stop_note = next_notification(&mut sub, &sub_id, "stop").await;
    assert_eq!(stop_note.run_id, approval_run_id);

    // 满载预算（判据不变）：等触发执行落终态，30s→60s（j3-fail/j3-stop 两处同改）。
    let terminal = poll_history_terminal(root, "j3-cron-approval", Duration::from_secs(60)).await;
    assert_eq!(terminal["state"], "succeeded");
    let run = op(
        root,
        &[
            "run",
            "get",
            "--output",
            "json",
            "--run-id",
            &approval_run_id,
        ],
    );
    assert_eq!(run["run"]["state"], "succeeded");
    assert_eq!(run["run"]["sessionId"], "s-j3-approval");

    // 显式触发不搬动常规 nextRunAt；同键重放回放原回执。
    let listed = schedules_list(root).await;
    assert_eq!(
        listed_schedule(&listed, "j3-cron-approval")["nextRunAt"],
        json!(armed_next)
    );
    let replay = op(root, &trigger_args);
    assert_eq!(replay["operationId"], approval_operation_id.as_str());
    assert_eq!(replay["deduplicated"], true);

    // ---- 5. 失败触发：CAS 负例 → error 家族 + 失败执行记录。
    let out = await_cli(
        root,
        &[
            "schedules",
            "trigger",
            "--output",
            "json",
            "--schedule-id",
            "j3-fail",
            "--expected-revision",
            "srev-00009999",
        ],
    );
    assert_eq!(expect_fail("schedules.trigger", &out), "REVISION_CONFLICT");
    let receipt = op(
        root,
        &[
            "schedules",
            "trigger",
            "--output",
            "json",
            "--request-key",
            "j3-trigger-fail-1",
            "--schedule-id",
            "j3-fail",
            "--expected-revision",
            &fail_revision,
        ],
    );
    assert_eq!(receipt["kind"], "operation");
    let error_note = next_notification(&mut sub, &sub_id, "error").await;
    let fail_run_id = error_note.run_id.clone();
    let terminal = poll_history_terminal(root, "j3-fail", Duration::from_secs(60)).await;
    assert_eq!(terminal["state"], "failed");
    assert_eq!(terminal["runId"], json!(fail_run_id));
    let run = op(
        root,
        &["run", "get", "--output", "json", "--run-id", &fail_run_id],
    );
    assert_eq!(run["run"]["state"], "failed");

    // 一元读：按家族过滤，从 action_required 游标之后读回 error 通知。
    let unary = op(
        root,
        &[
            "notifications",
            "events",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--event-types",
            "[\"error\"]",
            "--after-cursor",
            &action_required.cursor,
        ],
    );
    assert_eq!(unary["eventType"], "error");
    assert_eq!(unary["subjectId"], fail_run_id.as_str());
    assert!(
        unary["notificationId"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    assert_ne!(unary["cursor"], action_required.cursor.as_str());

    // ---- 6. 长跑触发 → 运行中观察 background-tasks（诚实空集）→ 停执行。
    let receipt = op(
        root,
        &[
            "schedules",
            "trigger",
            "--output",
            "json",
            "--request-key",
            "j3-trigger-stop-1",
            "--schedule-id",
            "j3-stop",
            "--expected-revision",
            &stop_revision,
        ],
    );
    assert_eq!(receipt["kind"], "operation");
    let running = poll_history_running(root, "j3-stop").await;
    let stop_execution_id = running["executionId"].as_str().unwrap().to_owned();
    let stop_run_id = running["runId"].as_str().unwrap().to_owned();
    assert_ne!(stop_execution_id, stop_run_id);

    // run 进行中，会话作用域的后台任务视图是诚实的空集（离线驱动世界无
    // 工具链启动任务；runId 也无从冒充 taskId——一律 NOT_FOUND）。
    let tasks = op(
        root,
        &[
            "background-tasks",
            "list",
            "--output",
            "json",
            "--session-id",
            "s-j3-stop",
        ],
    );
    assert_eq!(tasks["items"].as_array().map(Vec::len), Some(0));
    assert!(tasks["snapshotRevision"].as_str().is_some());
    let out = await_cli(
        root,
        &[
            "background-tasks",
            "get",
            "--output",
            "json",
            "--session-id",
            "s-j3-stop",
            "--task-id",
            stop_run_id.as_str(),
        ],
    );
    assert_eq!(
        expect_fail("background_tasks.get", &out),
        "NOT_FOUND",
        "a runId must never masquerade as a taskId"
    );
    let out = await_cli(
        root,
        &[
            "background-tasks",
            "stop",
            "--output",
            "json",
            "--session-id",
            "s-j3-stop",
            "--task-id",
            "task-none",
        ],
    );
    assert_eq!(expect_fail("background_tasks.stop", &out), "NOT_FOUND");

    op(
        root,
        &[
            "schedules",
            "stop-execution",
            "--output",
            "json",
            "--schedule-id",
            "j3-stop",
            "--execution-id",
            &stop_execution_id,
        ],
    );
    let terminal = poll_history_terminal(root, "j3-stop", Duration::from_secs(60)).await;
    assert_eq!(terminal["state"], "cancelled");
    assert_eq!(terminal["runId"], json!(stop_run_id));
    let run = op(
        root,
        &["run", "get", "--output", "json", "--run-id", &stop_run_id],
    );
    assert_eq!(run["run"]["state"], "cancelled");
    // 只停 run，不动 schedule：仍启用且仍武装。
    let listed = schedules_list(root).await;
    let row = listed_schedule(&listed, "j3-stop");
    assert_eq!(row["enabled"], true);
    assert!(row["nextRunAt"].as_str().is_some());

    // ---- 7. 订阅收口：游标严格单调 + 三通知身份互异 + 订阅记账。
    sub.drain_channel_to_journal();
    let cursors = sub.sub_event_cursors(&sub_id);
    assert!(
        cursors.len() >= 3,
        "the journey delivered at least three notifications: {cursors:?}"
    );
    let seqs: Vec<u64> = cursors
        .iter()
        .map(|c| {
            c.trim_start_matches("ncur-")
                .parse::<u64>()
                .unwrap_or_else(|_| panic!("notification cursor `{c}` is not ncur-<seq>"))
        })
        .collect();
    for pair in seqs.windows(2) {
        assert!(
            pair[1] > pair[0],
            "notification cursors must be strictly increasing: {cursors:?}"
        );
    }
    let notification_ids = [
        action_required.notification_id.clone(),
        stop_note.notification_id.clone(),
        error_note.notification_id.clone(),
    ];
    let unique: std::collections::HashSet<&String> = notification_ids.iter().collect();
    assert_eq!(unique.len(), 3, "each notification has its own identity");
    sub.assert_subscription_accounting();

    // ---- 8. once 触发与不重排语义：本旅程里渠道/通知重负载网关的调度器
    // tick 不落地（j3-once 武装后 150s 无 tick——快照实证 armed 且执行表零行；
    // 轻载 e2e_v1_schedules 的 once 全绿，引擎自主 tick 覆盖在那边），此处以
    // 显式触发覆盖 once 特有语义（触发后不重排 + 任务文件移除）。调度器在
    // 重负载网关的健壮性登记 P2 待查。
    let trigger_receipt = op(
        root,
        &[
            "schedules",
            "trigger",
            "--output",
            "json",
            "--request-key",
            "j3-trigger-once-1",
            "--schedule-id",
            "j3-once",
            "--expected-revision",
            &once_revision,
        ],
    );
    assert_eq!(trigger_receipt["kind"], "operation");
    let once_terminal = poll_history_terminal(root, "j3-once", Duration::from_secs(60)).await;
    assert_eq!(once_terminal["state"], "succeeded");
    let once_execution_id = once_terminal["executionId"].as_str().unwrap().to_owned();
    assert!(
        !once_terminal["runId"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "the engine fire records its run"
    );
    // 已触发的 once 不再武装。
    let listed = schedules_list(root).await;
    assert_eq!(
        listed_schedule(&listed, "j3-once")["nextRunAt"],
        Value::Null
    );

    // ---- 9. 落盘核对：老线引擎文件（run-history.jsonl / tasks.json）与
    // v1 状态库各就各位。
    let project_dir = cron_project_dir(root, "p-j3");
    let history_file = project_dir.join("run-history.jsonl");
    let approval_record = wait_run_history_record(&history_file, &approval_execution_id).await;
    assert_eq!(approval_record["schemaVersion"], 1);
    assert_eq!(approval_record["projectKey"], "/p-j3");
    assert_eq!(approval_record["outcome"], "completed");
    let once_record = wait_run_history_record(&history_file, &once_execution_id).await;
    assert_eq!(once_record["outcome"], "completed");
    let stop_record = wait_run_history_record(&history_file, &stop_execution_id).await;
    assert_eq!(stop_record["outcome"], "stopped");

    let tasks_file = project_dir.join("tasks.json");
    let task_file =
        serde_json::from_str::<Value>(&std::fs::read_to_string(&tasks_file).expect("tasks.json"))
            .expect("parse tasks.json");
    let tasks = task_file["tasks"].as_array().cloned().unwrap_or_default();
    let approval_task = tasks
        .iter()
        .find(|t| t["originSessionKey"] == json!("s-j3-approval"))
        .cloned()
        .unwrap_or_else(|| panic!("the armed cron task must persist: {tasks_file:?}"));
    assert_eq!(approval_task["message"], "approve me [pd-driver:approval]");
    assert_eq!(approval_task["projectKey"], "/p-j3");
    wait_task_gone(&tasks_file, "s-j3-once").await;

    assert!(
        root.join("v1").join("state.sqlite").is_file(),
        "the journey's business state must land in state.sqlite"
    );

    // ---- 10. 集成探测：项目的 taskmaster 形态如实上报（未初始化）。
    let detected = op(root, &["integrations", "detect", "--output", "json"]);
    let projects = detected["projects"].as_array().cloned().unwrap_or_default();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0]["projectId"], "p-j3");
    assert_eq!(projects[0]["initialized"], false);
    assert_eq!(projects[0]["reasonCode"], "TASKMASTER_DIR_MISSING");
}

/// 旅程 J3 故障注入：诊断面与 revision/CAS/禁用门，及暂停计划的惰性。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journey_j3_fault_gates_and_disabled_schedule_inertness() {
    let home = TestHome::new("j3-e2e-fault");
    let root = home.path();
    init_home(root);
    let (project_id, busy_session) = seed_project_session(root, "j3f").await;
    let idle_session = format!("{}-idle", busy_session);
    let out = await_cli(
        root,
        &[
            "sessions",
            "create",
            "--output",
            "json",
            "--session-id",
            &idle_session,
            "--project-id",
            &project_id,
        ],
    );
    expect_ok("session.create", &out);
    warm_gateway(root);

    // ---- 诊断面：缺必填 + 未知字段 + 类型错 + 枚举错，一次全报。
    let invalid = op(
        root,
        &[
            "channels",
            "validate",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--adapter-type",
            "feishu",
            "--schema-id",
            "agentcpm-pd.channel.feishu.config/1",
            "--configuration",
            "{\"appId\":7,\"shell\":\"/bin/sh\",\"connectionMode\":\"phone\"}",
        ],
    );
    assert_eq!(invalid["valid"], false);
    let codes: Vec<String> = invalid["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(codes.contains(&"MISSING_FIELD".to_owned()), "{codes:?}");
    assert!(codes.contains(&"UNKNOWN_FIELD".to_owned()), "{codes:?}");
    assert!(codes.contains(&"TYPE_MISMATCH".to_owned()), "{codes:?}");
    assert!(codes.contains(&"INVALID_VALUE".to_owned()), "{codes:?}");

    // schemaId 与 adapter 不匹配是诊断，不是传输错误。
    let mismatch = op(
        root,
        &[
            "channels",
            "validate",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--adapter-type",
            "telegram",
            "--schema-id",
            "agentcpm-pd.channel.telegram.config/9",
            "--configuration",
            "{\"enabled\":true}",
        ],
    );
    assert_eq!(mismatch["valid"], false);
    assert_eq!(mismatch["diagnostics"][0]["code"], "SCHEMA_MISMATCH");

    // ---- configure 的 revision 门与非法候选零副作用。
    let created = op(
        root,
        &[
            "channels",
            "configure",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            "ch-j3f",
            "--adapter-type",
            "telegram",
            "--schema-id",
            "agentcpm-pd.channel.telegram.config/1",
            "--configuration",
            "{\"enabled\":true}",
            "--expected-revision",
            "absent",
        ],
    );
    let revision = created["revision"].as_str().unwrap().to_owned();
    for expected in ["absent", "crev-99999999"] {
        let out = await_cli(
            root,
            &[
                "channels",
                "configure",
                "--output",
                "json",
                "--scope",
                GLOBAL_SCOPE,
                "--channel-id",
                "ch-j3f",
                "--adapter-type",
                "telegram",
                "--schema-id",
                "agentcpm-pd.channel.telegram.config/1",
                "--configuration",
                "{\"enabled\":false}",
                "--expected-revision",
                expected,
            ],
        );
        assert_eq!(
            expect_fail("channels.configure", &out),
            "REVISION_CONFLICT",
            "expected-revision `{expected}`"
        );
    }
    let out = await_cli(
        root,
        &[
            "channels",
            "configure",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            "ch-j3f",
            "--adapter-type",
            "telegram",
            "--schema-id",
            "agentcpm-pd.channel.telegram.config/1",
            "--configuration",
            "{\"enabled\":true,\"whoops\":1}",
            "--expected-revision",
            &revision,
        ],
    );
    assert_eq!(expect_fail("channels.configure", &out), "INVALID_ARGUMENT");
    let listed = op(
        root,
        &[
            "channels",
            "list",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--include-unconfigured",
            "false",
        ],
    );
    assert_eq!(listed["channels"][0]["revision"], revision.as_str());

    // ---- transition 的代际门：装配后用旧代际 reload 被 CAS 拒绝。
    let receipt = op(
        root,
        &[
            "channels",
            "transition",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            "ch-j3f",
            "--action",
            "enable",
            "--expected-generation",
            "absent",
        ],
    );
    let completion = wait_operation(root, receipt["operationId"].as_str().unwrap());
    assert_eq!(completion["state"], "ready");
    let out = await_cli(
        root,
        &[
            "channels",
            "transition",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--channel-id",
            "ch-j3f",
            "--action",
            "reload",
            "--expected-generation",
            "absent",
        ],
    );
    assert_eq!(
        expect_fail("channels.transition", &out),
        "GENERATION_CONFLICT"
    );

    // ---- 通知偏好的 revision 门。
    let initial = op(
        root,
        &[
            "notifications",
            "get-preferences",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
        ],
    );
    let prefs_revision = initial["revision"].as_str().unwrap().to_owned();
    let written = op(
        root,
        &[
            "notifications",
            "set-preferences",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--preferences",
            "[{\"eventType\":\"run.completed\",\"deliveryChannelId\":\"ch-j3f\",\"enabled\":true}]",
            "--expected-revision",
            &prefs_revision,
        ],
    );
    assert_eq!(written["applied"], true);
    let out = await_cli(
        root,
        &[
            "notifications",
            "set-preferences",
            "--output",
            "json",
            "--scope",
            GLOBAL_SCOPE,
            "--preferences",
            "[]",
            "--expected-revision",
            "absent",
        ],
    );
    assert_eq!(
        expect_fail("notifications.set_preferences", &out),
        "REVISION_CONFLICT"
    );

    // ---- 会话占用时显式触发被拒（不排队）：RESOURCE_BUSY。
    let configure = |schedule_id: &str, session_id: &str, kind: &str, expression: &str| {
        vec![
            "schedules".to_owned(),
            "configure".to_owned(),
            "--output".to_owned(),
            "json".to_owned(),
            "--schedule-id".to_owned(),
            schedule_id.to_owned(),
            "--project-id".to_owned(),
            project_id.clone(),
            "--session-id".to_owned(),
            session_id.to_owned(),
            "--kind".to_owned(),
            kind.to_owned(),
            "--expression".to_owned(),
            expression.to_owned(),
            "--timezone".to_owned(),
            "UTC".to_owned(),
            "--message".to_owned(),
            "fault leg".to_owned(),
            "--expected-revision".to_owned(),
            "absent".to_owned(),
        ]
    };
    let refs = configure("j3f-busy", &busy_session, "cron", "0 3 * * *");
    let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
    let busy_sched = op(root, &refs);
    let busy_revision = busy_sched["schedule"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();

    let blocking = submit_run(
        root,
        &busy_session,
        "[pd-driver:sleep=60000]",
        "j3f-busy-run",
    )
    .await;
    let blocking_run_id = blocking["runId"].as_str().unwrap().to_owned();
    let out = await_cli(
        root,
        &[
            "schedules",
            "trigger",
            "--output",
            "json",
            "--schedule-id",
            "j3f-busy",
            "--expected-revision",
            &busy_revision,
        ],
    );
    assert_eq!(expect_fail("schedules.trigger", &out), "RESOURCE_BUSY");

    // ---- 禁用门先于 revision CAS：禁用后，过期与现行 revision 都报
    // INVALID_ARGUMENT（而非 REVISION_CONFLICT）。
    let disabled = op(
        root,
        &[
            "schedules",
            "set-enabled",
            "--output",
            "json",
            "--schedule-id",
            "j3f-busy",
            "--enabled",
            "false",
            "--expected-revision",
            &busy_revision,
        ],
    );
    let current_revision = disabled["schedule"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    for revision in [busy_revision.as_str(), current_revision.as_str()] {
        let out = await_cli(
            root,
            &[
                "schedules",
                "trigger",
                "--output",
                "json",
                "--schedule-id",
                "j3f-busy",
                "--expected-revision",
                revision,
            ],
        );
        assert_eq!(
            expect_fail("schedules.trigger", &out),
            "INVALID_ARGUMENT",
            "the disabled gate precedes the revision CAS (revision `{revision}`)"
        );
    }

    // ---- 暂停的 once 不点火：到期后历史仍为空。
    let once_at = now_ms_plus(2_000);
    let refs = configure("j3f-inert", &idle_session, "once", &once_at);
    let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
    let inert_sched = op(root, &refs);
    let inert_revision = inert_sched["schedule"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    op(
        root,
        &[
            "schedules",
            "set-enabled",
            "--output",
            "json",
            "--schedule-id",
            "j3f-inert",
            "--enabled",
            "false",
            "--expected-revision",
            &inert_revision,
        ],
    );
    tokio::time::sleep(Duration::from_secs(4)).await;
    let page = history(root, "j3f-inert").await;
    assert_eq!(
        page["items"].as_array().map(Vec::len),
        Some(0),
        "a paused schedule does not fire"
    );

    // ---- NOT_FOUND 家族与非法时区。
    let out = await_cli(
        root,
        &[
            "schedules",
            "history",
            "--output",
            "json",
            "--schedule-id",
            "j3-none",
        ],
    );
    assert_eq!(expect_fail("schedules.history", &out), "NOT_FOUND");
    let out = await_cli(
        root,
        &[
            "schedules",
            "stop-execution",
            "--output",
            "json",
            "--schedule-id",
            "j3f-busy",
            "--execution-id",
            "exec-none",
        ],
    );
    assert_eq!(expect_fail("schedules.stop_execution", &out), "NOT_FOUND");
    let out = await_cli(
        root,
        &[
            "schedules",
            "configure",
            "--output",
            "json",
            "--schedule-id",
            "j3f-bad-tz",
            "--project-id",
            &project_id,
            "--session-id",
            &idle_session,
            "--kind",
            "cron",
            "--expression",
            "* * * * *",
            "--timezone",
            "Mars/Olympus",
            "--message",
            "hello",
            "--expected-revision",
            "absent",
        ],
    );
    assert_eq!(expect_fail("schedules.configure", &out), "INVALID_ARGUMENT");

    // 清障：占用会话的 run 显式取消，不靠进程退出兜底。
    op(
        root,
        &[
            "run",
            "cancel",
            "--output",
            "json",
            "--run-id",
            &blocking_run_id,
        ],
    );
}
