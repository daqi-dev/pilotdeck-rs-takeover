//! `cli.task.runtime` end-to-end acceptance (runtimes.* / host.* /
//! knowledge.*, `007 §10`).
//!
//! Every scenario drives the real `agentcpm-pd` binary against an isolated
//! temporary `AGENTCPM_PD_HOME` with the embedded gateway answering over the
//! real `/v1/ws` transport. The external peers are deterministic fixtures
//! under the test home — a stdio runtime that handshakes with the v1 runtime
//! protocol (`agentcpm-pd.runtime/1`, one `printf` line) and a knowledge
//! module speaking `agentcpm-pd.knowledge/1` (a python3 NDJSON script);
//! no external service is ever contacted.
//!
//! Scripted cases (the task's 关联用例):
//!
//! * `cli.case.operation.runtimes.list` / `.describe` /
//!   `cli.case.contract.capability.runtime-modules`: native 默认与选择来
//!   源清晰、configured/disabled/ready/failed 逐项区分、能力来自实际协
//!   商、端点命令不出现在普通输出。
//! * `cli.case.operation.runtimes.configure`: native/stdio/TCP 各自校验、
//!   CAS（absent 创建 / revision 匹配）、makeDefault 选择翻转、
//!   runtime.configurationChanged 事件可查。
//! * `cli.case.operation.runtimes.connect`: stdio 受管子进程握手成功才
//!   ready、协议不匹配明确 PROTOCOL_UNSUPPORTED、重复请求（同 key 与新
//!   key）不启动第二个子进程、连接后 describe 报告实际协商结果。
//! * `cli.case.operation.runtimes.release`: 有活动 run 默认 busy、
//!   cancelActive 逐个终态、stdio 只回收自己的子进程、native 报告
//!   retainedResources。
//! * `cli.case.operation.host.describe` /
//!   `cli.case.contract.capability.public-host-capabilities` /
//!   `cli.case.packages.host-grant`: describe 与真实调用逐项一致（
//!   model.list 可调用→available；无 grant 调 prepare/stream→
//!   CAPABILITY_GRANT_REQUIRED，describe 同口径）；grant 门在 e2e 保持关
//!   闭（签发机制无公开路径，单测覆盖直种 grant 的验证路径）。
//! * `cli.case.packages.knowledge-config` + `cli.case.operation.knowledge.*`
//!   （K1 验收矩阵）: 未装配/未配置时 inspect/configure 正常、query 明确
//!   不可用；disabled 显式 DISABLED；validate 后 ready；query 命中带引用、
//!   citation 解析可溯源、未知引用 NOT_FOUND；越权（方法不在允许集）
//!   CAPABILITY_UNAVAILABLE；未声明选项 UNSUPPORTED_QUERY_OPTION。
//!
//! 合同行为断言中的本地执行状态：e2e 全量在 cb8/mbp2 跑（本机构建冻
//! 结）；文件按 007 §10 挂具写成即可运行形态。

#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use common_v1::*;

/// The v1 runtime protocol handshake line a stdio peer emits.
fn runtime_handshake_line() -> String {
    json!({
        "protocolVersion": "agentcpm-pd.runtime/1",
        "capabilityIds": ["e2e.capability"],
        "operations": ["e2e.operation"],
        "limits": {
            "maxConcurrentRuns": 3,
            "supportsCancel": true,
            "supportsResume": false
        }
    })
    .to_string()
}

