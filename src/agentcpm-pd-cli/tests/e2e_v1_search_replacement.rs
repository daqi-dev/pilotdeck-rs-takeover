//! `cli.task.history` e2e: 历史搜索（search.*）、子代理目录与历史
//! （subagent.*）、末轮替换事务的管理侧（replacement.prepare/get/rollback，
//! 与已交付的 replacement.commit 组成完整事务）。
//!
//! 场景 -> 验收映射（design.json 关联用例）：
//! - `search_start_results_roundtrip`（cli.case.contract.capability.search /
//!   operation.search.start / operation.search.results / source-flow
//!   794f6a1051e9f0dc / e3cd3230dffa27d1 / f9d287c42db16878 的本任务切片）：
//!   中文+大小写混合命中、session/project/instance scope、角色过滤、
//!   regex、大小写敏感、分页不漏重、entryId 回查、范围外项目零命中、
//!   pageLimit 截断水位（indexingComplete=false + searchedThrough）、
//!   幂等（同 key 去重 / 异义冲突 / requests.lookup）、非法 regex 接纳前
//!   拒绝、无结果与失败区分（failed 操作的 results 报
//!   OPERATION_NOT_READY）。
//! - `subagent_catalog_and_history`（cli.case.contract.capability.subagents /
//!   operation.subagent.types / list / history）：服务端固定类型目录、
//!   未装配（无项目）与无历史区分、父 scope 授权（PARENT_MISMATCH /
//!   NOT_FOUND）、同名不同父 run 不串、tool 角色 runId/toolCallId 关联
//!   保留、空历史完整集合。
//! - `replacement_prepare_get_rollback`（cli.case.contract.capability.
//!   regenerate / operation.replacement.prepare / get / rollback /
//!   source-flow c97e61ff3ed7c68e）：prepare 不截断历史、CAS 与陈旧
//!   expectedTurnId 拒绝、并发占线 REPLACEMENT_BUSY、到期不改历史
//!   （phase=expired）、丢响应后 get 定位、回滚保留原历史、committed
//!   拒绝回滚、与 commit 的完整事务闭环。
//!
//! 本地未跑（2026-11-18 构建冻结令），待集成者 cb8 验证。

// The suite spawns real gateways through the CLI's embedded-gateway feature;
// without it there is nothing to test.
#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::Path;

use common_v1::*;
use serde_json::{Value, json};

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

/// Opens the instance's store in-process to seed history / subagents (the
/// documented联调点: no spawn operation exists in the 285-op contract, so
/// subagent rows and pre-integration history land through the store API the
/// future spawn path will call). Opened while no runs are active, so the
/// startup recovery pass of `open_and_recover` observes nothing to interrupt.
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

async fn session_revision(home: &Path, session_id: &str) -> String {
    let out = await_cli(
        home,
        &[
            "sessions",
            "get",
            "--output",
            "json",
            "--session-id",
            session_id,
        ],
    );
    let sess = expect_ok("session.get", &out);
    sess["session"]["revision"].as_str().unwrap().to_owned()
}

async fn search_start(
    home: &Path,
    scope: &str,
    query: &str,
    key: &str,
    extra: &[&str],
) -> CliOutcome {
    let mut args = vec![
        "search",
        "start",
        "--output",
        "json",
        "--scope",
        scope,
        "--query",
        query,
        "--request-key",
        key,
    ];
    args.extend_from_slice(extra);
    await_cli(home, &args)
}

async fn search_results(home: &Path, operation_id: &str, page: Option<&str>) -> CliOutcome {
    let mut args = vec![
        "search",
        "results",
        "--output",
        "json",
        "--operation-id",
        operation_id,
    ];
    if let Some(page) = page {
        args.extend(["--page", page]);
    }
    await_cli(home, &args)
}

