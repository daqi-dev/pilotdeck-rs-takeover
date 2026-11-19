//! S5 journey F8: 终端租约过期与接管（`_tmp/20261119-s5/journeys-execution-plan.md` §3 F8）。
//!
//! t015 已有租约矩阵的单操作面;本文件做旅程化的故障组合,单一 home/terminal
//! 走完整序列:
//!
//! 1. `terminal create` + `terminal control`（acquire,controlEpoch 0→1,token 只经
//!    `api invoke --input-json` 的临时文件走,不上 argv）→ `terminal input` 打点
//!    marker A（inputSeq=1）→ 越序探测 `INPUT_SEQUENCE_GAP`（水位恰 1）。
//! 2. 注入一（真等租约到期）:`LEASE_TTL_SECS=60` 是编译常量、无注入缝
//!    （W0 按任务书未建,见 w0 报告）,按计划的替代方案真等:acquire 后的
//!    66s（60s TTL + 6s 余量）内本测试不做任何续约。
//! 3. 判据:过期后 `terminal input` 被拒（`LEASE_EXPIRED`）;`terminal control`
//!    renew 同样 `LEASE_EXPIRED`;`terminal control` takeover（epoch 1→2）后
//!    恢复输入——marker B（inputSeq=2,续接水位,不重置）,越序探测证明水位恰 2。
//! 4. release（epoch→3）后启动 attach 进程（`--machine`,attach 自己 acquire）:
//!    `term.input` 打点 marker C,ack `acceptedThroughSeq=3`（水位续接）;随后
//!    SIGKILL attach（注入二,测试自有子进程）。
//! 5. 被杀 attach 的租约随进程停止续约:先以 takeover 语义探测 acquire——
//!    立即探测必得 `CONTROL_CONFLICT`（租约还活着,证明续约确实随进程停止）,
//!    轮询至成功即到期（≤60s）,随即 release 把终端让给新 attach。
//! 6. attach 重启:重放的 retained 窗口从 byteOffset 0 起、与首段逐字节一致,
//!    并越过首段终点继续（marker D,ack `acceptedThroughSeq=4`,inputSeq 连续）;
//!    两段输出流 outputEpoch 恒定、帧偏移连续无缝（不重不漏,outputEpoch/gap
//!    语义,无 gap 帧）;与独立的一元 `terminal output` 逐字节游标走读对照,
//!    前缀逐字节相等 + FNV-1a 校验和一致。
//!
//! 注入只作用于测试自有子进程（TestHome 拉起的 gateway、本测试 spawn 的
//! attach 进程）,绝不触碰 `~/.pilotdeck` 或按名字匹配的系统进程。租约 token
//! 是敏感短凭证:只经临时目录里的输入文件传递,不上 argv、不进断言输出。
//!
//! 真等 60s×2 段,单测墙钟约 2.5 分钟。
//! 本地未跑（本机禁跑 cargo）,待集成者在 cb8 验证。

// The suite drives the real embedded gateway through the real binary; without
// the feature there is nothing to test.
#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio::sync::mpsc;

use common_v1::{CliOutcome, TestHome, await_cli, base_command, expect_ok, init_home, signal_pid};

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn b64_decode(encoded: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .unwrap_or_else(|e| panic!("invalid base64 in an output frame: {e}"))
}