/// Writes a stdio runtime manifest-pair? No — a runtime is configured through
/// the public operation; this only returns the configuration object.
fn stdio_configuration() -> Value {
    json!({
        "command": "sh",
        "args": [
            "-c",
            format!("printf '%s\\n' '{}'; sleep 10", runtime_handshake_line())
        ],
    })
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

/// 失败断言：非零退出码 + `outcome=failed` + `error.code`。
fn expect_failure(method: &str, out: &CliOutcome) -> String {
    assert_ne!(out.code, 0, "`{method}` must not exit 0: {}", out.stderr);
    let lines: Vec<&str> = out
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 1, "`{method}` stdout: {}", out.stdout);
    let value: Value = serde_json::from_str(lines[0])
        .unwrap_or_else(|e| panic!("`{method}` stdout not JSON: {e}"));
    assert_eq!(value["outcome"], "failed", "`{method}`: {value}");
    value["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
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

fn runtimes_configure(
    home: &Path,
    runtime_id: &str,
    configuration: &Value,
    expected: &str,
) -> Value {
    let out = await_cli(
        home,
        &[
            "runtimes",
            "configure",
            "--output",
            "json",
            "--scope",
            r#"{"kind":"global"}"#,
            "--runtime-id",
            runtime_id,
            "--kind",
            "stdio",
            "--provider-id",
            "e2e-provider",
            "--schema-id",
            "agentcpm-pd.runtime.stdio/1",
            "--configuration",
            &configuration.to_string(),
            "--make-default",
            "true",
            "--expected-revision",
            expected,
        ],
    );
    expect_ok("runtimes.configure", &out)
}

fn find_runtime<'a>(listed: &'a Value, runtime_id: &str) -> &'a Value {
    listed["runtimes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["runtimeId"] == json!(runtime_id))
        .unwrap_or_else(|| panic!("runtime `{runtime_id}` missing from {listed}"))
}

// ---------------------------------------------------------------------------
// Scenario 1: runtimes.* — states, selection source, handshake, release
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtimes_lifecycle_states_handshake_and_release() {
    let home = TestHome::new("runtime-e2e-lifecycle");
    let root = home.path();
    init_home(root);

    // Native only: builtin default, ready, and no process detail in output.
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

    // Schema validation: the wrong schema id for the kind is rejected.
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
            "rt-e2e",
            "--kind",
            "stdio",
            "--provider-id",
            "e2e-provider",
            "--schema-id",
            "agentcpm-pd.runtime.tcp/1",
            "--configuration",
            &stdio_configuration().to_string(),
            "--expected-revision",
            "absent",
        ],
    );
    assert_eq!(
        expect_failure("runtimes.configure", &out),
        "SCHEMA_MISMATCH"
    );

    // Create + makeDefault: the configured (not assembled) state and the
    // explicit selection source are both reported, and the peer command
    // never leaks into the listing.
    let configured = runtimes_configure(root, "rt-e2e", &stdio_configuration(), "absent");
    assert_eq!(configured["applied"], true);
    assert_eq!(configured["generation"], Value::Null);

    let listed = runtimes_list(root);
    assert_eq!(listed["defaultRuntimeId"], "rt-e2e");
    assert_eq!(listed["selectionSource"], "configured");
    let custom = find_runtime(&listed, "rt-e2e");
    assert_eq!(custom["status"]["state"], "configured");
    assert_eq!(custom["status"]["available"], false);
    assert_eq!(custom["status"]["reasonCode"], "RUNTIME_NOT_CONNECTED");
    assert_eq!(custom["capabilityIds"], json!([]));
    let serialized = listed.to_string();
    assert!(
        !serialized.contains("printf"),
        "the stdio peer command must not leak into the listing: {serialized}"
    );

    // CAS: `absent` over an existing runtime and a stale revision both fail.
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
            "rt-e2e",
            "--kind",
            "stdio",
            "--provider-id",
            "e2e-provider",
            "--schema-id",
            "agentcpm-pd.runtime.stdio/1",
            "--configuration",
            &stdio_configuration().to_string(),
            "--expected-revision",
            "absent",
        ],
    );
    assert_eq!(
        expect_failure("runtimes.configure", &out),
        "REVISION_CONFLICT"
    );

    // Connect: handshake succeeds → the operation completes with the
    // negotiated facts.
    let revision = custom["revision"].as_str().unwrap().to_owned();
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
            "rt-e2e",
            "--expected-revision",
            &revision,
            "--request-key",
            "e2e-connect-1",
        ],
    );
    let receipt = expect_ok("runtimes.connect", &out);
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
    let done = wait_terminal_operation(root, &operation_id);
    assert_eq!(done["operation"]["state"], "succeeded");
    let result = &done["operation"]["result"];
    assert_eq!(result["state"], "ready");
    assert_eq!(result["protocolVersion"], "agentcpm-pd.runtime/1");
    assert_eq!(result["capabilityIds"], json!(["e2e.capability"]));
    let generation = result["generation"].as_str().unwrap().to_owned();

    // The same key replays the receipt (no second child).
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
            "rt-e2e",
            "--expected-revision",
            &revision,
            "--request-key",
            "e2e-connect-1",
        ],
    );
    let replay = expect_ok("runtimes.connect", &out);
    assert_eq!(replay["deduplicated"], true);

    // A different key on the assembled runtime: zero-spawn success with the
    // same live generation.
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
            "rt-e2e",
            "--expected-revision",
            &revision,
            "--request-key",
            "e2e-connect-2",
        ],
    );
    let second = expect_ok("runtimes.connect", &out);
    let second_op = second["operationId"].as_str().unwrap().to_owned();
    let second_done = wait_terminal_operation(root, &second_op);
    assert_eq!(
        second_done["operation"]["result"]["generation"],
        json!(generation)
    );

    // describe now reports the negotiated operations and limits.
    let described = runtimes_describe(root, "rt-e2e");
    assert_eq!(described["supportedOperations"], json!(["e2e.operation"]));
    assert_eq!(described["limits"]["maxConcurrentRuns"], 3);
    let listed = runtimes_list(root);
    let custom = find_runtime(&listed, "rt-e2e");
    assert_eq!(custom["status"]["state"], "ready");
    assert_eq!(custom["status"]["available"], true);
    assert_eq!(custom["capabilityIds"], json!(["e2e.capability"]));

    // Release with the live generation: the owned child is drained.
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
            "rt-e2e",
            "--expected-generation",
            &generation,
            "--request-key",
            "e2e-release-1",
        ],
    );
    let release_receipt = expect_ok("runtimes.release", &out);
    let release_op = release_receipt["operationId"].as_str().unwrap().to_owned();
    let released = wait_terminal_operation(root, &release_op);
    let result = &released["operation"]["result"];
    assert_eq!(result["released"], true);
    assert_eq!(result["retainedResources"], json!([]));
    let listed = runtimes_list(root);
    assert_eq!(
        find_runtime(&listed, "rt-e2e")["status"]["state"],
        "configured"
    );

    // A protocol-mismatching peer fails honestly (no fake ready).
    let bad_handshake = r#"{"protocolVersion":"vendor/9"}"#;
    let bad_config = json!({
        "command": "sh",
        "args": ["-c", format!("printf '%s\\n' '{bad_handshake}'; sleep 5")],
    });
    runtimes_configure(root, "rt-bad", &bad_config, "absent");
    let out = await_cli(
        root,
        &[
            "runtimes",
            "list",
            "--output",
            "json",
            "--scope",
            r#"{"kind":"global"}"#,
        ],
    );
    let listed = expect_ok("runtimes.list", &out);
    let revision = find_runtime(&listed, "rt-bad")["revision"]
        .as_str()
        .unwrap()
        .to_owned();
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
            "rt-bad",
            "--expected-revision",
            &revision,
            "--request-key",
            "e2e-connect-bad",
        ],
    );
    let receipt = expect_ok("runtimes.connect", &out);
    let done = wait_terminal_operation(root, receipt["operationId"].as_str().unwrap());
    assert_eq!(done["operation"]["state"], "failed");
    assert_eq!(done["operation"]["error"]["code"], "PROTOCOL_UNSUPPORTED");
    let listed = runtimes_list(root);
    let failed = find_runtime(&listed, "rt-bad");
    assert_eq!(failed["status"]["state"], "failed");
    assert_eq!(failed["status"]["reasonCode"], "PROTOCOL_UNSUPPORTED");
}

