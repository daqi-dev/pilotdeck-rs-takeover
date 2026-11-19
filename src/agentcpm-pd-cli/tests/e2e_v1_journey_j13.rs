//! S5 journey J13: 运行时与知识（K1）— runtimes / host / knowledge。
//!
//! 一条完整业务旅程驱动真实 `agentcpm-pd` 二进制（隔离 `AGENTCPM_PD_HOME`、
//! 内嵌 gateway 经真实 `/v1/ws` 传输、`--output json` envelope 逐步过
//! schema 校验），对应 `_tmp/20261119-s5/journeys-execution-plan.md` 的
//! J13 步骤序列：
//!
//! 1. `runtimes list/describe`（native 基线）→ `runtimes configure`
//!    （stdio 替身，absent 创建 + makeDefault）。
//! 2. `runtimes connect`（替身真实握手）→ 长操作经 `operations get` 至
//!    succeeded；`operations events` 逐页（`--page {"limit":1}` +
//!    `--after-cursor` 链式）读回该操作的事件流，断言事件类型恰为
//!    queued/started/completed 且游标序列严格单调（旅程的流读取判据）。
//! 3. `runtimes describe/list` 报告协商结果 → `host describe`（8 个公开
//!    能力逐项）→ `host prepare-model` / `host stream-model` 真实调用与
//!    describe 同口径：grant 签发机制无公开路径（S3 既定事实），一律
//!    `CAPABILITY_GRANT_REQUIRED`、非零退出，不伪装成功。
//! 4. `runtimes release`（stdio 只回收自己的子进程）。
//! 5. `knowledge inspect/configure/validate/query` + `knowledge citation
//!    resolve`（受管 stdio 模块 fixture，命中带引用、未知引用
//!    NOT_FOUND）。
//! 6. 故障负例：受管模块在收到 query 后死亡（应答前 EOF）→
//!    `knowledge query` 明确 `UPSTREAM_FAILED`（规范化失败不得装成空成
//!    功），且主绑定不受串扰。
//! 7. 落盘断言：`v1/state.sqlite` 存在；SIGKILL gateway 后经下一命令
//!    自动重启，runtime 配置（configured + 同 revision + 默认选择）与
//!    knowledge 绑定（ready + 同 revision）逐字段从盘上读回，重启后
//!    `knowledge query` 仍真实命中。
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

/// The v1 runtime protocol handshake line the stdio stub peer emits.
fn runtime_handshake_line() -> String {
    json!({
        "protocolVersion": "agentcpm-pd.runtime/1",
        "capabilityIds": ["j13.capability"],
        "operations": ["j13.operation"],
        "limits": {
            "maxConcurrentRuns": 2,
            "supportsCancel": true,
            "supportsResume": false
        }
    })
    .to_string()
}

/// The stdio runtime stub: handshake once, then stay alive until released.
fn stdio_configuration() -> Value {
    json!({
        "command": "sh",
        "args": [
            "-c",
            format!("printf '%s\\n' '{}'; sleep 30", runtime_handshake_line())
        ],
    })
}

fn runtimes_list(home: &Path) -> Value {
    let out = await_cli(
        home,
        &[
            "runtimes",
            "list",
            "--output",
            "json",
            "--scope",
            r#"{"kind":"global"}"#,
        ],
    );
    expect_ok("runtimes.list", &out)
}

fn runtimes_describe(home: &Path, runtime_id: &str) -> Value {
    let out = await_cli(
        home,
        &[
            "runtimes",
            "describe",
            "--output",
            "json",
            "--scope",
            r#"{"kind":"global"}"#,
            "--runtime-id",
            runtime_id,
        ],
    );
    expect_ok("runtimes.describe", &out)
}

fn find_runtime<'a>(listed: &'a Value, runtime_id: &str) -> &'a Value {
    listed["runtimes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["runtimeId"] == json!(runtime_id))
        .unwrap_or_else(|| panic!("runtime `{runtime_id}` missing from {listed}"))
}