/// FNV-1a 64-bit over the assembled byte stream（「输出字节流校验和一致」的
/// 显式判据;逐字节对照是更强的超集）。
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Registers project `p-f8` on the default authorized root so
/// `terminals.create` can resolve the working directory（t015 同形）。
async fn seed_terminal_project(home: &Path) -> String {
    let instance_id = common_v1::init_home_instance(home);
    let out = await_cli(
        home,
        &[
            "projects",
            "register",
            "--output",
            "json",
            "--project-id",
            "p-f8",
            "--name",
            "F8 Lease Journey",
            "--environment-id",
            "local",
            "--root",
            &format!(
                "{{\"instanceId\":\"{instance_id}\",\"rootId\":\"default\",\"relativePath\":\"f8\"}}"
            ),
        ],
    );
    expect_ok("project.register", &out)["project"]["projectId"]
        .as_str()
        .unwrap()
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

async fn terminal_of(home: &Path, terminal_id: &str) -> Value {
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

/// Runs one `api invoke` with a JSON input file（the only sanctioned way to
/// carry a lease token——token 不上 argv,t015 同形）。
async fn invoke_json(home: &Path, method: &str, input: &Value) -> CliOutcome {
    let path = home.join("f8-invoke-input.json");
    std::fs::write(&path, serde_json::to_string(input).unwrap()).unwrap();
    await_cli(
        home,
        &[
            "api",
            "invoke",
            "--output",
            "json",
            method,
            "--input-json",
            path.to_str().unwrap(),
        ],
    )
}

/// Parses a failed invocation: one `outcome=failed` envelope line, a non-zero
/// exit, and the contract error code。
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

/// One `terminals.input` attempt（token 经输入文件）。
async fn terminal_input(
    home: &Path,
    terminal_id: &str,
    server_epoch: &str,
    control_epoch: i64,
    token: &str,
    input_seq: i64,
    bytes: &[u8],
) -> CliOutcome {
    invoke_json(
        home,
        "terminals.input",
        &json!({
            "terminalId": terminal_id,
            "serverEpoch": server_epoch,
            "controlEpoch": control_epoch,
            "leaseToken": token,
            "inputSeq": input_seq,
            "bytesBase64": b64(bytes),
        }),
    )
    .await
}

/// Walks the unary（non-follow）`terminals.output` from the earliest retained
/// byte until caught up, returning the full byte stream plus its outputEpoch
/// — the independent oracle the attach streams are compared against。
async fn unary_output_walk(home: &Path, terminal_id: &str) -> (String, Vec<u8>) {
    let mut cursor: Option<Value> = None;
    let mut epoch: Option<String> = None;
    let mut bytes: Vec<u8> = Vec::new();
    for _ in 0..4096 {
        let mut input = json!({"terminalId": terminal_id, "follow": false});
        if let Some(c) = &cursor {
            input["cursor"] = c.clone();
        }
        let frame = expect_ok(
            "terminals.output",
            &invoke_json(home, "terminals.output", &input).await,
        );
        match frame["type"].as_str().unwrap_or_default() {
            "bytes" => {
                let e = frame["cursor"]["outputEpoch"]
                    .as_str()
                    .expect("epoch")
                    .to_owned();
                if let Some(prev) = &epoch {
                    assert_eq!(prev, &e, "the outputEpoch must be stable across the walk");
                } else {
                    epoch = Some(e);
                }
                let start = frame["cursor"]["byteOffset"].as_u64().expect("offset");
                assert_eq!(
                    start,
                    bytes.len() as u64,
                    "the walk must stay contiguous: {frame}"
                );
                bytes.extend_from_slice(&b64_decode(frame["bytesBase64"].as_str().unwrap_or("")));
                cursor = Some(frame["nextCursor"].clone());
            }
            "heartbeat" => return (epoch.expect("at least one bytes frame"), bytes),
            other => panic!("unexpected frame type `{other}` in the oracle walk: {frame}"),
        }
    }
    panic!("the unary output walk did not converge for `{terminal_id}`");
}

/// Reads `terminals.output` forward（cursor kept across retries）until the
/// marker bytes appear; heartbeat means caught-up-now, so it retries。
async fn wait_output_marker(home: &Path, terminal_id: &str, marker: &[u8]) {
    let mut cursor: Option<Value> = None;
    for _ in 0..100 {
        let mut input = json!({"terminalId": terminal_id, "follow": false});
        if let Some(c) = &cursor {
            input["cursor"] = c.clone();
        }
        let frame = expect_ok(
            "terminals.output",
            &invoke_json(home, "terminals.output", &input).await,
        );
        match frame["type"].as_str().unwrap_or_default() {
            "bytes" => {
                let chunk = b64_decode(frame["bytesBase64"].as_str().unwrap_or(""));
                cursor = Some(frame["nextCursor"].clone());
                if contains_bytes(&chunk, marker) {
                    return;
                }
            }
            "heartbeat" => {
                cursor = Some(frame["nextCursor"].clone());
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            other => panic!("unexpected frame type `{other}` waiting for the marker: {frame}"),
        }
    }
    panic!(
        "marker {:?} never reached the output stream",
        String::from_utf8_lossy(marker)
    );
}

// ---------------------------------------------------------------------------
// attach 进程挂具（machine 模式,`007 §4` host 段帧）
// ---------------------------------------------------------------------------

/// A live `terminal attach --machine` subprocess。Every `term.output` frame is
/// recorded at read time（the accounting base for the byte-offset judgments）;
/// the channel carries every parsed frame for control-flow waits（conn.state /
/// request acks）——frames skipped by a wait are never lost, the reader already
/// recorded them。
struct AttachProc {
    child: Child,
    stdin: Option<tokio::process::ChildStdin>,
    rx: mpsc::Receiver<String>,
    frames: Arc<Mutex<Vec<Value>>>,
    stderr_log: Arc<Mutex<String>>,
    next_id: usize,
}

impl AttachProc {
    async fn start(home: &Path, terminal_id: &str) -> Self {
        let mut cmd = base_command(home);
        cmd.args([
            "terminal",
            "attach",
            "--machine",
            "--terminal-id",
            terminal_id,
        ]);
        cmd.stdin(std::process::Stdio::piped());
        let mut child = cmd.spawn().expect("spawn terminal attach");
        let stdout = child.stdout.take().expect("piped attach stdout");
        let stderr = child.stderr.take().expect("piped attach stderr");
        let (tx, rx) = mpsc::channel::<String>(256);
        let frames: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let record = frames.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(value) = serde_json::from_str::<Value>(&line)
                    && value["method"] == "term.output"
                {
                    record.lock().unwrap().push(value["params"].clone());
                }
                if tx.send(line).await.is_err() {
                    break;
                }
            }
        });
        let stderr_log = Arc::new(Mutex::new(String::new()));
        let log = stderr_log.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut guard = log.lock().unwrap();
                if guard.len() < 256 * 1024 {
                    guard.push_str(&line);
                    guard.push('\n');
                }
            }
        });
        let stdin = child.stdin.take();
        Self {
            child,
            stdin,
            rx,
            frames,
            stderr_log,
            next_id: 0,
        }
    }

    async fn wait_for(
        &mut self,
        pred: impl Fn(&Value) -> bool,
        timeout: Duration,
        what: &str,
    ) -> Value {
        tokio::time::timeout(timeout, async {
            loop {
                let Some(line) = self.rx.recv().await else {
                    panic!(
                        "attach stdout closed while waiting for {what}; stderr:\n{}",
                        self.stderr_log.lock().unwrap()
                    );
                };
                if line.trim().is_empty() {
                    continue;
                }
                let value: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("non-JSON attach frame ({e}): {line}"));
                if pred(&value) {
                    return value;
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "timed out after {}ms waiting for {what} from attach; stderr:\n{}",
                timeout.as_millis(),
                self.stderr_log.lock().unwrap()
            )
        })
    }

    /// `conn.state ready`——attach 只有在拿到租约并开出订阅之后才报 ready。
    async fn wait_ready(&mut self) {
        let ready = self
            .wait_for(
                |v| v["method"] == "conn.state" && v["params"]["state"] == "ready",
                // 满载预算（判据不变）：attach 拿租约开订阅后的 ready 帧，30s→60s。
                Duration::from_secs(60),
                "conn.state ready from attach",
            )
            .await;
        assert!(ready["params"].get("reason").is_none(), "{ready}");
    }

    /// Sends one host `term.input` and awaits its ack。
    async fn send_input(&mut self, bytes: &[u8]) -> Value {
        self.next_id += 1;
        let id = format!("f8i-{}", self.next_id);
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "term.input",
            "params": {"bytesBase64": b64(bytes)},
        });
        let line = serde_json::to_string(&frame).unwrap();
        let stdin = self.stdin.as_mut().expect("attach stdin still open");
        stdin
            .write_all(line.as_bytes())
            .await
            .expect("write to attach");
        stdin.write_all(b"\n").await.expect("newline to attach");
        stdin.flush().await.expect("flush attach stdin");
        let ack = self
            .wait_for(
                |v| v.get("id").and_then(Value::as_str) == Some(id.as_str()),
                // 满载预算（判据不变）：单条 term.input 的回执帧，15s→60s。
                Duration::from_secs(60),
                "term.input ack from attach",
            )
            .await;
        assert!(
            ack.get("result").is_some(),
            "term.input must be acked, not errored: {ack}"
        );
        ack["result"].clone()
    }

    /// A quiet window for trailing output bytes, then the recorded stream.
    fn collect_output(&self, quiet: Duration) -> (String, Vec<u8>) {
        std::thread::sleep(quiet);
        let frames = self.frames.lock().unwrap().clone();
        let mut epoch: Option<String> = None;
        let mut bytes: Vec<u8> = Vec::new();
        for frame in &frames {
            assert_eq!(
                frame["type"], "bytes",
                "only bytes frames are expected（a gap would mean eviction）: {frame}"
            );
            let e = frame["cursor"]["outputEpoch"]
                .as_str()
                .expect("epoch")
                .to_owned();
            match &epoch {
                None => epoch = Some(e),
                Some(prev) => assert_eq!(prev, &e, "outputEpoch must be stable in one attach"),
            }
            let start = frame["cursor"]["byteOffset"].as_u64().expect("offset");
            assert_eq!(
                start,
                bytes.len() as u64,
                "attach frames must be contiguous from byteOffset 0: {frame}"
            );
            bytes.extend_from_slice(&b64_decode(frame["bytesBase64"].as_str().unwrap_or("")));
        }
        (
            epoch.expect("attach must deliver at least one output frame（the retained replay）"),
            bytes,
        )
    }

    /// SIGKILL（the fault injection）and reap the test's own child。
    async fn kill(&mut self) {
        if let Some(pid) = self.child.id() {
            signal_pid(pid, rustix::process::Signal::KILL);
        }
        // 满载预算（判据不变）：SIGKILL 后收尸子进程，10s→60s。
        let status = tokio::time::timeout(Duration::from_secs(60), self.child.wait())
            .await
            .expect("reap the killed attach")
            .expect("wait killed attach");
        assert!(
            status.code().is_none(),
            "a SIGKILLed attach is signalled, not exited: {status:?}"
        );
    }

    /// Closes stdin（EOF）——attach returns 0 on host EOF。
    async fn finish(mut self) -> i32 {
        self.stdin = None;
        // 满载预算（判据不变）：stdin EOF 后 attach 的干净退出，20s→60s。
        tokio::time::timeout(Duration::from_secs(60), self.child.wait())
            .await
            .expect("attach must exit after stdin EOF")
            .expect("wait attach")
            .code()
            .unwrap_or(-1)
    }
}