// ---------------------------------------------------------------------------
// Scenario 2: runtimes.release busy/cancelActive on the native runtime
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtimes_release_native_busy_and_cancel_active() {
    let home = TestHome::new("runtime-e2e-release");
    let root = home.path();
    let init = init_home(root);
    let iid = init["instanceId"].as_str().unwrap().to_owned();

    let out = await_cli(
        root,
        &[
            "projects",
            "register",
            "--output",
            "json",
            "--project-id",
            "proj-rt",
            "--name",
            "Rt",
            "--environment-id",
            "local",
            "--root",
            &format!(r#"{{"instanceId":"{iid}","rootId":"r1","relativePath":"w"}}"#),
        ],
    );
    expect_ok("project.register", &out);
    let out = await_cli(
        root,
        &[
            "sessions",
            "create",
            "--output",
            "json",
            "--session-id",
            "sess-rt",
            "--project-id",
            "proj-rt",
        ],
    );
    expect_ok("session.create", &out);

    let out = await_cli(
        root,
        &[
            "run",
            "submit",
            "--output",
            "json",
            "--session-id",
            "sess-rt",
            "--text",
            "[pd-driver:sleep=60000]",
            "--request-key",
            "e2e-rt-run-1",
        ],
    );
    let submitted = expect_ok("run.submit", &out);
    let run_id = submitted["runId"].as_str().unwrap().to_owned();

    // The native runtime's generation is the instance generation.
    let listed = runtimes_list(root);
    let native = find_runtime(&listed, "native");
    let generation = native["generation"].as_str().unwrap().to_owned();

    // Default release: busy.
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
            "native",
            "--expected-generation",
            &generation,
            "--request-key",
            "e2e-rel-busy",
        ],
    );
    assert_eq!(expect_failure("runtimes.release", &out), "RESOURCE_BUSY");

    // cancelActive: the run terminalizes cancelled, each individually
    // queryable; the native runtime is reported retained.
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
            "native",
            "--expected-generation",
            &generation,
            "--cancel-active",
            "true",
            "--request-key",
            "e2e-rel-cancel",
        ],
    );
    let receipt = expect_ok("runtimes.release", &out);
    let done = wait_terminal_operation(root, receipt["operationId"].as_str().unwrap());
    let result = &done["operation"]["result"];
    assert_eq!(result["cancelledRunIds"], json!([run_id]));
    assert_eq!(result["retainedResources"], json!(["native"]));
    let out = await_cli(
        root,
        &["runs", "get", "--output", "json", "--run-id", &run_id],
    );
    let run = expect_ok("run.get", &out);
    assert_eq!(run["run"]["state"], "cancelled");
}

