//! S5 业务旅程 J6（上下文与输入编排）端到端：
//! `_tmp/20261119-s5/journeys-execution-plan.md` §3 J6 的命令序列，经真实
//! `agentcpm-pd` 二进制与 `/v1/ws` 传输、隔离 `AGENTCPM_PD_HOME`、
//! `common_v1` 挂具的 envelope schema 校验。
//!
//! 旅程判据（计划文档原文）：stdout + run.events 终态（replaced/committed
//! 状态机）；rollback 后无悬挂 run。
//!
//! - `j6_context_and_input_orchestration_journey`：`context styles` →
//!   `context configure`（thinking + 风格覆盖）→ `input submit`（argument
//!   源）与 `input follow`（多轮逐行编排，NDJSON 帧流 + 批次日志落盘）→
//!   `context inspect`（来源/预算）→ 洪泛 run 进行中 `context compact`
//!   被拒（SESSION_BUSY 安全边界）→ run 终态后 compact 成功并 inspect
//!   复核。
//! - `j6_replacement_commit_and_rollback_journey`：末轮替换事务
//!   prepare → get → commit（新 run 受理 + 原子确认，run.events 终态 +
//!   replacement.get 的 committed 状态机）→ prepare → rollback（撤销
//!   分支；rollback 后无悬挂 run）。故障负例：陈旧 turn（TURN_CONFLICT）、
//!   并发 prepare（REPLACEMENT_BUSY）、错 revision（REVISION_CONFLICT）、
//!   回滚已 committed plan（ALREADY_COMMITTED）。
//!
//! history 种子走 t007 登记的联调点（`V1Store::append_history_entries`）：
//! run→history 投影未接线前这是唯一的生产原语路径（既有
//! `e2e_v1_input_replacement.rs` 同法）。

// The suite spawns the embedded gateway through the CLI feature; without it
// there is nothing to test.
#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use common_v1::{
    CliOutcome, OP_VERSION, TestHome, await_cli, cli_with_stdin, expect_ok, init_home,
    init_home_instance, registry, submit_run, wait_run,
};

/// 解析失败 envelope：单行、`outcome=failed`，返回合同错误码。
fn expect_fail(method: &str, out: &CliOutcome) -> String {
    assert_ne!(
        out.code, 0,
        "`{method}` must exit non-zero on a contract failure; stdout: {}",
        out.stdout
    );
    let lines: Vec<&str> = out
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 1, "`{method}` stdout: {}", out.stdout);
    let value: Value = serde_json::from_str(lines[0])
        .unwrap_or_else(|e| panic!("`{method}` stdout not JSON: {e}"));
    assert_eq!(value["version"], OP_VERSION, "{value}");
    assert_eq!(value["outcome"], "failed", "{value}");
    value["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// 播种一个带真实授权根的项目、工作区和会话（t013/context e2e 同法）。
async fn seed_workspace(tag: &str) -> (TestHome, PathBuf, String, String, String) {
    let home = TestHome::new(tag);
    init_home(home.path());
    let instance_id = init_home_instance(home.path());

    let workspace_rel = format!("{tag}-ws");
    let out = await_cli(
        home.path(),
        &[
            "directories",
            "create",
            "--output",
            "json",
            "--path",
            &json!({
                "authorizedRootId": "default",
                "relativePath": {"encoding": "utf8", "value": workspace_rel},
            })
            .to_string(),
            "--request-key",
            &format!("dirs-{tag}"),
        ],
    );
    expect_ok("directories.create", &out);
    let workspace = home.path().join("workspaces").join(&workspace_rel);
    assert!(workspace.is_dir(), "workspace seeded");

    let project_id = format!("p-{tag}");
    let out = await_cli(
        home.path(),
        &[
            "projects",
            "register",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--name",
            "S5 Journey J6",
            "--environment-id",
            "local",
            "--root",
            &format!(
                "{{\"instanceId\":\"{instance_id}\",\"rootId\":\"default\",\"relativePath\":\"{workspace_rel}\"}}"
            ),
        ],
    );
    expect_ok("project.register", &out);
    let session_id = format!("s-{tag}");
    let out = await_cli(
        home.path(),
        &[
            "sessions",
            "create",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--project-id",
            &project_id,
        ],
    );
    expect_ok("session.create", &out);
    (home, workspace, project_id, session_id, instance_id)
}

fn write_style(dir: &Path, relative: &str, body: &str) {
    let path = dir.join(format!("{relative}.md"));
    std::fs::create_dir_all(path.parent().expect("style parent")).unwrap();
    std::fs::write(path, body).unwrap();
}

async fn context_inspect(root: &Path, session_id: &str) -> Value {
    let out = await_cli(
        root,
        &[
            "context",
            "inspect",
            "--output",
            "json",
            "--session-id",
            session_id,
        ],
    );
    expect_ok("context.inspect", &out)
}

fn source_value<'a>(result: &'a Value, key: &str) -> &'a Value {
    result["sources"]
        .as_array()
        .expect("inspect sources")
        .iter()
        .find(|pair| pair["key"].as_str() == Some(key))
        .map(|pair| &pair["value"])
        .expect("source present")
}