// ---------------------------------------------------------------------------
// The journey
// ---------------------------------------------------------------------------

/// F8: lease expiry refuses input and control renews; takeover restores the
/// input right without resetting the sequence watermark; a killed attach's
/// lease stops renewing（alive right after the kill, expired within the TTL）;
/// the restarted attach replays the retained window byte-identically and
/// continues past the old stream's end; the whole stream matches the
/// independent unary walk byte-for-byte and by checksum。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f8_lease_expiry_takeover_and_attach_resume() {
    let home = TestHome::new("f8-lease-attach");
    let root = home.path();
    init_home(root);
    let project_id = seed_terminal_project(root).await;

    // ------------------------------------------------------------------
    // 1. create + claim（acquire, epoch 0→1）。
    // ------------------------------------------------------------------
    let terminal = create_terminal(root, &project_id).await;
    let terminal_id = terminal["terminalId"].as_str().unwrap().to_owned();
    let server_epoch = terminal["serverEpoch"].as_str().unwrap().to_owned();
    let output_epoch = terminal["outputEpoch"].as_str().unwrap().to_owned();
    assert_eq!(terminal["state"], "running");
    assert_eq!(terminal["controlEpoch"], 0);
    assert_eq!(terminal["lastAcceptedInputSeq"], 0);

    let control = expect_ok(
        "terminals.control",
        &invoke_json(
            root,
            "terminals.control",
            &json!({"terminalId": terminal_id, "action": "acquire", "expectedControlEpoch": 0}),
        )
        .await,
    );
    let lease = control["lease"].clone();
    let token_1 = lease["leaseToken"].as_str().unwrap().to_owned();
    let epoch_1 = lease["controlEpoch"].as_i64().unwrap();
    assert_eq!(epoch_1, 1);
    assert_eq!(lease["lastAcceptedInputSeq"], 0, "{lease}");
    // The lease TTL anchor: no renewal from this test afterwards.
    let lease_deadline = tokio::time::Instant::now() + Duration::from_secs(66);

    // ------------------------------------------------------------------
    // 2. marker A（inputSeq=1）+ 越序探测（水位恰 1）。
    // ------------------------------------------------------------------
    let marker_a = b"echo f8mA7Q\n";
    let ack = expect_ok(
        "terminals.input",
        &terminal_input(
            root,
            &terminal_id,
            &server_epoch,
            epoch_1,
            &token_1,
            1,
            marker_a,
        )
        .await,
    );
    assert_eq!(ack["acceptedThroughSeq"], 1, "{ack}");
    assert_eq!(ack["deduplicated"], false, "{ack}");
    assert_eq!(
        ack["acceptedBytes"].as_u64(),
        Some(marker_a.len() as u64),
        "{ack}"
    );
    wait_output_marker(root, &terminal_id, b"f8mA7Q").await;

    let gap = terminal_input(
        root,
        &terminal_id,
        &server_epoch,
        epoch_1,
        &token_1,
        3,
        b"\n",
    )
    .await;
    assert_eq!(
        expect_fail("terminals.input", &gap),
        "INPUT_SEQUENCE_GAP",
        "inputSeq 3 after watermark 1 must be refused as a gap"
    );

    // ------------------------------------------------------------------
    // 3. 注入一:真等租约到期（LEASE_TTL_SECS=60,无注入缝;66s = TTL + 余量）。
    // ------------------------------------------------------------------
    tokio::time::sleep_until(lease_deadline).await;

    // 过期后 terminal input 被拒（租约错误码）。
    let expired = terminal_input(
        root,
        &terminal_id,
        &server_epoch,
        epoch_1,
        &token_1,
        2,
        b"\n",
    )
    .await;
    assert_eq!(
        expect_fail("terminals.input", &expired),
        "LEASE_EXPIRED",
        "the expired lease must refuse input"
    );
    // renew 同样被拒——过期租约不可续,只能重新接管。
    let renew = invoke_json(
        root,
        "terminals.control",
        &json!({
            "terminalId": terminal_id,
            "action": "renew",
            "expectedControlEpoch": epoch_1,
            "leaseToken": token_1,
        }),
    )
    .await;
    assert_eq!(
        expect_fail("terminals.control", &renew),
        "LEASE_EXPIRED",
        "renew on an expired lease must be refused"
    );

    // ------------------------------------------------------------------
    // 4. takeover（epoch 1→2）恢复输入:marker B（inputSeq=2,续接水位）,
    //    越序探测证明水位恰 2（接管不重置 inputSeq）。
    // ------------------------------------------------------------------
    let control = expect_ok(
        "terminals.control",
        &invoke_json(
            root,
            "terminals.control",
            &json!({
                "terminalId": terminal_id,
                "action": "takeover",
                "expectedControlEpoch": epoch_1,
            }),
        )
        .await,
    );
    let lease = control["lease"].clone();
    let token_2 = lease["leaseToken"].as_str().unwrap().to_owned();
    let epoch_2 = lease["controlEpoch"].as_i64().unwrap();
    assert_eq!(epoch_2, 2);
    assert_eq!(
        lease["lastAcceptedInputSeq"], 1,
        "接管续接既有水位: {lease}"
    );

    let marker_b = b"echo f8mB7Q\n";
    let ack = expect_ok(
        "terminals.input",
        &terminal_input(
            root,
            &terminal_id,
            &server_epoch,
            epoch_2,
            &token_2,
            2,
            marker_b,
        )
        .await,
    );
    assert_eq!(ack["acceptedThroughSeq"], 2, "{ack}");
    wait_output_marker(root, &terminal_id, b"f8mB7Q").await;
    let gap = terminal_input(
        root,
        &terminal_id,
        &server_epoch,
        epoch_2,
        &token_2,
        4,
        b"\n",
    )
    .await;
    assert_eq!(
        expect_fail("terminals.input", &gap),
        "INPUT_SEQUENCE_GAP",
        "inputSeq 4 after watermark 2 must be refused as a gap"
    );
    assert_eq!(
        terminal_of(root, &terminal_id).await["lastAcceptedInputSeq"],
        2,
        "the watermark stays at 2 after the takeover phase"
    );

    // ------------------------------------------------------------------
    // 5. release（epoch 2→3）→ attach #1（自己 acquire,续订由它负责）。
    // ------------------------------------------------------------------
    let released = expect_ok(
        "terminals.control",
        &invoke_json(
            root,
            "terminals.control",
            &json!({
                "terminalId": terminal_id,
                "action": "release",
                "expectedControlEpoch": epoch_2,
                "leaseToken": token_2,
            }),
        )
        .await,
    );
    assert_eq!(released["terminal"]["controlEpoch"], 3, "{released}");

    let mut attach_1 = AttachProc::start(root, &terminal_id).await;
    attach_1.wait_ready().await;
    let ack = attach_1.send_input(b"echo f8mC7Q\n").await;
    assert_eq!(
        ack["acceptedThroughSeq"], 3,
        "attach 续接接管阶段的水位（inputSeq 连续）: {ack}"
    );
    assert_eq!(ack["deduplicated"], false, "{ack}");
    // 等 marker C 的输出到达 + 尾随字节静默。
    // 满载预算（判据不变）：等 marker 经 PTY 抵达 attach 流，15s→60s。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if attach_1.frames.lock().unwrap().iter().any(|f| {
            contains_bytes(
                &b64_decode(f["bytesBase64"].as_str().unwrap_or("")),
                b"f8mC7Q",
            )
        }) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "marker C never reached the attach stream; stderr:\n{}",
            attach_1.stderr_log.lock().unwrap()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        terminal_of(root, &terminal_id).await["lastAcceptedInputSeq"],
        3,
        "the watermark advanced through attach #1"
    );
    let (epoch_s1, stream_1) = attach_1.collect_output(Duration::from_millis(800));
    assert_eq!(
        epoch_s1, output_epoch,
        "outputEpoch 恒定（同一终端,从未重启）"
    );

    // ------------------------------------------------------------------
    // 6. 注入二:SIGKILL attach #1 → 轮询接管探测:先 CONTROL_CONFLICT
    //    （被杀 attach 的租约仍活着——续约确实随进程停止）,到期后 acquire
    //    成功,立即 release 让新 attach 接手。
    // ------------------------------------------------------------------
    attach_1.kill().await;
    let expected_epoch = terminal_of(root, &terminal_id).await["controlEpoch"]
        .as_i64()
        .expect("controlEpoch");
    let mut saw_live_lease = false;
    let mut takeover_lease: Option<Value> = None;
    for _ in 0..45 {
        let probe = invoke_json(
            root,
            "terminals.control",
            &json!({
                "terminalId": terminal_id,
                "action": "acquire",
                "expectedControlEpoch": expected_epoch,
            }),
        )
        .await;
        if probe.code == 0 {
            takeover_lease = Some(expect_ok("terminals.control", &probe)["lease"].clone());
            break;
        }
        assert_eq!(
            expect_fail("terminals.control", &probe),
            "CONTROL_CONFLICT",
            "the killed attach's lease must hold the terminal until it expires"
        );
        saw_live_lease = true;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    assert!(
        saw_live_lease,
        "被杀 attach 的租约在紧随其后的探测里必须仍活着（续约停止的正面证据）"
    );
    let takeover_lease =
        takeover_lease.expect("the dead attach's lease must expire within the TTL");
    let token_3 = takeover_lease["leaseToken"].as_str().unwrap().to_owned();
    let epoch_3 = takeover_lease["controlEpoch"].as_i64().unwrap();
    let released = expect_ok(
        "terminals.control",
        &invoke_json(
            root,
            "terminals.control",
            &json!({
                "terminalId": terminal_id,
                "action": "release",
                "expectedControlEpoch": epoch_3,
                "leaseToken": token_3,
            }),
        )
        .await,
    );
    assert_eq!(
        released["terminal"]["controlEpoch"],
        epoch_3 + 1,
        "{released}"
    );

    // ------------------------------------------------------------------
    // 7. attach 重启:重放 retained 窗口 + 续读;marker D,inputSeq=4。
    // ------------------------------------------------------------------
    let mut attach_2 = AttachProc::start(root, &terminal_id).await;
    attach_2.wait_ready().await;
    let ack = attach_2.send_input(b"echo f8mD7Q\n").await;
    assert_eq!(
        ack["acceptedThroughSeq"], 4,
        "重启后的 attach 续接被杀 attach 留下的水位（inputSeq 连续）: {ack}"
    );
    // 满载预算（判据不变）：等 marker D 经 PTY 抵达 attach 流，15s→60s。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if attach_2.frames.lock().unwrap().iter().any(|f| {
            contains_bytes(
                &b64_decode(f["bytesBase64"].as_str().unwrap_or("")),
                b"f8mD7Q",
            )
        }) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "marker D never reached the attach stream; stderr:\n{}",
            attach_2.stderr_log.lock().unwrap()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (epoch_s2, stream_2) = attach_2.collect_output(Duration::from_millis(800));
    assert_eq!(epoch_s2, output_epoch, "重启前后 outputEpoch 恒定");
    let exit_code = attach_2.finish().await;
    assert_eq!(exit_code, 0, "attach #2 exits 0 on host EOF");

    // ------------------------------------------------------------------
    // 8. 独立一元走读为 oracle:前缀逐字节相等 + 校验和一致。
    // ------------------------------------------------------------------
    let (epoch_oracle, oracle) = unary_output_walk(root, &terminal_id).await;
    assert_eq!(
        epoch_oracle, output_epoch,
        "the oracle agrees on the outputEpoch"
    );
    let markers: [&[u8]; 4] = [b"f8mA7Q", b"f8mB7Q", b"f8mC7Q", b"f8mD7Q"];
    for marker in markers {
        assert!(
            contains_bytes(&oracle, marker),
            "marker {:?} missing from the oracle stream",
            String::from_utf8_lossy(marker)
        );
    }
    assert!(
        stream_2.len() > stream_1.len(),
        "重启后的 attach 必须越过首段终点继续（续读）: {} > {}",
        stream_2.len(),
        stream_1.len()
    );
    assert!(
        contains_bytes(&stream_1, b"f8mA7Q") && contains_bytes(&stream_1, b"f8mB7Q"),
        "attach #1 的重放必须覆盖接管阶段之前的输出"
    );
    assert!(
        contains_bytes(&stream_2, b"f8mC7Q") && contains_bytes(&stream_2, b"f8mD7Q"),
        "attach #2 的重放+续读必须覆盖被杀 attach 阶段与其后的输出"
    );
    // 不重不漏:两段各自是 oracle 的逐字节前缀;重叠区间（重放）字节一致,
    // union 无缝覆盖。校验和判据同源。
    assert!(
        oracle.len() >= stream_2.len(),
        "the oracle walk must reach at least as far as attach #2"
    );
    assert_eq!(
        &oracle[..stream_1.len()],
        stream_1.as_slice(),
        "attach #1 的字节流必须与 oracle 前缀逐字节一致"
    );
    assert_eq!(
        &oracle[..stream_2.len()],
        stream_2.as_slice(),
        "attach #2 的字节流（重放+续读）必须与 oracle 前缀逐字节一致"
    );
    assert_eq!(
        fnv1a(&oracle[..stream_2.len()]),
        fnv1a(&stream_2),
        "输出字节流校验和一致"
    );
    assert_eq!(
        terminal_of(root, &terminal_id).await["lastAcceptedInputSeq"],
        4,
        "the watermark ends at 4（inputSeq 连续全程）"
    );

    // ------------------------------------------------------------------
    // 9. 收尾:force close,终端终态 exited（不让 gateway 留着一个 shell）。
    // ------------------------------------------------------------------
    let closed = expect_ok(
        "terminals.close",
        &invoke_json(
            root,
            "terminals.close",
            &json!({
                "terminalId": terminal_id,
                "expectedServerEpoch": server_epoch,
                "mode": "force",
            }),
        )
        .await,
    );
    assert_eq!(closed["kind"], "operation", "{closed}");
    let mut terminal = Value::Null;
    for _ in 0..100 {
        terminal = terminal_of(root, &terminal_id).await;
        if terminal["state"] == "exited" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        terminal["state"], "exited",
        "force close 终止 shell: {terminal}"
    );
}