/// 等待搜索操作终态（有界轮询 operations.get）。
async fn await_search_terminal(home: &Path, operation_id: &str) -> Value {
    for _ in 0..100 {
        let op = operations_get(home, operation_id).await;
        let state = op["operation"]["state"].as_str().unwrap_or_default();
        if matches!(
            state,
            "succeeded" | "failed" | "cancelled" | "interrupted" | "result_unknown"
        ) {
            return op["operation"].clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("search operation `{operation_id}` did not terminate in time");
}

async fn operations_get(home: &Path, operation_id: &str) -> Value {
    let out = await_cli(
        home,
        &[
            "operations",
            "get",
            "--output",
            "json",
            "--operation-id",
            operation_id,
        ],
    );
    expect_ok("operations.get", &out)
}

// ---------------------------------------------------------------------------
// Scenario 1: search
// ---------------------------------------------------------------------------

#[tokio::test]
async fn search_start_results_roundtrip() {
    let home = TestHome::new("hist-srch");
    let root = home.path();
    init_home(root);
    let (project_id, session_id) = seed_project_session(root, "srch").await;

    // 历史种子：中文 + 大小写混合 + 多角色。
    let store = seeding_store(root);
    store
        .append_history_entries(
            &session_id,
            &[
                json!({"turnId": "t-1", "role": "user", "content": [{"kind": "text", "text": "张三的 NEEdle 问题"}]}),
                json!({"turnId": "t-1", "role": "assistant", "content": [{"kind": "text", "text": "first answer"}]}),
                json!({"turnId": "t-2", "role": "user", "content": [{"kind": "text", "text": "second question about needle"}]}),
                json!({"turnId": "t-2", "role": "assistant", "content": [{"kind": "text", "text": "second answer"}]}),
            ],
        )
        .unwrap();
    drop(store);

    // --- 非法 regex：接纳前拒绝 INVALID_PATTERN ---
    let out = search_start(
        root,
        r#"{"kind":"instance"}"#,
        "[unclosed",
        "srch-bad-regex",
        &["--syntax", "regex"],
    )
    .await;
    assert_eq!(expect_fail("search.start", &out), "INVALID_PATTERN");

    // --- 空query：INVALID_ARGUMENT ---
    let out = search_start(root, r#"{"kind":"instance"}"#, "", "srch-empty", &[]).await;
    assert_eq!(expect_fail("search.start", &out), "INVALID_ARGUMENT");

    // --- 大小写不敏感 literal（session scope）：两条命中 ---
    let out = search_start(
        root,
        &format!(r#"{{"kind":"session","projectId":"{project_id}","sessionId":"{session_id}"}}"#),
        "needle",
        "srch-1",
        &[],
    )
    .await;
    let receipt = expect_ok("search.start", &out);
    assert_eq!(receipt["kind"], "operation");
    assert_eq!(receipt["durable"], true);
    let op_id = receipt["operationId"].as_str().unwrap().to_owned();
    let op = await_search_terminal(root, &op_id).await;
    assert_eq!(op["state"], "succeeded");
    assert_eq!(op["result"]["resultCount"], 2);
    assert_eq!(op["result"]["indexingComplete"], true);
    assert_eq!(op["result"]["searchId"], op_id.as_str());

    // 分页：一页一条，两页取完（无漏重）。
    let out = search_results(root, &op_id, Some(r#"{"limit":1}"#)).await;
    let page1 = expect_ok("search.results", &out);
    assert_eq!(page1["items"].as_array().unwrap().len(), 1);
    assert_eq!(page1["pageInfo"]["complete"], false);
    assert_eq!(page1["indexingComplete"], true);
    let cursor = page1["pageInfo"]["nextCursor"].as_str().unwrap().to_owned();
    let out = search_results(
        root,
        &op_id,
        Some(&format!(r#"{{"limit":10,"cursor":"{cursor}"}}"#)),
    )
    .await;
    let page2 = expect_ok("search.results", &out);
    assert_eq!(page2["items"].as_array().unwrap().len(), 1);
    assert_eq!(page2["pageInfo"]["complete"], true);

    // entryId 回查（结果稳定标识）。
    let hit = page2["items"][0].clone();
    let entry_id = hit["entryId"].as_str().unwrap().to_owned();
    let ids = history_turn_ids(root, &session_id).await;
    let history_out = await_cli(
        root,
        &[
            "history",
            "read",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--page",
            "{\"limit\":500}",
        ],
    );
    let page = expect_ok("history.read", &history_out);
    let entry_ids: Vec<&str> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["entryId"].as_str().unwrap())
        .collect();
    assert!(entry_ids.contains(&entry_id.as_str()), "entryId可回查");
    assert_eq!(ids.len(), 4);

    // --- 大小写敏感：仅 NEEdle 一条 ---
    let out = search_start(
        root,
        r#"{"kind":"instance"}"#,
        "NEEdle",
        "srch-2",
        &["--case-sensitive"],
    )
    .await;
    let receipt = expect_ok("search.start", &out);
    let op2 = await_search_terminal(root, receipt["operationId"].as_str().unwrap()).await;
    assert_eq!(op2["result"]["resultCount"], 1);

    // --- regex + 角色过滤（user 两条命中，assistant 不算）---
    let out = search_start(
        root,
        r#"{"kind":"instance"}"#,
        "n[eE]+dle",
        "srch-3",
        &["--syntax", "regex", "--roles", r#"["user"]"#],
    )
    .await;
    let receipt = expect_ok("search.start", &out);
    let op3_id = receipt["operationId"].as_str().unwrap().to_owned();
    let op3 = await_search_terminal(root, &op3_id).await;
    assert_eq!(op3["result"]["resultCount"], 2);
    let out = search_results(root, &op3_id, None).await;
    let page = expect_ok("search.results", &out);
    for item in page["items"].as_array().unwrap() {
        assert_eq!(item["role"], "user");
    }

    // --- 范围外项目：零命中（无结果≠失败）---
    let out = search_start(
        root,
        r#"{"kind":"project","projectId":"p-out-of-scope"}"#,
        "needle",
        "srch-4",
        &[],
    )
    .await;
    let receipt = expect_ok("search.start", &out);
    let op4_id = receipt["operationId"].as_str().unwrap().to_owned();
    let op4 = await_search_terminal(root, &op4_id).await;
    assert_eq!(op4["result"]["resultCount"], 0);
    let out = search_results(root, &op4_id, None).await;
    let page = expect_ok("search.results", &out);
    assert_eq!(page["items"].as_array().unwrap().len(), 0);
    assert_eq!(page["pageInfo"]["complete"], true);

    // --- pageLimit 截断：indexingComplete=false + 水位 ---
    let out = search_start(
        root,
        r#"{"kind":"instance"}"#,
        "answer",
        "srch-5",
        &["--page-limit", "1"],
    )
    .await;
    let receipt = expect_ok("search.start", &out);
    let op5_id = receipt["operationId"].as_str().unwrap().to_owned();
    let op5 = await_search_terminal(root, &op5_id).await;
    assert_eq!(op5["result"]["resultCount"], 1);
    assert_eq!(op5["result"]["indexingComplete"], false);
    assert!(op5["result"]["searchedThrough"].as_str().is_some());
    let out = search_results(root, &op5_id, None).await;
    let page = expect_ok("search.results", &out);
    assert_eq!(page["indexingComplete"], false);

    // --- 幂等：同 key 同输入去重；异义冲突；requests.lookup ---
    let out = search_start(root, r#"{"kind":"instance"}"#, "needle", "srch-6", &[]).await;
    let receipt = expect_ok("search.start", &out);
    let op6 = receipt["operationId"].as_str().unwrap().to_owned();
    let _ = await_search_terminal(root, &op6).await;
    let out = search_start(root, r#"{"kind":"instance"}"#, "needle", "srch-6", &[]).await;
    let replay = expect_ok("search.start", &out);
    assert_eq!(replay["deduplicated"], true);
    assert_eq!(replay["operationId"].as_str().unwrap(), op6);
    let out = search_start(root, r#"{"kind":"instance"}"#, "other", "srch-6", &[]).await;
    assert_eq!(expect_fail("search.start", &out), "IDEMPOTENCY_CONFLICT");
    let out = await_cli(
        root,
        &[
            "requests",
            "lookup",
            "--output",
            "json",
            "--method-id",
            "search.start",
            "--idempotency-key",
            "srch-6",
        ],
    );
    let lookup = expect_ok("requests.lookup", &out);
    assert_eq!(lookup["requestState"], "accepted");
    assert_eq!(lookup["resource"]["operationId"].as_str().unwrap(), op6);

    // --- 未知 operationId：NOT_FOUND；坏游标：INVALID_ARGUMENT ---
    let out = search_results(root, "op-missing", None).await;
    assert_eq!(expect_fail("search.results", &out), "NOT_FOUND");
    let out = search_results(root, &op6, Some(r#"{"cursor":"bogus"}"#)).await;
    assert_eq!(expect_fail("search.results", &out), "INVALID_ARGUMENT");
}

// ---------------------------------------------------------------------------
// Scenario 2: subagents
// ---------------------------------------------------------------------------

#[tokio::test]
async fn subagent_catalog_and_history() {
    let home = TestHome::new("hist-sub");
    let root = home.path();
    init_home(root);
    let (project_id, session_id) = seed_project_session(root, "sub").await;

    // 父归属对：一个已终态的 run。
    let receipt = submit_run(root, &session_id, "parent run", "sub-parent").await;
    let run_id = receipt["runId"].as_str().unwrap().to_owned();
    expect_ok("run.wait", &wait_run(root, &run_id));

    // 子代理种子（联调点：无 spawn 操作，经存储 API 种下未来 spawn 将调
    // 用的同一写入口）。
    let store = seeding_store(root);
    store
        .register_subagent("sub-e2e-1", &session_id, &run_id, "explore", "succeeded")
        .unwrap();
    store
        .append_subagent_entries(
            "sub-e2e-1",
            &[
                json!({"turnId": "t-1", "role": "user", "content": [{"kind": "text", "text": "scan the workspace"}]}),
                json!({"turnId": "t-1", "role": "assistant", "content": [{"kind": "text", "text": "found 3 files"}]}),
                // tool 角色与 runId/toolCallId 关联保留。
                json!({"turnId": "t-2", "role": "tool", "content": [{"kind": "text", "text": "grep ok"}], "runId": run_id, "toolCallId": "call-1"}),
            ],
        )
        .unwrap();
    drop(store);

    // --- types：服务端固定目录（注册项目）---
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
    let ids: Vec<&str> = types["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["typeId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["general-purpose", "explore", "plan", "verify"]);
    assert_eq!(types["pageInfo"]["complete"], true);

    // 未注册项目：空目录 + complete（未装配与无历史区分）。
    let out = await_cli(
        root,
        &[
            "subagent",
            "types",
            "--output",
            "json",
            "--project-id",
            "p-unregistered",
        ],
    );
    let types = expect_ok("subagent.types", &out);
    assert_eq!(types["items"].as_array().unwrap().len(), 0);
    assert_eq!(types["pageInfo"]["complete"], true);

    // --- list：父 scope 归属隔离 ---
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
            &run_id,
        ],
    );
    let list = expect_ok("subagent.list", &out);
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    assert_eq!(list["items"][0]["subagentId"], "sub-e2e-1");
    assert_eq!(list["items"][0]["typeId"], "explore");
    assert_eq!(list["items"][0]["state"], "succeeded");

    // 错误父会话：PARENT_MISMATCH。
    let out = await_cli(
        root,
        &[
            "subagent",
            "list",
            "--output",
            "json",
            "--parent-session-id",
            "s-other-session",
            "--parent-run-id",
            &run_id,
        ],
    );
    assert_eq!(expect_fail("subagent.list", &out), "PARENT_MISMATCH");

    // --- history：角色/tool 关联保留 + 越权拒绝 ---
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
            &run_id,
            "--subagent-id",
            "sub-e2e-1",
        ],
    );
    let history = expect_ok("subagent.history", &out);
    let entries = history["items"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[2]["role"], "tool");
    assert_eq!(entries[2]["toolCallId"], "call-1");
    assert_eq!(entries[2]["runId"], run_id.as_str());

    // 越权（错误的父 run）与不存在的子代理分别明确。
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
            "run-not-the-parent",
            "--subagent-id",
            "sub-e2e-1",
        ],
    );
    assert_eq!(expect_fail("subagent.history", &out), "PARENT_MISMATCH");
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
            &run_id,
            "--subagent-id",
            "sub-missing",
        ],
    );
    assert_eq!(expect_fail("subagent.history", &out), "NOT_FOUND");

    // 无历史：完整空集合（新登记的子代理）。
    let store = seeding_store(root);
    store
        .register_subagent("sub-e2e-2", &session_id, &run_id, "plan", "queued")
        .unwrap();
    drop(store);
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
            &run_id,
            "--subagent-id",
            "sub-e2e-2",
        ],
    );
    let history = expect_ok("subagent.history", &out);
    assert_eq!(history["items"].as_array().unwrap().len(), 0);
    assert_eq!(history["pageInfo"]["complete"], true);
}

// ---------------------------------------------------------------------------
// Scenario 3: replacement prepare/get/rollback (+ commit closure)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn replacement_prepare_get_rollback() {
    let home = TestHome::new("hist-repl");
    let root = home.path();
    init_home(root);
    let (_project_id, session_id) = seed_project_session(root, "repl").await;

    let store = seeding_store(root);
    store
        .append_history_entries(&session_id, &conversation())
        .unwrap();
    drop(store);
    assert_eq!(history_turn_ids(root, &session_id).await.len(), 4);

    // --- 陈旧 session revision：REVISION_CONFLICT，历史完整 ---
    let out = await_cli(
        root,
        &[
            "replacement",
            "prepare",
            "--output",
            "json",
            "--replacement-id",
            "rep-1",
            "--session-id",
            &session_id,
            "--expected-revision",
            "rev-bogus",
            "--expected-turn-id",
            "t-2",
        ],
    );
    assert_eq!(
        expect_fail("replacement.prepare", &out),
        "REVISION_CONFLICT"
    );
    assert_eq!(history_turn_ids(root, &session_id).await.len(), 4);

    // --- 陈旧 expectedTurnId：TURN_CONFLICT ---
    let rev = session_revision(root, &session_id).await;
    let out = await_cli(
        root,
        &[
            "replacement",
            "prepare",
            "--output",
            "json",
            "--replacement-id",
            "rep-1",
            "--session-id",
            &session_id,
            "--expected-revision",
            &rev,
            "--expected-turn-id",
            "t-1",
        ],
    );
    assert_eq!(expect_fail("replacement.prepare", &out), "TURN_CONFLICT");

    // --- prepare 成功：不截断可见历史 ---
    let out = await_cli(
        root,
        &[
            "replacement",
            "prepare",
            "--output",
            "json",
            "--replacement-id",
            "rep-1",
            "--session-id",
            &session_id,
            "--expected-revision",
            &rev,
            "--expected-turn-id",
            "t-2",
        ],
    );
    let prepared = expect_ok("replacement.prepare", &out);
    let replacement = &prepared["replacement"];
    assert_eq!(replacement["phase"], "prepared");
    assert_eq!(replacement["state"], "waiting");
    assert_eq!(replacement["replacementRunId"], Value::Null);
    let base = replacement["baseRevision"].as_str().unwrap().to_owned();
    assert_eq!(history_turn_ids(root, &session_id).await.len(), 4);

    // --- 并发占线：第二条 prepare → REPLACEMENT_BUSY ---
    let out = await_cli(
        root,
        &[
            "replacement",
            "prepare",
            "--output",
            "json",
            "--replacement-id",
            "rep-2",
            "--session-id",
            &session_id,
            "--expected-revision",
            &rev,
            "--expected-turn-id",
            "t-2",
        ],
    );
    assert_eq!(expect_fail("replacement.prepare", &out), "REPLACEMENT_BUSY");

    // --- get：与 prepare 一致（受理响应丢失后的原 ID 查询路径）---
    let out = await_cli(
        root,
        &[
            "replacement",
            "get",
            "--output",
            "json",
            "--replacement-id",
            "rep-1",
        ],
    );
    let got = expect_ok("replacement.get", &out);
    assert_eq!(got["replacement"]["phase"], "prepared");
    assert_eq!(got["replacement"]["baseRevision"], base.as_str());

    // --- rollback：CAS 通过 → rolledBack，历史原样 ---
    let out = await_cli(
        root,
        &[
            "replacement",
            "rollback",
            "--output",
            "json",
            "--replacement-id",
            "rep-1",
            "--expected-revision",
            &base,
        ],
    );
    let rolled = expect_ok("replacement.rollback", &out);
    assert_eq!(rolled["replacement"]["phase"], "rolledBack");
    assert_eq!(history_turn_ids(root, &session_id).await.len(), 4);
    // CAS 错位 → REVISION_CONFLICT。
    let rolled_rev = rolled["replacement"]["baseRevision"]
        .as_str()
        .unwrap()
        .to_owned();
    let out = await_cli(
        root,
        &[
            "replacement",
            "rollback",
            "--output",
            "json",
            "--replacement-id",
            "rep-1",
            "--expected-revision",
            "rev-wrong",
        ],
    );
    assert_eq!(
        expect_fail("replacement.rollback", &out),
        "REVISION_CONFLICT"
    );
    // 幂等：以当前 revision 重滚成功。
    let out = await_cli(
        root,
        &[
            "replacement",
            "rollback",
            "--output",
            "json",
            "--replacement-id",
            "rep-1",
            "--expected-revision",
            &rolled_rev,
        ],
    );
    let again = expect_ok("replacement.rollback", &out);
    assert_eq!(again["replacement"]["phase"], "rolledBack");

    // --- 到期不改历史：ttl=1ms 的 prepare 过期后 phase=expired ---
    let rev2 = session_revision(root, &session_id).await;
    let out = await_cli(
        root,
        &[
            "replacement",
            "prepare",
            "--output",
            "json",
            "--replacement-id",
            "rep-3",
            "--session-id",
            &session_id,
            "--expected-revision",
            &rev2,
            "--expected-turn-id",
            "t-2",
            "--ttl-ms",
            "1",
        ],
    );
    let prepared3 = expect_ok("replacement.prepare", &out);
    let base3 = prepared3["replacement"]["baseRevision"]
        .as_str()
        .unwrap()
        .to_owned();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let out = await_cli(
        root,
        &[
            "replacement",
            "get",
            "--output",
            "json",
            "--replacement-id",
            "rep-3",
        ],
    );
    let got3 = expect_ok("replacement.get", &out);
    assert_eq!(got3["replacement"]["phase"], "expired");
    assert_eq!(history_turn_ids(root, &session_id).await.len(), 4);
    // 过期后 commit：REPLACEMENT_EXPIRED（历史仍完整）。
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
            "[{\"kind\":\"text\",\"text\":\"edited\"}]",
            "--expected-revision",
            &base3,
            "--request-key",
            "repl-expired-commit",
        ],
    );
    assert_eq!(
        expect_fail("replacement.commit", &out),
        "REPLACEMENT_EXPIRED"
    );
    assert_eq!(history_turn_ids(root, &session_id).await.len(), 4);

    // --- 完整事务闭环：prepare → commit → get(committed + run) →
    //     rollback 拒绝（ALREADY_COMMITTED）。
    let rev3 = session_revision(root, &session_id).await;
    let out = await_cli(
        root,
        &[
            "replacement",
            "prepare",
            "--output",
            "json",
            "--replacement-id",
            "rep-4",
            "--session-id",
            &session_id,
            "--expected-revision",
            &rev3,
            "--expected-turn-id",
            "t-2",
        ],
    );
    let prepared4 = expect_ok("replacement.prepare", &out);
    let base4 = prepared4["replacement"]["baseRevision"]
        .as_str()
        .unwrap()
        .to_owned();
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
            "[{\"kind\":\"text\",\"text\":\"edited question\"}]",
            "--expected-revision",
            &base4,
            "--request-key",
            "repl-commit-1",
        ],
    );
    let receipt = expect_ok("replacement.commit", &out);
    let run_id = receipt["runId"].as_str().unwrap().to_owned();
    expect_ok("run.wait", &wait_run(root, &run_id));
    // 替换生效：只剩 t-1。
    assert_eq!(
        history_turn_ids(root, &session_id).await,
        vec!["t-1", "t-1"]
    );

    let out = await_cli(
        root,
        &[
            "replacement",
            "get",
            "--output",
            "json",
            "--replacement-id",
            "rep-4",
        ],
    );
    let got4 = expect_ok("replacement.get", &out);
    assert_eq!(got4["replacement"]["phase"], "committed");
    assert_eq!(
        got4["replacement"]["replacementRunId"].as_str().unwrap(),
        run_id
    );
    assert_eq!(got4["replacement"]["state"], "succeeded");

    // committed 拒绝回滚执行。
    let out = await_cli(
        root,
        &[
            "replacement",
            "rollback",
            "--output",
            "json",
            "--replacement-id",
            "rep-4",
            "--expected-revision",
            &base4,
        ],
    );
    assert_eq!(
        expect_fail("replacement.rollback", &out),
        "ALREADY_COMMITTED"
    );
    // 历史保持 commit 后形状。
    assert_eq!(
        history_turn_ids(root, &session_id).await,
        vec!["t-1", "t-1"]
    );

    // 未知 replacementId：NOT_FOUND。
    let out = await_cli(
        root,
        &[
            "replacement",
            "get",
            "--output",
            "json",
            "--replacement-id",
            "rep-missing",
        ],
    );
    assert_eq!(expect_fail("replacement.get", &out), "NOT_FOUND");
}
