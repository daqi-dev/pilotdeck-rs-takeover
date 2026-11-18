//! t007 e2e: explicit input submission and the last-turn replacement commit
//! (`cli.case.c008`, `cli.case.c009`, `cli.case.submit-unknown`,
//! `cli.case.operation.input.submit`, `cli.case.operation.replacement.commit`,
//! `cli.case.source-flow.c97e61ff` 的本任务切片).
//!
//! Every scenario drives the real `agentcpm-pd` binary over the real `/v1/ws`
//! transport against an isolated `AGENTCPM_PD_HOME`, using the shared
//! `common_v1` harness. Scenario -> acceptance mapping:
//!
//! - `input_submit_roundtrip` (c008/c009/operation.input.submit): argument
//!   and stdinText sources, durable acceptance with the client exiting right
//!   after (the run keeps going in the gateway), same-key dedup and
//!   different-content conflict, empty/oversized/invalid-UTF-8 stdin, and the
//!   pre-dispatch TARGET_MISMATCH rejection. `argument必须text` /
//!   `stdinText禁止text` is schema-enforced and covered by the in-crate unit
//!   tests (`local_ops/input.rs`).
//! - `replacement_commit_roundtrip` (operation.replacement.commit /
//!   source-flow regenerate): freeze (prepare storage primitive; the
//!   `replacement.prepare` handler belongs to the history task and is the
//!   documented联调点) -> atomic commit (history truncated + new run
//!   accepted) -> same-key replay without double truncation ->
//!   requests.lookup -> stale revision / moved boundary / committed-again /
//!   TTL expiry refusals with the original history intact.
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
use tokio::io::AsyncWriteExt;

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