// ---------------------------------------------------------------------------
// Scenario 3: host.describe — the projection matches real callability
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_describe_matches_real_callability_and_grants_stay_closed() {
    let home = TestHome::new("runtime-e2e-host");
    let root = home.path();
    init_home(root);

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
    let described = expect_ok("host.describe", &out);
    let operations: Vec<(String, bool, Value)> = described["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["operationId"].as_str().unwrap().to_owned(),
                entry["available"].as_bool().unwrap(),
                entry["reasonCode"].clone(),
            )
        })
        .collect();
    assert_eq!(operations.len(), 8, "the public-host capability set");

    let op_available = |id: &str| {
        operations
            .iter()
            .find(|(op, _, _)| op == id)
            .unwrap_or_else(|| panic!("`{id}` missing from host.describe"))
            .1
    };

    // describe 与真实调用逐项一致: model.list is callable → available.
    assert!(op_available("model.list"));
    assert!(op_available("host.describe"));
    let out = await_cli(root, &["models", "list", "--output", "json"]);
    expect_ok("model.list", &out);

    // The grant-gated pair: implemented but closed. A real call without a
    // grant answers CAPABILITY_GRANT_REQUIRED, exactly as describe reports.
    for op in ["host.prepare_model", "host.stream_model"] {
        let (_, available, reason) = operations.iter().find(|(entry, _, _)| entry == op).unwrap();
        assert_eq!(*available, false, "{op}");
        assert_eq!(reason, json!("CAPABILITY_GRANT_REQUIRED"), "{op}");
    }
    let prepare_input = json!({
        "scope": {"kind": "global"},
        "modelId": "prov-x/m-x",
        "schemaId": "agentcpm-pd.model.request/1",
        "request": {"messages": [{"role": "user", "content": "hi"}]},
        "executionContext": {
            "parentRunId": "run-none",
            "parentInvocationId": "inv-none",
            "consumerKind": "runtimeTool",
            "grantRef": "grant-forged"
        },
        "budget": {"maxInputTokens": 100, "maxOutputTokens": 100, "timeoutMs": 1000}
    });
    let input_path = root.join("e2e-prepare-input.json");
    std::fs::write(&input_path, prepare_input.to_string()).unwrap();
    let out = await_cli(
        root,
        &[
            "host",
            "prepare-model",
            "--output",
            "json",
            "--input-json",
            input_path.to_str().unwrap(),
            "--request-key",
            "e2e-prepare-1",
        ],
    );
    assert_eq!(
        expect_failure("host.prepare_model", &out),
        "CAPABILITY_GRANT_REQUIRED"
    );

    let stream_input = json!({
        "scope": {"kind": "global"},
        "preparedRequestId": "hpr-none",
        "requestHash": "deadbeef",
        "executionContext": {
            "parentRunId": "run-none",
            "parentInvocationId": "inv-none",
            "consumerKind": "runtimeTool",
            "grantRef": "grant-forged"
        }
    });
    let input_path = root.join("e2e-stream-input.json");
    std::fs::write(&input_path, stream_input.to_string()).unwrap();
    let out = await_cli(
        root,
        &[
            "host",
            "stream-model",
            "--output",
            "json",
            "--input-json",
            input_path.to_str().unwrap(),
            "--request-key",
            "e2e-stream-1",
        ],
    );
    assert_eq!(
        expect_failure("host.stream_model", &out),
        "CAPABILITY_GRANT_REQUIRED"
    );

    // The skills pair is dynamically consistent with its real callability
    // (the extensions domain may or may not be in this build).
    for op in ["skills.list", "skills.apply_import"] {
        let available = op_available(op);
        let out = await_cli(
            root,
            &[
                "skills",
                "list",
                "--output",
                "json",
                "--scope",
                r#"{"kind":"global"}"#,
                "--query",
                "z",
            ],
        );
        let lines: Vec<&str> = out
            .stdout
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();
        let callable = out.code == 0
            && lines.len() == 1
            && serde_json::from_str::<Value>(lines[0])
                .ok()
                .and_then(|v| v["outcome"].as_str().map(str::to_owned))
                .is_some_and(|outcome| outcome == "succeeded" || outcome == "accepted");
        assert_eq!(
            available, callable,
            "host.describe availability for `{op}` must match its real callability"
        );
    }

    // declaredGaps never contains an entry the build actually serves.
    for gap in described["declaredGaps"].as_array().unwrap() {
        let gap_id = gap["operationId"].as_str().unwrap();
        assert!(
            !op_available(gap_id),
            "declaredGaps entry `{gap_id}` must not be advertised available"
        );
    }
}