async fn wait_operation(root: &Path, operation_id: &str) -> Value {
    for _ in 0..400 {
        let out = await_cli(
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
        if let Ok(page) =
            serde_json::from_str::<Value>(out.stdout.lines().next().unwrap_or_default())
        {
            let operation = &page["result"]["operation"];
            if matches!(
                operation["state"].as_str(),
                Some("succeeded" | "failed" | "cancelled" | "interrupted" | "result_unknown")
            ) {
                return operation.clone();
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("operation `{operation_id}` never became terminal");
}

/// The `run.output.delta` texts a run emitted (the offline driver echoes the
/// complete prompt in the single delta before completing).
async fn run_delta_texts(root: &Path, run_id: &str) -> Vec<String> {
    let page = await_cli(
        root,
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

/// 一个 run 的全部事件类型（run.events 单页；替换事务的 run 事件数远小于
/// 页上限，洪泛 run 不走本断言）。
async fn run_event_types(root: &Path, run_id: &str) -> Vec<String> {
    let page = await_cli(
        root,
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
        .map(|e| e["type"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// 解析 `input follow` 的 NDJSON 帧流：每行一个 envelope（schema 校验走
/// `input.follow` 的 result schema），返回 frame 投影数组。
fn follow_frames(out: &CliOutcome) -> Vec<Value> {
    let lines: Vec<&str> = out
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert!(
        !lines.is_empty(),
        "`input follow` printed no frames; stderr: {}",
        out.stderr
    );
    lines
        .iter()
        .map(|line| {
            let value: Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("`input follow` frame is not JSON: {e}\n{line}"));
            assert_eq!(value["version"], OP_VERSION, "{value}");
            assert!(
                value["requestId"].as_str().is_some_and(|s| !s.is_empty()),
                "{value}"
            );
            assert_eq!(value["outcome"], "succeeded", "{value}");
            let result = value["result"].clone();
            registry()
                .validate_result("input.follow", &result)
                .unwrap_or_else(|e| panic!("input.follow frame failed schema validation: {e:?}"));
            result["frame"].clone()
        })
        .collect()
}

/// J6 主旅程：styles 目录 → configure（thinking + 风格覆盖）→ input
/// submit / input follow → inspect → 洪泛下 compact 的安全边界。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn j6_context_and_input_orchestration_journey() {
    let (home, _workspace, project_id, session_id, _instance_id) = seed_workspace("j6-ctx").await;
    let root = home.path();

    // 1) 风格目录：user 命名空间播种一个风格。
    write_style(&root.join("styles"), "concise", "user concise body");

    // 2) 有效模型链（thinking 能力声明 + session 级 selection）。
    let out = await_cli(
        root,
        &[
            "providers",
            "write",
            "--output",
            "json",
            "--provider-id",
            "prov-j6",
            "--name",
            "j6 journey provider",
            "--protocol",
            "openai",
            "--endpoint",
            "https://api.example.com/v1",
        ],
    );
    expect_ok("provider.write", &out);
    let out = await_cli(
        root,
        &[
            "models",
            "write",
            "--output",
            "json",
            "--model",
            "{\"providerId\":\"prov-j6\",\"modelId\":\"m-j6\"}",
            "--name",
            "j6 journey model",
            "--declared-capabilities",
            "[{\"key\":\"thinking\",\"value\":[\"low\",\"high\"]}]",
        ],
    );
    expect_ok("model.write", &out);
    let scope = format!(
        "{{\"kind\":\"session\",\"projectId\":\"{project_id}\",\"sessionId\":\"{session_id}\"}}"
    );
    let out = await_cli(
        root,
        &[
            "models",
            "selection",
            "get",
            "--output",
            "json",
            "--scope",
            &scope,
        ],
    );
    let base = expect_ok("model_selection.get", &out);
    let selection_rev = base["revision"].as_str().unwrap().to_owned();
    let out = await_cli(
        root,
        &[
            "models",
            "selection",
            "set",
            "--output",
            "json",
            "--scope",
            &scope,
            "--expected-revision",
            &selection_rev,
            "--model",
            "{\"providerId\":\"prov-j6\",\"modelId\":\"m-j6\"}",
        ],
    );
    expect_ok("model_selection.set", &out);

    // 3) context styles：播种的 user 风格在 session 作用域可见。
    let out = await_cli(
        root,
        &["context", "styles", "--output", "json", "--scope", &scope],
    );
    let listed = expect_ok("context.styles", &out);
    let ids: Vec<String> = listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["styleId"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, vec!["user:concise".to_owned()]);
    assert_eq!(listed["items"][0]["available"], json!(true));

    // 4) inspect 基线 + configure（thinking + 风格覆盖）。
    let inspect = context_inspect(root, &session_id).await;
    assert_eq!(
        source_value(&inspect, "effectiveModel"),
        &json!({"providerId": "prov-j6", "modelId": "m-j6"})
    );
    assert_eq!(source_value(&inspect, "thinking"), &json!(Value::Null));
    let base_revision = inspect["revision"].as_str().unwrap().to_owned();

    let out = await_cli(
        root,
        &[
            "context",
            "configure",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--expected-revision",
            &base_revision,
            "--thinking",
            "high",
            "--output-style-id",
            "user:concise",
        ],
    );
    let configured = expect_ok("context.configure", &out);
    let setting = |result: &Value, key: &str| -> Value {
        result["values"]
            .as_array()
            .expect("configure values")
            .iter()
            .find(|entry| entry["key"].as_str() == Some(key))
            .expect("setting present")["value"]
            .clone()
    };
    assert_eq!(setting(&configured, "thinking"), json!("high"));
    assert_eq!(setting(&configured, "outputStyleId"), json!("user:concise"));
    let configured_revision = configured["revision"].as_str().unwrap().to_owned();
    assert_ne!(configured_revision, base_revision);

    let inspect = context_inspect(root, &session_id).await;
    assert_eq!(source_value(&inspect, "thinking"), &json!("high"));
    assert_eq!(
        source_value(&inspect, "outputStyleId"),
        &json!("user:concise")
    );
    assert_eq!(
        inspect["revision"].as_str(),
        Some(configured_revision.as_str())
    );

    // 5) input submit（argument 源）：durable 受理，CLI 退出后 run 继续。
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
            "j6 argument turn",
            "--request-key",
            "j6-in-arg-1",
        ],
    );
    let result = expect_ok("input.submit", &out);
    let receipt = &result["receipt"];
    assert_eq!(receipt["sessionId"].as_str(), Some(session_id.as_str()));
    assert_eq!(receipt["accepted"], "durable");
    assert_eq!(receipt["state"], "queued");
    let arg_run = receipt["runId"].as_str().unwrap().to_owned();
    let waited = wait_run(root, &arg_run);
    let waited = expect_ok("run.wait", &waited);
    assert_eq!(waited["run"]["state"], "succeeded");
    assert_eq!(
        run_delta_texts(root, &arg_run).await,
        vec!["j6 argument turn"]
    );

    // 6) input follow（多轮逐行编排）：两行输入 → 2 个 accepted 帧 + 1 个
    //    end 帧，帧序递增，逐帧过 result schema。
    let out = cli_with_stdin(
        root,
        &["input", "follow", "--session-id", &session_id],
        Some("j6 follow one\nj6 follow two\n"),
    )
    .await;
    assert_eq!(
        out.code, 0,
        "`input follow` must exit 0; stderr: {}",
        out.stderr
    );
    let frames = follow_frames(&out);
    assert_eq!(frames.len(), 3, "two accepted frames plus the end frame");
    let sequences: Vec<u64> = frames
        .iter()
        .map(|f| f["sequence"].as_u64().expect("frame sequence"))
        .collect();
    assert_eq!(
        sequences,
        vec![0, 1, 2],
        "frame sequence must advance per frame"
    );
    let kinds: Vec<&str> = frames
        .iter()
        .map(|f| f["type"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(kinds, vec!["accepted", "accepted", "end"]);
    let follow_runs: Vec<String> = frames
        .iter()
        .filter(|f| f["type"] == json!("accepted"))
        .map(|f| f["receipt"]["runId"].as_str().unwrap().to_owned())
        .collect();
    assert!(!follow_runs[0].is_empty());
    assert!(!follow_runs[1].is_empty());
    assert_ne!(follow_runs[0], follow_runs[1], "each line is its own run");
    for (run_id, expected) in [
        (&follow_runs[0], "j6 follow one"),
        (&follow_runs[1], "j6 follow two"),
    ] {
        let waited = wait_run(root, run_id);
        let waited = expect_ok("run.wait", &waited);
        assert_eq!(waited["run"]["state"], "succeeded");
        assert_eq!(run_delta_texts(root, run_id).await, vec![expected]);
    }
    // 批次日志落盘：`<home>/client/input-follow/<batch>.ndjson`，每行输入
    // 一条 submitted + 一条 accepted 记录（键在提交前持久化）。
    let journal_dir = root.join("client").join("input-follow");
    let journal: Vec<PathBuf> = std::fs::read_dir(&journal_dir)
        .expect("input-follow journal dir")
        .map(|e| e.expect("journal entry").path())
        .collect();
    assert_eq!(journal.len(), 1, "exactly one batch journal");
    let raw = std::fs::read_to_string(&journal[0]).expect("journal bytes");
    let records: Vec<Value> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("journal record"))
        .collect();
    assert_eq!(records.len(), 4, "submitted+accepted per input line");
    assert_eq!(records[0]["state"], json!("submitted"));
    assert_eq!(records[1]["state"], json!("accepted"));
    assert_eq!(records[2]["state"], json!("submitted"));
    assert_eq!(records[3]["state"], json!("accepted"));

    // 7) inspect（来源/预算）：上下文覆盖与预算视图在场。
    let inspect = context_inspect(root, &session_id).await;
    assert_eq!(source_value(&inspect, "thinking"), &json!("high"));
    assert_eq!(
        source_value(&inspect, "outputStyleId"),
        &json!("user:concise")
    );
    assert!(
        inspect["budget"]["status"].as_str().is_some(),
        "inspect must project the context budget"
    );
    let flood_revision = inspect["revision"].as_str().unwrap().to_owned();

    // 8) 洪泛 run 进行中：compact 被拒（SESSION_BUSY 安全边界——洪泛/
    //    忙碌会话不压缩，在途轮不被损坏）。洪泛强度取 T7 既有口径的
    //    8000 事件（t7 先例在 60 s wait 内完成），给「洪泛仍在进行」留足
    //    判定窗口。
    let submitted = submit_run(
        root,
        &session_id,
        "[pd-driver:events=8000]",
        "j6-ctx-flood-1",
    )
    .await;
    let flood_run = submitted["runId"].as_str().unwrap().to_owned();
    let out = await_cli(
        root,
        &[
            "context",
            "compact",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--expected-revision",
            &flood_revision,
            "--request-key",
            "j6-ctx-compact-1",
        ],
    );
    assert_eq!(
        expect_fail("context.compact", &out),
        "SESSION_BUSY",
        "compaction must refuse a session with a run in flight"
    );

    // 洪泛 run 终态后：同一 key 重新受理（busy 拒绝可重试），压缩完成。
    let waited = wait_run(root, &flood_run);
    let waited = expect_ok("run.wait", &waited);
    assert_eq!(waited["run"]["state"], "succeeded");
    let out = await_cli(
        root,
        &[
            "context",
            "compact",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--expected-revision",
            &flood_revision,
            "--request-key",
            "j6-ctx-compact-1",
        ],
    );
    let receipt = expect_ok("context.compact", &out);
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
    let operation = wait_operation(root, &operation_id).await;
    assert_eq!(operation["state"], json!("succeeded"));
    let completion = &operation["result"];
    assert_eq!(completion["beforeRevision"], json!(flood_revision));
    assert_eq!(completion["afterRevision"], json!(flood_revision));

    // 9) inspect 复核 + 终态：会话无悬挂 run。
    let inspect = context_inspect(root, &session_id).await;
    assert_eq!(inspect["revision"], json!(flood_revision));
    assert_eq!(inspect["compactionState"], json!("none"));
    let out = await_cli(
        root,
        &[
            "sessions",
            "get",
            "--output",
            "json",
            "--session-id",
            &session_id,
        ],
    );
    let session = expect_ok("session.get", &out);
    assert_eq!(
        session["session"]["activeRunId"],
        json!(Value::Null),
        "the journey must leave no run in flight"
    );
}

/// 打开实例的 store（t007 登记的联调点：run→history 投影未接线前，历史
/// 条目经 `V1Store::append_history_entries` 原语播种）。仅在无 active run
/// 时打开一次。
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

/// 两轮对话：t-1（user + assistant），t-2（user + assistant）。
fn conversation() -> Vec<Value> {
    vec![
        json!({"turnId": "t-1", "role": "user", "content": [{"kind": "text", "text": "first question"}]}),
        json!({"turnId": "t-1", "role": "assistant", "content": [{"kind": "text", "text": "first answer"}]}),
        json!({"turnId": "t-2", "role": "user", "content": [{"kind": "text", "text": "second question"}]}),
        json!({"turnId": "t-2", "role": "assistant", "content": [{"kind": "text", "text": "second answer"}]}),
    ]
}

async fn history_turn_ids(root: &Path, session_id: &str) -> Vec<String> {
    let out = await_cli(
        root,
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

/// 当前 session revision（replacement.prepare 的 CAS 令牌）。
async fn session_revision(root: &Path, session_id: &str) -> String {
    let out = await_cli(
        root,
        &[
            "sessions",
            "get",
            "--output",
            "json",
            "--session-id",
            session_id,
        ],
    );
    let session = expect_ok("session.get", &out);
    session["session"]["revision"]
        .as_str()
        .expect("session revision")
        .to_owned()
}

/// J6 替换事务旅程：prepare → get → commit（新 run 受理 + 原子确认 +
/// run.events 终态）→ prepare → rollback（撤销后无悬挂 run）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn j6_replacement_commit_and_rollback_journey() {
    let (home, _workspace, project_id, session_id, _instance_id) = seed_workspace("j6-repl").await;
    let root = home.path();

    // 历史（两轮）经 t007 联调点原语播种；此刻旅程尚无 run。
    let store = seeding_store(root);
    store
        .append_history_entries(&session_id, &conversation())
        .unwrap();
    assert_eq!(history_turn_ids(root, &session_id).await.len(), 4);
    let session_rev = session_revision(root, &session_id).await;

    // 故障负例：陈旧 turn（末轮是 t-2）→ TURN_CONFLICT，历史原样。
    let out = await_cli(
        root,
        &[
            "replacement",
            "prepare",
            "--output",
            "json",
            "--replacement-id",
            "j6-rep-stale",
            "--session-id",
            &session_id,
            "--expected-revision",
            &session_rev,
            "--expected-turn-id",
            "t-1",
            "--ttl-ms",
            "60000",
        ],
    );
    assert_eq!(expect_fail("replacement.prepare", &out), "TURN_CONFLICT");
    assert_eq!(history_turn_ids(root, &session_id).await.len(), 4);

    // prepare：冻结末轮边界（不截断可见历史）。
    let out = await_cli(
        root,
        &[
            "replacement",
            "prepare",
            "--output",
            "json",
            "--replacement-id",
            "j6-rep-1",
            "--session-id",
            &session_id,
            "--expected-revision",
            &session_rev,
            "--expected-turn-id",
            "t-2",
            "--ttl-ms",
            "120000",
        ],
    );
    let prepared = expect_ok("replacement.prepare", &out);
    let replacement = &prepared["replacement"];
    assert_eq!(replacement["replacementId"].as_str(), Some("j6-rep-1"));
    assert_eq!(replacement["expectedTurnId"].as_str(), Some("t-2"));
    assert_eq!(replacement["phase"], json!("prepared"));
    assert_eq!(replacement["state"], json!("waiting"));
    assert_eq!(replacement["replacementRunId"], json!(Value::Null));
    let base_revision = replacement["baseRevision"]
        .as_str()
        .expect("baseRevision")
        .to_owned();
    assert!(
        !base_revision.is_empty(),
        "prepare must hand back the commit CAS token"
    );
    // prepare 不截断可见历史。
    assert_eq!(history_turn_ids(root, &session_id).await.len(), 4);

    // 故障负例：同会话并发 prepare → REPLACEMENT_BUSY。
    let out = await_cli(
        root,
        &[
            "replacement",
            "prepare",
            "--output",
            "json",
            "--replacement-id",
            "j6-rep-other",
            "--session-id",
            &session_id,
            "--expected-revision",
            &session_rev,
            "--expected-turn-id",
            "t-2",
            "--ttl-ms",
            "60000",
        ],
    );
    assert_eq!(expect_fail("replacement.prepare", &out), "REPLACEMENT_BUSY");

    // get：prepared 投影。
    let out = await_cli(
        root,
        &[
            "replacement",
            "get",
            "--output",
            "json",
            "--replacement-id",
            "j6-rep-1",
        ],
    );
    let got = expect_ok("replacement.get", &out);
    assert_eq!(got["replacement"]["phase"], json!("prepared"));
    assert_eq!(got["replacement"]["state"], json!("waiting"));
    assert_eq!(
        got["replacement"]["baseRevision"].as_str(),
        Some(base_revision.as_str())
    );

    // 故障负例：错 expectedRevision 的 commit → REVISION_CONFLICT，历史完好。
    let out = await_cli(
        root,
        &[
            "replacement",
            "commit",
            "--output",
            "json",
            "--replacement-id",
            "j6-rep-1",
            "--input",
            "[{\"kind\":\"text\",\"text\":\"j6 replacement question\"}]",
            "--expected-revision",
            "j6-bogus-revision",
            "--request-key",
            "j6-commit-bad-rev",
        ],
    );
    assert_eq!(expect_fail("replacement.commit", &out), "REVISION_CONFLICT");
    assert_eq!(history_turn_ids(root, &session_id).await.len(), 4);

    // commit：一个事务内历史截断 + 新 run 受理（原子确认）。
    let out = await_cli(
        root,
        &[
            "replacement",
            "commit",
            "--output",
            "json",
            "--replacement-id",
            "j6-rep-1",
            "--input",
            "[{\"kind\":\"text\",\"text\":\"j6 replacement question\"}]",
            "--expected-revision",
            &base_revision,
            "--request-key",
            "j6-commit-1",
        ],
    );
    let receipt = expect_ok("replacement.commit", &out);
    assert_eq!(receipt["kind"], "run");
    assert_eq!(receipt["durable"], true);
    assert_eq!(receipt["state"], "queued");
    let run_id = receipt["runId"].as_str().unwrap().to_owned();

    // 新 run 终态 + run.events 终态事件（replaced/committed 状态机的执行侧）。
    let waited = wait_run(root, &run_id);
    let waited = expect_ok("run.wait", &waited);
    assert_eq!(waited["run"]["state"], "succeeded");
    let event_types = run_event_types(root, &run_id).await;
    assert!(
        event_types.iter().any(|t| t == "run.completed"),
        "the replacement run must reach its terminal transition event; got {event_types:?}"
    );
    let out = await_cli(
        root,
        &["run", "get", "--output", "json", "--run-id", &run_id],
    );
    let run = expect_ok("run.get", &out);
    assert_eq!(run["run"]["continuedFrom"].as_str(), Some("t-2"));

    // 替换生效：只剩 t-1。
    assert_eq!(
        history_turn_ids(root, &session_id).await,
        vec!["t-1", "t-1"]
    );

    // get：committed 投影（state 映射新 run 终态，replacementRunId 指向它）。
    let out = await_cli(
        root,
        &[
            "replacement",
            "get",
            "--output",
            "json",
            "--replacement-id",
            "j6-rep-1",
        ],
    );
    let got = expect_ok("replacement.get", &out);
    assert_eq!(got["replacement"]["phase"], json!("committed"));
    assert_eq!(got["replacement"]["state"], json!("succeeded"));
    assert_eq!(
        got["replacement"]["replacementRunId"].as_str(),
        Some(run_id.as_str())
    );

    // 故障负例：回滚已 committed 的 plan → ALREADY_COMMITTED。
    let out = await_cli(
        root,
        &[
            "replacement",
            "rollback",
            "--output",
            "json",
            "--replacement-id",
            "j6-rep-1",
            "--expected-revision",
            &base_revision,
        ],
    );
    assert_eq!(
        expect_fail("replacement.rollback", &out),
        "ALREADY_COMMITTED"
    );

    // rollback 分支：对当前末轮（t-1）prepare → rollback 撤销。
    let session_rev_after = session_revision(root, &session_id).await;
    let out = await_cli(
        root,
        &[
            "replacement",
            "prepare",
            "--output",
            "json",
            "--replacement-id",
            "j6-rep-2",
            "--session-id",
            &session_id,
            "--expected-revision",
            &session_rev_after,
            "--expected-turn-id",
            "t-1",
            "--ttl-ms",
            "60000",
        ],
    );
    let prepared = expect_ok("replacement.prepare", &out);
    let base_revision_2 = prepared["replacement"]["baseRevision"]
        .as_str()
        .expect("baseRevision")
        .to_owned();
    let out = await_cli(
        root,
        &[
            "replacement",
            "rollback",
            "--output",
            "json",
            "--replacement-id",
            "j6-rep-2",
            "--expected-revision",
            &base_revision_2,
        ],
    );
    let rolled = expect_ok("replacement.rollback", &out);
    assert_eq!(rolled["replacement"]["phase"], json!("rolledBack"));
    assert_eq!(rolled["replacement"]["state"], json!("cancelled"));

    // rollback 后无悬挂 run：activeRunId 为空 + 非终态 run 列表为空。
    let out = await_cli(
        root,
        &[
            "sessions",
            "get",
            "--output",
            "json",
            "--session-id",
            &session_id,
        ],
    );
    let session = expect_ok("session.get", &out);
    assert_eq!(session["session"]["activeRunId"], json!(Value::Null));
    let out = await_cli(
        root,
        &[
            "run",
            "list",
            "--output",
            "json",
            "--scope",
            &format!(
                "{{\"kind\":\"session\",\"projectId\":\"{project_id}\",\"sessionId\":\"{session_id}\"}}"
            ),
            "--states",
            "[\"queued\",\"running\",\"waiting\",\"cancelling\"]",
        ],
    );
    let listed = expect_ok("run.list", &out);
    assert_eq!(
        listed["items"].as_array().unwrap().len(),
        0,
        "no run may be left in flight after the rollback"
    );

    // rollback 未截断历史（prepare 只冻结，撤销无需恢复动作）。
    assert_eq!(
        history_turn_ids(root, &session_id).await,
        vec!["t-1", "t-1"]
    );

    // get：rolledBack 投影稳定。
    let out = await_cli(
        root,
        &[
            "replacement",
            "get",
            "--output",
            "json",
            "--replacement-id",
            "j6-rep-2",
        ],
    );
    let got = expect_ok("replacement.get", &out);
    assert_eq!(got["replacement"]["phase"], json!("rolledBack"));
    assert_eq!(got["replacement"]["state"], json!("cancelled"));
}