/// Runs one CLI invocation feeding raw (possibly non-UTF-8) stdin bytes.
async fn cli_with_stdin_bytes(home: &Path, args: &[&str], stdin: Vec<u8>) -> CliOutcome {
    let mut cmd = base_command(home);
    cmd.args(args);
    cmd.stdin(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn agentcpm-pd");
    let mut handle = child.stdin.take().expect("piped stdin");
    handle.write_all(&stdin).await.expect("write stdin bytes");
    drop(handle);
    let output = tokio::time::timeout(CLI_TIMEOUT, child.wait_with_output())
        .await
        .expect("cli invocation timed out")
        .expect("wait for cli");
    CliOutcome {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// The `run.output.delta` texts a run emitted (the offline driver echoes the
/// complete prompt in the single delta before completing).
async fn run_delta_texts(home: &Path, run_id: &str) -> Vec<String> {
    let page = await_cli(
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
    let page = expect_ok("run.events", &page);
    page["frame"]["events"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter(|e| e["type"].as_str() == Some("run.output.delta"))
        .filter_map(|e| {
            e["payload"]["text"]
                .as_str()
                .map(|raw| raw.strip_prefix("completed: ").unwrap_or(raw).to_owned())
        })
        .collect()
}

/// Opens the instance's store in-process to seed history and prepared
/// replacement plans. This is the documented联调点: until the history task
/// delivers `replacement.prepare`, the plan row is created through the same
/// `V1Store::freeze_replacement_plan` primitive that handler will call. The
/// handle is opened once while no runs are active, so the startup recovery
/// pass of `open_and_recover` observes nothing to interrupt.
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

/// Two-turn conversation: t-1 (user + assistant), t-2 (user + assistant).
fn conversation() -> Vec<Value> {
    vec![
        json!({"turnId": "t-1", "role": "user", "content": [{"kind": "text", "text": "first question"}]}),
        json!({"turnId": "t-1", "role": "assistant", "content": [{"kind": "text", "text": "first answer"}]}),
        json!({"turnId": "t-2", "role": "user", "content": [{"kind": "text", "text": "second question"}]}),
        json!({"turnId": "t-2", "role": "assistant", "content": [{"kind": "text", "text": "second answer"}]}),
    ]
}

async fn history_turn_ids(home: &Path, session_id: &str) -> Vec<String> {
    let out = await_cli(
        home,
        &[
            "history",
            "read",
            "--output",
            "json",
            "--session-id",
            session_id,
            "--page",
            "{\"limit\":500}",
        ],
    );
    let page = expect_ok("history.read", &out);
    page["items"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|e| e["turnId"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_submit_roundtrip() {
    let home = TestHome::new("t007-input");
    let root = home.path();
    init_home(root).await;
    let (_project_id, session_id) = seed_project_session(root, "t007i").await;

    // --- argument source: durable acceptance, client exits, run continues ---
    let out = await_cli(
        root,
        &[
            "input",
            "submit",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--source",
            "argument",
            "--text",
            "hello argv",
            "--request-key",
            "in-arg-1",
        ],
    );
    let result = expect_ok("input.submit", &out);
    let receipt = &result["receipt"];
    assert_eq!(receipt["sessionId"].as_str().unwrap(), session_id);
    assert_eq!(receipt["accepted"], "durable");
    assert_eq!(receipt["state"], "queued");
    assert!(
        receipt["acceptedRevision"]
            .as_str()
            .is_some_and(|r| !r.is_empty())
    );
    let run_id = receipt["runId"].as_str().unwrap().to_owned();

    // The submit CLI already exited; the run keeps going in the gateway (c008).
    let waited = wait_run(root, &run_id);
    let waited = expect_ok("run.wait", &waited);
    assert_eq!(waited["run"]["state"], "succeeded");
    // EOF/完整性：完整输入恰好提交一次（offline driver 把完整 prompt 回显进
    // 唯一一条 delta）。
    assert_eq!(run_delta_texts(root, &run_id).await, vec!["hello argv"]);

    // --- same key, same content: deduplicated to the same run (c009) ---
    let out = await_cli(
        root,
        &[
            "input",
            "submit",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--source",
            "argument",
            "--text",
            "hello argv",
            "--request-key",
            "in-arg-1",
        ],
    );
    let replay = expect_ok("input.submit", &out);
    assert_eq!(
        replay["receipt"]["runId"].as_str().unwrap(),
        run_id,
        "the same key must return the original run"
    );

    // --- same key, different content: IDEMPOTENCY_CONFLICT ---
    let out = await_cli(
        root,
        &[
            "input",
            "submit",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--source",
            "argument",
            "--text",
            "different content",
            "--request-key",
            "in-arg-1",
        ],
    );
    assert_eq!(expect_fail("input.submit", &out), "IDEMPOTENCY_CONFLICT");

    // --- stdinText source: full multi-line input submitted once at EOF ---
    let out = cli_with_stdin(
        root,
        &[
            "input",
            "submit",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--source",
            "stdinText",
            "--request-key",
            "in-stdin-1",
        ],
        Some("line one\nline two\n"),
    )
    .await;
    let result = expect_ok("input.submit", &out);
    let stdin_run = result["receipt"]["runId"].as_str().unwrap().to_owned();
    let waited = wait_run(root, &stdin_run);
    expect_ok("run.wait", &waited);
    // 尾部换行按 TS 基线承接去除，内部换行保留。
    assert_eq!(
        run_delta_texts(root, &stdin_run).await,
        vec!["line one\nline two"]
    );

    // --- empty stdin: INPUT_EMPTY, zero submission ---
    let out = cli_with_stdin(
        root,
        &[
            "input",
            "submit",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--source",
            "stdinText",
            "--request-key",
            "in-empty-1",
        ],
        Some(""),
    )
    .await;
    assert_eq!(expect_fail("input.submit", &out), "INPUT_EMPTY");

    // --- stdin is /dev/null (no TTY, no confirmation read): immediate
    //     INPUT_EMPTY instead of hanging (「无TTY不读确认」) ---
    let out = await_cli(
        root,
        &[
            "input",
            "submit",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--source",
            "stdinText",
        ],
    );
    assert_eq!(expect_fail("input.submit", &out), "INPUT_EMPTY");

    // --- invalid UTF-8 stdin: INVALID_UTF8, zero submission ---
    let out = cli_with_stdin_bytes(
        root,
        &[
            "input",
            "submit",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--source",
            "stdinText",
            "--request-key",
            "in-bad-utf8",
        ],
        b"caf\xe9 not utf8".to_vec(),
    )
    .await;
    assert_eq!(expect_fail("input.submit", &out), "INVALID_UTF8");

    // --- one stdin carrying two inputs: AMBIGUOUS_INPUT (the `--input-json -`
    //     document already consumed stdin; stdinText would read it again) ---
    let out = await_cli_with_json(
        root,
        &["input", "submit", "--output", "json", "--input-json", "-"],
        &json!({"sessionId": session_id, "source": "stdinText"}),
    )
    .await;
    assert_eq!(expect_fail("input.submit", &out), "AMBIGUOUS_INPUT");

    // --- oversized argument input: INPUT_TOO_LARGE, zero submission ---
    let big = "x".repeat(1024 * 1024 - 800);
    let big_path = root.join("big-input.json");
    std::fs::write(
        &big_path,
        serde_json::to_string(&json!({
            "sessionId": session_id,
            "source": "argument",
            "text": big,
        }))
        .unwrap(),
    )
    .unwrap();
    let out = await_cli(
        root,
        &[
            "input",
            "submit",
            "--output",
            "json",
            "--input-json",
            big_path.to_str().unwrap(),
        ],
    );
    assert_eq!(expect_fail("input.submit", &out), "INPUT_TOO_LARGE");

    // --- explicit targetId mismatch: rejected before dispatch ---
    let out = await_cli_with_json(
        root,
        &["input", "submit", "--output", "json", "--input-json", "-"],
        &json!({
            "sessionId": session_id,
            "source": "argument",
            "text": "x",
            "targetId": "some-other-target",
        }),
    )
    .await;
    assert_eq!(expect_fail("input.submit", &out), "TARGET_MISMATCH");
}

/// Feeds an explicit `--input-json` document through stdin.
async fn await_cli_with_json(home: &Path, args: &[&str], input: &Value) -> CliOutcome {
    cli_with_stdin(home, args, Some(&serde_json::to_string(input).unwrap())).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replacement_commit_roundtrip() {
    let home = TestHome::new("t007-repl");
    let root = home.path();
    init_home(root).await;
    let (_project_id, session_id) = seed_project_session(root, "t007r").await;

    // 联调点：prepare 侧（history 任务）尚未交付，用同一存储原语种下
    // prepared plan（会话此刻无在途 run，open_and_recover 的恢复为空操作）。
    let store = seeding_store(root);
    store
        .append_history_entries(&session_id, &conversation())
        .unwrap();
    assert_eq!(history_turn_ids(root, &session_id).await.len(), 4);

    // --- stale revision first: REVISION_CONFLICT, history intact ---
    let plan = store
        .freeze_replacement_plan(&session_id, "rep-1", "t-2", None)
        .unwrap();
    let rev = plan["revision"].as_str().unwrap().to_owned();
    let out = await_cli(
        root,
        &[
            "replacement",
            "commit",
            "--output",
            "json",
            "--replacement-id",
            "rep-1",
            "--input",
            "[{\"kind\":\"text\",\"text\":\"edited question\"}]",
            "--expected-revision",
            "rev-bogus",
            "--request-key",
            "rc-bad-rev",
        ],
    );
    assert_eq!(expect_fail("replacement.commit", &out), "REVISION_CONFLICT");
    assert_eq!(history_turn_ids(root, &session_id).await.len(), 4);

    // --- the atomic commit: history truncated + new run accepted ---
    let out = await_cli(
        root,
        &[
            "replacement",
            "commit",
            "--output",
            "json",
            "--replacement-id",
            "rep-1",
            "--input",
            "[{\"kind\":\"text\",\"text\":\"edited question\"}]",
            "--expected-revision",
            &rev,
            "--request-key",
            "rc-1",
        ],
    );
    let receipt = expect_ok("replacement.commit", &out);
    assert_eq!(receipt["kind"], "run");
    assert_eq!(receipt["durable"], true);
    assert_eq!(receipt["state"], "queued");
    let run_id = receipt["runId"].as_str().unwrap().to_owned();

    let waited = wait_run(root, &run_id);
    expect_ok("run.wait", &waited);

    // 新 run 与被替换轮可追溯：continuedFrom 指向被替换轮。
    let out = await_cli(
        root,
        &["run", "get", "--output", "json", "--run-id", &run_id],
    );
    let run = expect_ok("run.get", &out);
    assert_eq!(run["run"]["continuedFrom"].as_str().unwrap(), "t-2");

    // 替换生效：只剩 t-1。
    assert_eq!(
        history_turn_ids(root, &session_id).await,
        vec!["t-1", "t-1"]
    );

    // --- same key, same input: replayed receipt, no double truncation ---
    let out = await_cli(
        root,
        &[
            "replacement",
            "commit",
            "--output",
            "json",
            "--replacement-id",
            "rep-1",
            "--input",
            "[{\"kind\":\"text\",\"text\":\"edited question\"}]",
            "--expected-revision",
            &rev,
            "--request-key",
            "rc-1",
        ],
    );
    let replay = expect_ok("replacement.commit", &out);
    assert_eq!(replay["deduplicated"], true);
    assert_eq!(replay["runId"].as_str().unwrap(), run_id);
    assert_eq!(
        history_turn_ids(root, &session_id).await,
        vec!["t-1", "t-1"],
        "the replay must not truncate again"
    );

    // --- requests.lookup finds the original resource for the key ---
    let out = await_cli(
        root,
        &[
            "requests",
            "lookup",
            "--output",
            "json",
            "--method-id",
            "replacement.commit",
            "--idempotency-key",
            "rc-1",
        ],
    );
    let lookup = expect_ok("requests.lookup", &out);
    assert_eq!(lookup["requestState"], "accepted");
    assert_eq!(lookup["resource"]["runId"].as_str().unwrap(), run_id);

    // --- same key, different content: IDEMPOTENCY_CONFLICT ---
    let out = await_cli(
        root,
        &[
            "replacement",
            "commit",
            "--output",
            "json",
            "--replacement-id",
            "rep-1",
            "--input",
            "[{\"kind\":\"text\",\"text\":\"other\"}]",
            "--expected-revision",
            &rev,
            "--request-key",
            "rc-1",
        ],
    );
    assert_eq!(
        expect_fail("replacement.commit", &out),
        "IDEMPOTENCY_CONFLICT"
    );

    // --- committed plan committed again (new key): REPLACEMENT_EXPIRED ---
    let out = await_cli(
        root,
        &[
            "replacement",
            "commit",
            "--output",
            "json",
            "--replacement-id",
            "rep-1",
            "--input",
            "[{\"kind\":\"text\",\"text\":\"x\"}]",
            "--expected-revision",
            &rev,
            "--request-key",
            "rc-2",
        ],
    );
    assert_eq!(
        expect_fail("replacement.commit", &out),
        "REPLACEMENT_EXPIRED"
    );

    // --- moved boundary: TURN_CONFLICT, history intact ---
    let plan3 = store
        .freeze_replacement_plan(&session_id, "rep-3", "t-1", None)
        .unwrap();
    let rev3 = plan3["revision"].as_str().unwrap().to_owned();
    store
        .append_history_entries(
            &session_id,
            &[json!({
                "turnId": "t-9",
                "role": "user",
                "content": [{"kind": "text", "text": "history moved on"}],
            })],
        )
        .unwrap();
    let out = await_cli(
        root,
        &[
            "replacement",
            "commit",
            "--output",
            "json",
            "--replacement-id",
            "rep-3",
            "--input",
            "[{\"kind\":\"text\",\"text\":\"x\"}]",
            "--expected-revision",
            &rev3,
            "--request-key",
            "rc-3",
        ],
    );
    assert_eq!(expect_fail("replacement.commit", &out), "TURN_CONFLICT");
    assert_eq!(history_turn_ids(root, &session_id).await.len(), 3);

    // --- TTL expiry: REPLACEMENT_EXPIRED, history intact ---
    let plan4 = store
        .freeze_replacement_plan(&session_id, "rep-4", "t-9", Some(1))
        .unwrap();
    let rev4 = plan4["revision"].as_str().unwrap().to_owned();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let out = await_cli(
        root,
        &[
            "replacement",
            "commit",
            "--output",
            "json",
            "--replacement-id",
            "rep-4",
            "--input",
            "[{\"kind\":\"text\",\"text\":\"x\"}]",
            "--expected-revision",
            &rev4,
            "--request-key",
            "rc-4",
        ],
    );
    assert_eq!(
        expect_fail("replacement.commit", &out),
        "REPLACEMENT_EXPIRED"
    );
    assert_eq!(history_turn_ids(root, &session_id).await.len(), 3);

    // --- unknown plan: NOT_FOUND ---
    let out = await_cli(
        root,
        &[
            "replacement",
            "commit",
            "--output",
            "json",
            "--replacement-id",
            "rep-nope",
            "--input",
            "[{\"kind\":\"text\",\"text\":\"x\"}]",
            "--expected-revision",
            &rev4,
            "--request-key",
            "rc-5",
        ],
    );
    assert_eq!(expect_fail("replacement.commit", &out), "NOT_FOUND");
}