// ---------------------------------------------------------------------------
// Scenario 4: knowledge.* — the K1 acceptance matrix
// ---------------------------------------------------------------------------

/// The deterministic knowledge module fixture (agentcpm-pd.knowledge/1).
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
                {"id": "hit-1", "text": "The public contract is the behavior truth.",
                 "citationIds": ["cite-1"]},
                {"id": "hit-2", "text": "Baselines do not guess availability.",
                 "citationIds": ["cite-2"]},
            ],
            "nextCursor": None, "truncated": False,
            "providerMetadata": {"engine": "e2e"}}})
    elif method == "resolve_citation":
        if req["params"]["citationId"] == "cite-1":
            send({"id": req["id"], "result": {
                "source": {"documentId": "doc-e2e", "title": "Contract", "version": "7"},
                "excerpt": "The contract is the behavior truth.",
                "contentReference": "docstore://doc-e2e"}})
        else:
            send({"id": req["id"], "error": {"code": "NOT_FOUND",
                "message": "unknown citation"}})
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

fn seed_k1_module(home: &Path, reference: &str, methods: &[&str], supported_options: &[&str]) {
    let dir = home.join("knowledge-modules");
    std::fs::create_dir_all(&dir).unwrap();
    let script_path = dir.join(format!("{reference}.py"));
    std::fs::write(
        &script_path,
        K1_MODULE
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

fn knowledge_configure(
    home: &Path,
    binding_id: &str,
    module_reference: &str,
    enabled: bool,
    methods: &str,
    expected: &str,
) -> Value {
    let out = await_cli(
        home,
        &[
            "knowledge",
            "configure",
            "--output",
            "json",
            "--binding-id",
            binding_id,
            "--expected-revision",
            expected,
            "--enabled",
            if enabled { "true" } else { "false" },
            "--module-reference",
            module_reference,
            "--allowed-methods",
            methods,
        ],
    );
    expect_ok("knowledge.configure", &out)
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
async fn knowledge_k1_acceptance_matrix() {
    let home = TestHome::new("runtime-e2e-k1");
    let root = home.path();
    init_home(root);

    // --- 未装配 (cli.case.packages.knowledge-config): inspect/configure
    // work with no provider; query is explicitly unavailable.
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
        expect_failure("knowledge.configure", &out),
        "UNKNOWN_MODULE_REFERENCE"
    );

    // Seed the managed module, then configure the binding against it.
    seed_k1_module(
        root,
        "kb-e2e",
        &["query", "resolve_citation"],
        &["knowledgeBaseIds", "limit", "cursor"],
    );
    let configured = knowledge_configure(
        root,
        "kb-main",
        "kb-e2e",
        true,
        r#"["query","resolve_citation"]"#,
        "absent",
    );
    assert_eq!(configured["persisted"], true);
    assert_eq!(configured["runtimeState"], "pending_validation");

    // --- 未配置: remove the managed manifest — the binding persists, the
    // inspect diagnostics still work without any provider, and the query is
    // explicitly unavailable (never an empty success).
    std::fs::remove_file(root.join("knowledge-modules").join("kb-e2e.json")).unwrap();
    let inspected = knowledge_inspect(root, "kb-main");
    assert_eq!(inspected["state"], "unconfigured");
    assert_eq!(inspected["unavailableReason"], "MODULE_NOT_ASSEMBLED");
    assert_eq!(inspected["declaredMethods"], json!([]));
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
            "contract",
        ],
    );
    assert_eq!(
        expect_failure("knowledge.query", &out),
        "CAPABILITY_UNAVAILABLE"
    );

    // The managed module returns: configured-but-not-validated.
    seed_k1_module(
        root,
        "kb-e2e",
        &["query", "resolve_citation"],
        &["knowledgeBaseIds", "limit", "cursor"],
    );
    let inspected = knowledge_inspect(root, "kb-main");
    assert_eq!(inspected["state"], "configured");
    assert_eq!(inspected["unavailableReason"], "NOT_VALIDATED");
    assert_eq!(
        inspected["declaredMethods"],
        json!(["query", "resolve_citation"])
    );
    // declared != effective before validation.
    assert_eq!(inspected["effectiveMethods"], json!([]));
    let revision = inspected["revision"].as_str().unwrap().to_owned();

    // CAS: a stale revision refuses validation.
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
            "rev-00000099",
        ],
    );
    assert_eq!(expect_failure("knowledge.validate", &out), "CONFLICT");

    if !python3_present() {
        // The no-provider matrix rows are already asserted above; the
        // module-process rows need the fixture runtime.
        eprintln!("note: python3 absent; skipping the k1 module-process rows");
        return;
    }

    // --- validate: the managed manifest/health checks pass and the binding
    // goes ready.
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
            &revision,
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
    assert_eq!(
        inspected["effectiveMethods"],
        json!(["query", "resolve_citation"])
    );

    // --- query: real hits with citations (0-hit ≠ upstream failure).
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
            "contract truth",
            "--limit",
            "10",
        ],
    );
    let queried = expect_ok("knowledge.query", &out);
    assert_eq!(queried["bindingRevision"], json!(revision));
    assert_eq!(queried["hits"].as_array().unwrap().len(), 2);
    assert_eq!(queried["hits"][0]["citationIds"], json!(["cite-1"]));
    assert_eq!(queried["truncated"], false);
    assert_eq!(queried["providerMetadata"]["engine"], "e2e");

    // --- citation.resolve: traceable source; unknown citation NOT_FOUND.
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
            "cite-1",
        ],
    );
    let resolved = expect_ok("knowledge.citation.resolve", &out);
    assert_eq!(resolved["source"]["documentId"], "doc-e2e");
    assert_eq!(resolved["source"]["version"], "7");
    assert_eq!(resolved["contentReference"], "docstore://doc-e2e");
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
            "cite-x",
        ],
    );
    assert_eq!(
        expect_failure("knowledge.citation.resolve", &out),
        "NOT_FOUND"
    );

    // --- 越权: a method outside the binding's allowed set refuses.
    let configured = knowledge_configure(
        root,
        "kb-cite",
        "kb-e2e",
        true,
        r#"["resolve_citation"]"#,
        "absent",
    );
    let out = await_cli(
        root,
        &[
            "knowledge",
            "validate",
            "--output",
            "json",
            "--binding-id",
            "kb-cite",
            "--expected-revision",
            configured["revision"].as_str().unwrap(),
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
            "kb-cite",
            "--query",
            "contract",
        ],
    );
    assert_eq!(
        expect_failure("knowledge.query", &out),
        "CAPABILITY_UNAVAILABLE"
    );

    // --- 声明缺失: a manifest declaring only query refuses citation
    // resolution even when the binding allows it.
    seed_k1_module(
        root,
        "kb-qonly",
        &["query"],
        &["knowledgeBaseIds", "limit", "cursor"],
    );
    let configured = knowledge_configure(
        root,
        "kb-q",
        "kb-qonly",
        true,
        r#"["query","resolve_citation"]"#,
        "absent",
    );
    let out = await_cli(
        root,
        &[
            "knowledge",
            "validate",
            "--output",
            "json",
            "--binding-id",
            "kb-q",
            "--expected-revision",
            configured["revision"].as_str().unwrap(),
        ],
    );
    let validated = expect_ok("knowledge.validate", &out);
    assert_eq!(validated["validated"], true);
    assert_eq!(validated["effectiveMethods"], json!(["query"]));
    let out = await_cli(
        root,
        &[
            "knowledge",
            "citation",
            "resolve",
            "--output",
            "json",
            "--binding-id",
            "kb-q",
            "--citation-id",
            "cite-1",
        ],
    );
    assert_eq!(
        expect_failure("knowledge.citation.resolve", &out),
        "CAPABILITY_UNAVAILABLE"
    );

    // --- disabled: an explicit DISABLED refusal, not an empty success.
    let configured = knowledge_configure(root, "kb-off", "kb-e2e", false, r#"["query"]"#, "absent");
    assert_eq!(configured["runtimeState"], "disabled");
    let out = await_cli(
        root,
        &[
            "knowledge",
            "query",
            "--output",
            "json",
            "--binding-id",
            "kb-off",
            "--query",
            "contract",
        ],
    );
    assert_eq!(expect_failure("knowledge.query", &out), "DISABLED");

    // --- schema 不匹配 (provider payload): a module manifest with an
    // option set that does not declare `limit` refuses the option explicitly.
    seed_k1_module(root, "kb-noopts", &["query", "resolve_citation"], &[]);
    let configured = knowledge_configure(
        root,
        "kb-noopts",
        "kb-noopts",
        true,
        r#"["query"]"#,
        "absent",
    );
    let out = await_cli(
        root,
        &[
            "knowledge",
            "validate",
            "--output",
            "json",
            "--binding-id",
            "kb-noopts",
            "--expected-revision",
            configured["revision"].as_str().unwrap(),
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
            "kb-noopts",
            "--query",
            "x",
            "--limit",
            "5",
        ],
    );
    assert_eq!(
        expect_failure("knowledge.query", &out),
        "UNSUPPORTED_QUERY_OPTION"
    );
}