fn wait_terminal_operation(home: &Path, operation_id: &str) -> Value {
    for _ in 0..200 {
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
        let result = expect_ok("operations.get", &out);
        if result["operation"]["state"].as_str().is_some_and(|s| {
            matches!(
                s,
                "succeeded" | "failed" | "cancelled" | "interrupted" | "result_unknown"
            )
        }) {
            return result;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("operation `{operation_id}` did not reach a terminal state");
}

/// Reads the operation's whole event journal page by page (`--page
/// {"limit":1}` forces pagination) and asserts the journey's stream-cursor
/// criterion: the event types arrive exactly in the lifecycle order and the
/// cursor sequence is strictly increasing across pages.
fn paged_operation_events(home: &Path, operation_id: &str) -> Vec<String> {
    let mut types = Vec::new();
    let mut cursors: Vec<String> = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..32 {
        let mut args = vec![
            "operations",
            "events",
            "--output",
            "json",
            "--operation-id",
            operation_id,
            "--page",
            "{\"limit\":1}",
        ];
        if let Some(cursor) = &after {
            args.push("--after-cursor");
            args.push(cursor.as_str());
        }
        let page = expect_ok("operations.events", &await_cli(home, &args));
        let frame = &page["frame"];
        for event in frame["events"].as_array().unwrap() {
            types.push(event["type"].as_str().unwrap().to_owned());
            let cursor = event["cursor"].as_str().unwrap().to_owned();
            if let Some(last) = cursors.last() {
                assert!(
                    cursor.trim_start_matches("cur-").parse::<u64>().unwrap()
                        > last.trim_start_matches("cur-").parse::<u64>().unwrap(),
                    "cursor sequence must be strictly increasing: {last} -> {cursor}"
                );
            }
            cursors.push(cursor);
        }
        if frame["terminal"] == true {
            assert_eq!(
                frame["caughtUp"], true,
                "a terminal frame must be caught up"
            );
            return types;
        }
        after = frame["nextCursor"].as_str().map(str::to_owned);
        assert!(
            after.is_some(),
            "a non-terminal page must carry the next cursor: {frame}"
        );
    }
    panic!("operations.events pagination did not converge for {operation_id}");
}

// ---------------------------------------------------------------------------
// The knowledge module fixtures (agentcpm-pd.knowledge/1, python3 NDJSON).
// ---------------------------------------------------------------------------

/// The well-behaved module: describe / query / resolve_citation.
const K1_MODULE: &str = r#"
import json, sys
def send(value):
    sys.stdout.write(json.dumps(value) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    req = json.loads(line)
    method = req.get("method")
    if method == "describe":
        send({"id": req["id"], "result": {
            "identity": {"moduleId": "__MODULE_ID__"},
            "protocolVersion": "agentcpm-pd.knowledge/1",
            "methods": __MODULE_METHODS__}})
    elif method == "query":
        send({"id": req["id"], "result": {
            "hits": [
                {"id": "j13-hit-1", "text": "The queue owns the run order.",
                 "citationIds": ["cite-j13-1"]},
                {"id": "j13-hit-2", "text": "Baselines do not guess availability.",
                 "citationIds": ["cite-j13-2"]},
            ],
            "nextCursor": None, "truncated": False,
            "providerMetadata": {"engine": "j13-e2e"}}})
    elif method == "resolve_citation":
        if req["params"]["citationId"] == "cite-j13-1":
            send({"id": req["id"], "result": {
                "source": {"documentId": "doc-j13", "title": "J13 Contract", "version": "13"},
                "excerpt": "The queue owns the run order.",
                "contentReference": "docstore://doc-j13"}})
        else:
            send({"id": req["id"], "error": {"code": "NOT_FOUND",
                "message": "unknown citation"}})
    else:
        send({"id": req["id"], "error": {"code": "UNSUPPORTED_METHOD",
            "message": "unknown method"}})
"#;

/// The doomed module: handshakes fine, then dies on its first query without
/// answering (the J13 fault injection — a managed module dying mid-call).
const K1_DOOMED_MODULE: &str = r#"
import json, sys
def send(value):
    sys.stdout.write(json.dumps(value) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    req = json.loads(line)
    method = req.get("method")
    if method == "describe":
        send({"id": req["id"], "result": {
            "identity": {"moduleId": "__MODULE_ID__"},
            "protocolVersion": "agentcpm-pd.knowledge/1",
            "methods": ["query"]}})
    elif method == "query":
        sys.exit(0)
    else:
        send({"id": req["id"], "error": {"code": "UNSUPPORTED_METHOD",
            "message": "unknown method"}})
"#;

fn python3_present() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Seeds one managed module under `<home>/knowledge-modules/`: the NDJSON
/// script plus the manifest the assembly scans.
fn seed_k1_module(
    home: &Path,
    reference: &str,
    script: &str,
    methods: &[&str],
    supported_options: &[&str],
) {
    let dir = home.join("knowledge-modules");
    std::fs::create_dir_all(&dir).unwrap();
    let script_path = dir.join(format!("{reference}.py"));
    std::fs::write(
        &script_path,
        script
            .replace("__MODULE_ID__", reference)
            .replace("__MODULE_METHODS__", &json!(methods).to_string()),
    )
    .unwrap();
    let manifest = json!({
        "identity": {"moduleId": reference},
        "transport": {
            "kind": "stdio",
            "command": "python3",
            "args": ["-I", script_path.to_string_lossy()],
        },
        "protocolVersion": "agentcpm-pd.knowledge/1",
        "declaredMethods": methods,
        "supportedOptions": supported_options,
        "credentialEnv": [],
    });
    std::fs::write(
        dir.join(format!("{reference}.json")),
        serde_json::to_string(&manifest).unwrap(),
    )
    .unwrap();
}

fn knowledge_inspect(home: &Path, binding_id: &str) -> Value {
    let out = await_cli(
        home,
        &[
            "knowledge",
            "inspect",
            "--output",
            "json",
            "--binding-id",
            binding_id,
        ],
    );
    expect_ok("knowledge.inspect", &out)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_host_and_knowledge_journey() {
    let home = TestHome::new("j13-runtime-knowledge");
    let root = home.path();
    init_home(root);

    // ------------------------------------------------------------------
    // 1. runtimes：native 基线 → 配置 stdio 替身（absent 创建 +
    //    makeDefault）。
    // ------------------------------------------------------------------
    let listed = runtimes_list(root);
    assert_eq!(listed["defaultRuntimeId"], "native");
    assert_eq!(listed["selectionSource"], "builtin-native");
    assert_eq!(listed["runtimes"].as_array().unwrap().len(), 1);
    let native = find_runtime(&listed, "native");
    assert_eq!(native["kind"], "native");
    assert_eq!(native["status"]["state"], "ready");
    assert_eq!(native["status"]["available"], true);
    assert_eq!(native["status"]["reasonCode"], Value::Null);

    let described = runtimes_describe(root, "native");
    assert_eq!(
        described["configurationSchemaId"],
        "agentcpm-pd.runtime.native/1"
    );
    assert!(
        described["supportedOperations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|op| op == "run.submit"),
        "native supportedOperations come from the actual registry"
    );
    assert_eq!(described["limits"]["supportsCancel"], true);

    let out = await_cli(
        root,
        &[
            "runtimes",
            "configure",
            "--output",
            "json",
            "--scope",
            r#"{"kind":"global"}"#,
            "--runtime-id",
            "rt-j13",
            "--kind",
            "stdio",
            "--provider-id",
            "j13-provider",
            "--schema-id",
            "agentcpm-pd.runtime.stdio/1",
            "--configuration",
            &stdio_configuration().to_string(),
            "--make-default",
            "true",
            "--expected-revision",
            "absent",
        ],
    );
    let configured = expect_ok("runtimes.configure", &out);
    assert_eq!(configured["applied"], true);

    let listed = runtimes_list(root);
    assert_eq!(listed["defaultRuntimeId"], "rt-j13");
    assert_eq!(listed["selectionSource"], "configured");
    let stub = find_runtime(&listed, "rt-j13");
    assert_eq!(stub["status"]["state"], "configured");
    assert_eq!(stub["status"]["available"], false);
    assert_eq!(stub["status"]["reasonCode"], "RUNTIME_NOT_CONNECTED");
    assert_eq!(stub["capabilityIds"], json!([]));
    assert!(
        !listed.to_string().contains("printf"),
        "the stdio peer command must not leak into the listing"
    );
    let runtime_revision = stub["revision"].as_str().unwrap().to_owned();

    // ------------------------------------------------------------------
    // 2. runtimes.connect：替身真实握手；operations.events 逐页读回
    //    事件流（类型序 + 游标单调）。
    // ------------------------------------------------------------------
    let out = await_cli(
        root,
        &[
            "runtimes",
            "connect",
            "--output",
            "json",
            "--scope",
            r#"{"kind":"global"}"#,
            "--runtime-id",
            "rt-j13",
            "--expected-revision",
            &runtime_revision,
            "--request-key",
            "j13-connect-1",
        ],
    );
    let receipt = expect_ok("runtimes.connect", &out);
    let connect_op = receipt["operationId"].as_str().unwrap().to_owned();
    let done = wait_terminal_operation(root, &connect_op);
    assert_eq!(done["operation"]["state"], "succeeded");
    let result = &done["operation"]["result"];
    assert_eq!(result["state"], "ready");
    assert_eq!(result["protocolVersion"], "agentcpm-pd.runtime/1");
    assert_eq!(result["capabilityIds"], json!(["j13.capability"]));
    let generation = result["generation"].as_str().unwrap().to_owned();

    let types = paged_operation_events(root, &connect_op);
    assert_eq!(
        types,
        vec![
            "operation.queued".to_owned(),
            "operation.started".to_owned(),
            "operation.completed".to_owned(),
        ],
        "the connect operation's event journal in lifecycle order"
    );

    // The negotiated facts are what describe/list now report.
    let described = runtimes_describe(root, "rt-j13");
    assert_eq!(described["supportedOperations"], json!(["j13.operation"]));
    assert_eq!(described["limits"]["maxConcurrentRuns"], 2);
    let listed = runtimes_list(root);
    let stub = find_runtime(&listed, "rt-j13");
    assert_eq!(stub["status"]["state"], "ready");
    assert_eq!(stub["status"]["available"], true);
    assert_eq!(stub["capabilityIds"], json!(["j13.capability"]));

    // ------------------------------------------------------------------
    // 3. host.describe 与真实调用逐项一致；grant 门保持关闭（签发机制
    //    无公开路径，S3 既定事实），不伪装成功。
    // ------------------------------------------------------------------
    let out = await_cli(
        root,
        &[
            "host",
            "describe",
            "--output",
            "json",
            "--scope",
            r#"{"kind":"global"}"#,
        ],
    );
    let described_host = expect_ok("host.describe", &out);
    let operations = described_host["operations"].as_array().unwrap().clone();
    assert_eq!(operations.len(), 8, "the public-host capability set");
    let entry_of = |id: &str| {
        operations
            .iter()
            .find(|e| e["operationId"] == json!(id))
            .unwrap_or_else(|| panic!("`{id}` missing from host.describe"))
            .clone()
    };
    assert_eq!(entry_of("model.list")["available"], true);
    assert_eq!(entry_of("host.describe")["available"], true);
    for gated in ["host.prepare_model", "host.stream_model"] {
        let entry = entry_of(gated);
        assert_eq!(entry["available"], false, "{gated}");
        assert_eq!(entry["reasonCode"], "CAPABILITY_GRANT_REQUIRED", "{gated}");
    }
    for gap in described_host["declaredGaps"].as_array().unwrap() {
        let gap_id = gap["operationId"].as_str().unwrap();
        assert!(
            entry_of(gap_id)["available"] != true,
            "declaredGaps entry `{gap_id}` must not be advertised available"
        );
    }

    // Real calls match describe: without a backend-issued grant the pair is
    // refused before any side effect.
    let prepare_input = json!({
        "scope": {"kind": "global"},
        "modelId": "prov-x/m-x",
        "schemaId": "agentcpm-pd.model.request/1",
        "request": {"messages": [{"role": "user", "content": "hello"}]},
        "executionContext": {
            "parentRunId": "run-j13-none",
            "parentInvocationId": "inv-j13-none",
            "consumerKind": "runtimeTool",
            "grantRef": "grant-j13-forged"
        },
        "budget": {"maxInputTokens": 100, "maxOutputTokens": 100, "timeoutMs": 1000}
    });
    let prepare_path = root.join("j13-prepare-input.json");
    std::fs::write(&prepare_path, prepare_input.to_string()).unwrap();
    let out = await_cli(
        root,
        &[
            "host",
            "prepare-model",
            "--output",
            "json",
            "--input-json",
            prepare_path.to_str().unwrap(),
            "--request-key",
            "j13-prepare-1",
        ],
    );
    assert_eq!(
        expect_fail("host.prepare_model", &out),
        "CAPABILITY_GRANT_REQUIRED"
    );

    let stream_input = json!({
        "scope": {"kind": "global"},
        "preparedRequestId": "hpr-j13-none",
        "requestHash": "deadbeef",
        "executionContext": {
            "parentRunId": "run-j13-none",
            "parentInvocationId": "inv-j13-none",
            "consumerKind": "runtimeTool",
            "grantRef": "grant-j13-forged"
        }
    });
    let stream_path = root.join("j13-stream-input.json");
    std::fs::write(&stream_path, stream_input.to_string()).unwrap();
    let out = await_cli(
        root,
        &[
            "host",
            "stream-model",
            "--output",
            "json",
            "--input-json",
            stream_path.to_str().unwrap(),
            "--request-key",
            "j13-stream-1",
        ],
    );
    assert_eq!(
        expect_fail("host.stream_model", &out),
        "CAPABILITY_GRANT_REQUIRED"
    );

    // ------------------------------------------------------------------
    // 4. runtimes.release：stdio 只回收自己的子进程。
    // ------------------------------------------------------------------
    let out = await_cli(
        root,
        &[
            "runtimes",
            "release",
            "--output",
            "json",
            "--scope",
            r#"{"kind":"global"}"#,
            "--runtime-id",
            "rt-j13",
            "--expected-generation",
            &generation,
            "--request-key",
            "j13-release-1",
        ],
    );
    let release_receipt = expect_ok("runtimes.release", &out);
    let released = wait_terminal_operation(root, release_receipt["operationId"].as_str().unwrap());
    let release_result = &released["operation"]["result"];
    assert_eq!(release_result["released"], true);
    assert_eq!(release_result["retainedResources"], json!([]));
    let listed = runtimes_list(root);
    assert_eq!(
        find_runtime(&listed, "rt-j13")["status"]["state"],
        "configured"
    );

    // ------------------------------------------------------------------
    // 5. knowledge：未装配负例 → 受管模块装配 → validate → query →
    //    citation resolve。
    // ------------------------------------------------------------------
    let out = await_cli(
        root,
        &[
            "knowledge",
            "configure",
            "--output",
            "json",
            "--binding-id",
            "kb-missing",
            "--expected-revision",
            "absent",
            "--enabled",
            "true",
            "--module-reference",
            "module-that-was-never-there",
            "--allowed-methods",
            r#"["query"]"#,
        ],
    );
    assert_eq!(
        expect_fail("knowledge.configure", &out),
        "UNKNOWN_MODULE_REFERENCE"
    );

    seed_k1_module(
        root,
        "kb-j13",
        K1_MODULE,
        &["query", "resolve_citation"],
        &["knowledgeBaseIds", "limit", "cursor"],
    );
    let out = await_cli(
        root,
        &[
            "knowledge",
            "configure",
            "--output",
            "json",
            "--binding-id",
            "kb-main",
            "--expected-revision",
            "absent",
            "--enabled",
            "true",
            "--module-reference",
            "kb-j13",
            "--allowed-methods",
            r#"["query","resolve_citation"]"#,
        ],
    );
    let configured = expect_ok("knowledge.configure", &out);
    assert_eq!(configured["persisted"], true);
    assert_eq!(configured["runtimeState"], "pending_validation");

    let inspected = knowledge_inspect(root, "kb-main");
    assert_eq!(inspected["state"], "configured");
    assert_eq!(inspected["unavailableReason"], "NOT_VALIDATED");
    assert_eq!(
        inspected["declaredMethods"],
        json!(["query", "resolve_citation"])
    );
    assert_eq!(inspected["effectiveMethods"], json!([]));
    let binding_revision = inspected["revision"].as_str().unwrap().to_owned();

    if !python3_present() {
        // The no-module rows above are already asserted; the module-process
        // rows need the fixture runtime.
        eprintln!("note: python3 absent; skipping the k1 module-process rows");
        return;
    }

    let out = await_cli(
        root,
        &[
            "knowledge",
            "validate",
            "--output",
            "json",
            "--binding-id",
            "kb-main",
            "--expected-revision",
            &binding_revision,
        ],
    );
    let validated = expect_ok("knowledge.validate", &out);
    assert_eq!(validated["validated"], true);
    assert_eq!(
        validated["effectiveMethods"],
        json!(["query", "resolve_citation"])
    );
    for check in validated["checks"].as_array().unwrap() {
        assert_eq!(check["status"], "pass", "check {check}");
    }

    let inspected = knowledge_inspect(root, "kb-main");
    assert_eq!(inspected["state"], "ready");
    assert_eq!(inspected["unavailableReason"], Value::Null);

    let out = await_cli(
        root,
        &[
            "knowledge",
            "query",
            "--output",
            "json",
            "--binding-id",
            "kb-main",
            "--query",
            "queue run order",
            "--limit",
            "10",
        ],
    );
    let queried = expect_ok("knowledge.query", &out);
    assert_eq!(queried["bindingRevision"], json!(binding_revision));
    assert_eq!(queried["hits"].as_array().unwrap().len(), 2);
    assert_eq!(queried["hits"][0]["citationIds"], json!(["cite-j13-1"]));
    assert_eq!(queried["truncated"], false);
    assert_eq!(queried["providerMetadata"]["engine"], "j13-e2e");

    let out = await_cli(
        root,
        &[
            "knowledge",
            "citation",
            "resolve",
            "--output",
            "json",
            "--binding-id",
            "kb-main",
            "--citation-id",
            "cite-j13-1",
        ],
    );
    let resolved = expect_ok("knowledge.citation.resolve", &out);
    assert_eq!(resolved["source"]["documentId"], "doc-j13");
    assert_eq!(resolved["source"]["version"], "13");
    assert_eq!(resolved["contentReference"], "docstore://doc-j13");
    let out = await_cli(
        root,
        &[
            "knowledge",
            "citation",
            "resolve",
            "--output",
            "json",
            "--binding-id",
            "kb-main",
            "--citation-id",
            "cite-j13-unknown",
        ],
    );
    assert_eq!(expect_fail("knowledge.citation.resolve", &out), "NOT_FOUND");

    // ------------------------------------------------------------------
    // 6. 故障负例：受管模块在收到 query 后死亡（应答前 EOF）→
    //    UPSTREAM_FAILED，绝不装成空成功；主绑定不受串扰。
    // ------------------------------------------------------------------
    seed_k1_module(root, "kb-j13-doomed", K1_DOOMED_MODULE, &["query"], &[]);
    let out = await_cli(
        root,
        &[
            "knowledge",
            "configure",
            "--output",
            "json",
            "--binding-id",
            "kb-doomed",
            "--expected-revision",
            "absent",
            "--enabled",
            "true",
            "--module-reference",
            "kb-j13-doomed",
            "--allowed-methods",
            r#"["query"]"#,
        ],
    );
    let doomed_binding = expect_ok("knowledge.configure", &out);
    let out = await_cli(
        root,
        &[
            "knowledge",
            "validate",
            "--output",
            "json",
            "--binding-id",
            "kb-doomed",
            "--expected-revision",
            doomed_binding["revision"].as_str().unwrap(),
        ],
    );
    expect_ok("knowledge.validate", &out);

    let out = await_cli(
        root,
        &[
            "knowledge",
            "query",
            "--output",
            "json",
            "--binding-id",
            "kb-doomed",
            "--query",
            "anything",
        ],
    );
    assert_eq!(expect_fail("knowledge.query", &out), "UPSTREAM_FAILED");

    let out = await_cli(
        root,
        &[
            "knowledge",
            "query",
            "--output",
            "json",
            "--binding-id",
            "kb-main",
            "--query",
            "queue run order",
        ],
    );
    let still = expect_ok("knowledge.query", &out);
    assert_eq!(
        still["hits"].as_array().unwrap().len(),
        2,
        "主绑定不受故障串扰: {still}"
    );

    // ------------------------------------------------------------------
    // 7. 落盘断言：SIGKILL gateway → 自动重启 → runtime 配置与 knowledge
    //    绑定逐字段从盘上读回，重启后 query 仍真实命中。
    // ------------------------------------------------------------------
    assert!(
        root.join("v1").join("state.sqlite").is_file(),
        "the gateway state store must exist on disk"
    );
    let pre_listed = runtimes_list(root);
    let pre_stub = find_runtime(&pre_listed, "rt-j13");
    let pre_runtime_revision = pre_stub["revision"].as_str().unwrap().to_owned();
    let pre_inspected = knowledge_inspect(root, "kb-main");
    let pre_binding_revision = pre_inspected["revision"].as_str().unwrap().to_owned();
    assert_eq!(pre_inspected["state"], "ready");

    let pid = home.gateway_pid().expect("gateway pid");
    signal_pid(pid, rustix::process::Signal::KILL);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let post_listed = runtimes_list(root);
    assert_eq!(post_listed["defaultRuntimeId"], "rt-j13", "默认选择落盘");
    assert_eq!(post_listed["selectionSource"], "configured");
    let post_stub = find_runtime(&post_listed, "rt-j13");
    assert_eq!(post_stub["status"]["state"], "configured");
    assert_eq!(
        post_stub["revision"].as_str().unwrap(),
        pre_runtime_revision,
        "runtime 配置修订跨进程死亡不变"
    );
    let post_inspected = knowledge_inspect(root, "kb-main");
    assert_eq!(post_inspected["state"], "ready");
    assert_eq!(
        post_inspected["revision"].as_str().unwrap(),
        pre_binding_revision,
        "knowledge 绑定修订跨进程死亡不变"
    );

    let out = await_cli(
        root,
        &[
            "knowledge",
            "query",
            "--output",
            "json",
            "--binding-id",
            "kb-main",
            "--query",
            "queue run order",
        ],
    );
    let after_restart = expect_ok("knowledge.query", &out);
    assert_eq!(
        after_restart["hits"].as_array().unwrap().len(),
        2,
        "重启后 query 仍真实命中"
    );
}
