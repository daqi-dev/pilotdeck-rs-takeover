//! `cli.task.model-test` end-to-end acceptance (provider_test.* /
//! model_references.*, `007 §10`).
//!
//! Every scenario drives the real `agentcpm-pd` binary against an isolated
//! temporary `AGENTCPM_PD_HOME` with the embedded gateway answering over the
//! real `/v1/ws` transport. Network probes hit a loopback mock provider
//! (`MockProviderEndpoint`, a std-thread HTTP/1.1 responder on
//! `127.0.0.1:0`) — no external service is ever contacted. The scenarios
//! script the task's related cases:
//!
//! * `cli.case.operation.provider_test.start`: validateOnly 零网络,
//!   network 显式且受预算（每 kind 恰好 budget 个请求）、批量逐项失败、
//!   旧配置证据不背书新配置（改 endpoint 后确认被拒）、幂等受理 +
//!   requests.lookup、INSTANCE_MISMATCH 零副作用。
//! * `cli.case.operation.provider_test.retry`: 新 operation 保留来源、原失
//!   败记录不改、失败项可重试而成功项不可。
//! * `cli.case.operation.provider_test.confirm_capabilities` /
//!   `.acknowledge`: 图像能力只认图像探测证据（否定证据也成立）、
//!   text-only 不能确认 image、acknowledge 不改结果、过期 revision 拒绝。
//! * `cli.case.operation.model_references.*`: inspect 列 selection 引用、
//!   plan 产出 planDigest、apply 原子替换、新引用使旧计划失效
//!   （REVISION_CONFLICT 整体拒绝）。
//!
//! 合同行为断言中的本地执行状态：e2e 全量在 cb8/mbp2 跑（本机构建冻
//! 结）；文件按 007 §10 挂具写成即可运行形态。

#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use common_v1::*;

// ---------------------------------------------------------------------------
// Loopback mock provider (std-thread HTTP/1.1, one request per connection)
// ---------------------------------------------------------------------------

struct HttpRequest {
    method: String,
    path: String,
    body: String,
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

fn read_http_request(stream: &mut std::net::TcpStream) -> std::io::Result<HttpRequest> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
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
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_owned();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let mut content_length = 0usize;
    for line in lines {
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
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
    Ok(HttpRequest { method, path, body })
}

/// A scripted loopback provider endpoint. `script(request_index, path, body)`
/// returns the (status, JSON body) to answer with; every served request is
/// journaled so scenarios can assert the exact network footprint.
struct MockProviderEndpoint {
    port: u16,
    log: Arc<Mutex<Vec<Value>>>,
}

impl MockProviderEndpoint {
    fn start<F>(script: F) -> Self
    where
        F: Fn(usize, &str, &str) -> (u16, String) + Send + Sync + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock provider");
        let port = listener.local_addr().unwrap().port();
        let log: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let counter = Arc::new(AtomicUsize::new(0));
        let script = Arc::new(script);
        let log_handle = log.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    continue;
                };
                let log = log.clone();
                let counter = counter.clone();
                let script = script.clone();
                std::thread::spawn(move || {
                    let Ok(request) = read_http_request(&mut stream) else {
                        return;
                    };
                    let index = counter.fetch_add(1, Ordering::SeqCst);
                    let (status, body) = script(index, &request.path, &request.body);
                    log.lock().unwrap().push(json!({
                        "index": index,
                        "method": request.method,
                        "path": request.path,
                        "body": request.body,
                    }));
                    let reason = match status {
                        200 => "OK",
                        400 => "Bad Request",
                        401 => "Unauthorized",
                        404 => "Not Found",
                        _ => "Status",
                    };
                    let response = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
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

    fn requests(&self) -> Vec<Value> {
        self.log.lock().unwrap().clone()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parses a failure envelope: exactly one stdout line, `outcome=failed`,
/// and the contract error payload returned.
fn expect_failed(method: &str, out: &CliOutcome) -> Value {
    assert_ne!(
        out.code, 0,
        "`{method}` unexpectedly succeeded: {}",
        out.stdout
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
    assert_eq!(value["version"], OP_VERSION, "{value}");
    assert_eq!(
        value["outcome"], "failed",
        "`{method}` outcome: {value}; stderr:\n{}",
        out.stderr
    );
    let error = value["error"].clone();
    assert!(
        error["code"].as_str().is_some_and(|c| !c.is_empty()),
        "`{method}` envelope has no error code: {value}"
    );
    error
}

fn wait_terminal_operation(home: &std::path::Path, operation_id: &str) -> Value {
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

fn write_provider(
    home: &std::path::Path,
    provider_id: &str,
    endpoint: &str,
    expected_revision: Option<&str>,
) -> Value {
    let mut args = vec![
        "providers".to_owned(),
        "write".to_owned(),
        "--output".to_owned(),
        "json".to_owned(),
        "--provider-id".to_owned(),
        provider_id.to_owned(),
        "--name".to_owned(),
        format!("{provider_id} display"),
        "--protocol".to_owned(),
        "openai".to_owned(),
        "--endpoint".to_owned(),
        endpoint.to_owned(),
    ];
    if let Some(revision) = expected_revision {
        args.push("--expected-revision".to_owned());
        args.push(revision.to_owned());
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = await_cli(home, &refs);
    expect_ok("provider.write", &out)
}

fn write_model(home: &std::path::Path, provider_id: &str, model_id: &str) -> Value {
    let out = await_cli(
        home,
        &[
            "models",
            "write",
            "--output",
            "json",
            "--model",
            &format!(r#"{{"providerId":"{provider_id}","modelId":"{model_id}"}}"#),
            "--name",
            &format!("{provider_id}/{model_id}"),
        ],
    );
    expect_ok("model.write", &out)
}

// ---------------------------------------------------------------------------
// provider_test.start / confirm / acknowledge over the mock provider
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_test_probe_evidence_confirm_and_acknowledge() {
    let home = TestHome::new("model-test-e2e-probe");
    let root = home.path();
    init_home(root);

    // The mock answers text probes with "1" and image probes with a color
    // name (the probe checks its random color appears in the answer; naming
    // all four is the scripted "model that saw the image" answer).
    let mock = MockProviderEndpoint::start(|_index, _path, body| {
        if body.contains("image_url") || body.contains("inlineData") {
            (
                200,
                r#"{"choices":[{"message":{"content":"the shape is red, blue, green or yellow"}}]}"#
                    .to_owned(),
            )
        } else {
            (
                200,
                r#"{"choices":[{"message":{"content":"1"}}]}"#.to_owned(),
            )
        }
    });

    let created = write_provider(root, "prov-mock", &mock.endpoint(), None);
    let prov_rev = created["provider"]["revision"].as_str().unwrap().to_owned();
    let model = write_model(root, "prov-mock", "m-1");
    let model_rev = model["model"]["revision"].as_str().unwrap().to_owned();

    let targets = json!([{
        "targetId": "t-1",
        "providerId": "prov-mock",
        "modelId": "m-1",
        "configRevision": prov_rev,
        "testKinds": ["text", "image"],
    }])
    .to_string();

    // validateOnly 零网络: accepted, completes, and the mock never hears
    // about it.
    let out = await_cli(
        root,
        &[
            "providers",
            "test",
            "start",
            "--output",
            "json",
            "--targets",
            &targets,
            "--probe-mode",
            "validateOnly",
            "--request-key",
            "key-validate-1",
        ],
    );
    let validate_receipt = expect_ok("provider_test.start", &out);
    let validate_operation =
        wait_terminal_operation(root, validate_receipt["operationId"].as_str().unwrap());
    assert_eq!(validate_operation["operation"]["state"], "succeeded");
    assert_eq!(
        validate_operation["operation"]["result"]["targets"][0]["evidenceIds"]
            .as_array()
            .map(Vec::len),
        Some(0),
        "a validation-only pass never produces evidence"
    );
    assert_eq!(
        mock.requests().len(),
        0,
        "validateOnly must stay zero-network"
    );

    // network is explicit and budgeted: exactly one request per kind.
    let out = await_cli(
        root,
        &[
            "providers",
            "test",
            "start",
            "--output",
            "json",
            "--targets",
            &targets,
            "--probe-mode",
            "network",
            "--request-budget",
            "1",
            "--request-key",
            "key-network-1",
        ],
    );
    let receipt = expect_ok("provider_test.start", &out);
    assert_eq!(receipt["state"], "queued");
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();

    // Idempotent replay: same key + same input returns the same resource
    // (「省略新资源ID且携带有效idempotencyKey能受理；回包丢失后
    // requests.lookup同methodId/key返回原Gateway资源ID」).
    let out = await_cli(
        root,
        &[
            "providers",
            "test",
            "start",
            "--output",
            "json",
            "--targets",
            &targets,
            "--probe-mode",
            "network",
            "--request-budget",
            "1",
            "--request-key",
            "key-network-1",
        ],
    );
    let replay = expect_ok("provider_test.start", &out);
    assert_eq!(replay["deduplicated"], true);
    assert_eq!(replay["operationId"], json!(operation_id));
    let out = await_cli(
        root,
        &[
            "requests",
            "lookup",
            "--output",
            "json",
            "--method-id",
            "provider_test.start",
            "--idempotency-key",
            "key-network-1",
        ],
    );
    let lookup = expect_ok("requests.lookup", &out);
    assert_eq!(lookup["resource"]["operationId"], json!(operation_id));

    let operation = wait_terminal_operation(root, &operation_id);
    assert_eq!(operation["operation"]["state"], "succeeded");
    let result = operation["operation"]["result"].clone();
    assert_eq!(result["partial"], false);
    let outcome = &result["targets"][0];
    assert_eq!(outcome["state"], "succeeded");
    assert_eq!(outcome["issues"].as_array().map(Vec::len), Some(0));
    let evidence_ids: Vec<String> = outcome["evidenceIds"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(evidence_ids.len(), 2, "one evidence row per kind");
    let served = mock.requests();
    assert_eq!(served.len(), 2, "budget 1 = one request per kind");
    for request in &served {
        assert_eq!(request["method"], "POST");
        assert!(
            request["path"]
                .as_str()
                .is_some_and(|p| p == "/v1/chat/completions"),
            "endpoint candidate: {}",
            request["path"]
        );
    }

    // confirm_capabilities: text-only cannot confirm image; both kinds of
    // evidence can (image capability never inferred from text).
    let evidence_json = serde_json::to_string(&evidence_ids).unwrap();
    let confirm_args = |evidence: &str, capabilities: &str| {
        vec![
            "providers".to_owned(),
            "test".to_owned(),
            "confirm-capabilities".to_owned(),
            "--output".to_owned(),
            "json".to_owned(),
            "--operation-id".to_owned(),
            operation_id.clone(),
            "--model".to_owned(),
            r#"{"providerId":"prov-mock","modelId":"m-1"}"#.to_owned(),
            "--expected-model-revision".to_owned(),
            model_rev.clone(),
            "--evidence-ids".to_owned(),
            evidence.to_owned(),
            "--capabilities".to_owned(),
            capabilities.to_owned(),
        ]
    };
    let only_text = json!([evidence_ids[0]]).to_string();
    let args = confirm_args(&only_text, r#"[{"key":"image","value":true}]"#);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = await_cli(root, &refs);
    let error = expect_failed("provider_test.confirm_capabilities", &out);
    assert_eq!(error["code"], "INSUFFICIENT_EVIDENCE");

    let args = confirm_args(
        &evidence_json,
        r#"[{"key":"text","value":true},{"key":"image","value":true}]"#,
    );
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = await_cli(root, &refs);
    let confirmed = expect_ok("provider_test.confirm_capabilities", &out);
    let new_model_rev = confirmed["model"]["revision"].as_str().unwrap().to_owned();
    let evidence_pairs = confirmed["model"]["capabilityEvidence"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        evidence_pairs
            .iter()
            .any(|p| p["key"] == "text" && p["value"] == true)
    );
    assert!(
        evidence_pairs
            .iter()
            .any(|p| p["key"] == "image" && p["value"] == true)
    );

    // 陈旧版本不写: the old model revision conflicts.
    let args = confirm_args(&evidence_json, r#"[{"key":"image","value":true}]"#);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = await_cli(root, &refs);
    let error = expect_failed("provider_test.confirm_capabilities", &out);
    assert_eq!(error["code"], "REVISION_CONFLICT");

    // 旧配置证据不背书新配置: change the endpoint, the evidence no longer
    // matches the live configuration.
    write_provider(
        root,
        "prov-mock",
        &format!("{}/v2", mock.endpoint()),
        Some(&prov_rev),
    );
    let args = confirm_args(&evidence_json, r#"[{"key":"image","value":true}]"#);
    // Refresh the expected model revision to the current one.
    let mut refreshed = args.clone();
    let pos = refreshed
        .iter()
        .position(|a| a == &model_rev)
        .expect("model revision arg");
    refreshed[pos] = new_model_rev.clone();
    let refs: Vec<&str> = refreshed.iter().map(String::as_str).collect();
    let out = await_cli(root, &refs);
    let error = expect_failed("provider_test.confirm_capabilities", &out);
    assert_eq!(error["code"], "EVIDENCE_MISMATCH");

    // acknowledge: the human has seen the result; the outcome itself never
    // changes and a stale revision is refused.
    let op_rev = operation["operation"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    let out = await_cli(
        root,
        &[
            "providers",
            "test",
            "acknowledge",
            "--output",
            "json",
            "--operation-id",
            &operation_id,
            "--expected-revision",
            &op_rev,
        ],
    );
    let acked = expect_ok("provider_test.acknowledge", &out);
    assert_eq!(acked["acknowledged"], true);
    let out = await_cli(
        root,
        &[
            "providers",
            "test",
            "acknowledge",
            "--output",
            "json",
            "--operation-id",
            &operation_id,
            "--expected-revision",
            &op_rev,
        ],
    );
    let error = expect_failed("provider_test.acknowledge", &out);
    assert_eq!(error["code"], "REVISION_CONFLICT");
    let after = wait_terminal_operation(root, &operation_id);
    assert_eq!(
        after["operation"]["result"], result,
        "acknowledge never rewrites the recorded outcome"
    );

    // INSTANCE_MISMATCH: explicit input JSON naming another instance; both
    // ends see zero side effects.
    let requests_before = mock.requests().len();
    let bad_input = json!({
        "instanceId": "not-this-instance",
        "targets": [{
            "targetId": "t-1",
            "providerId": "prov-mock",
            "modelId": "m-1",
            "configRevision": "rev-00000001",
            "testKinds": ["text"],
        }],
        "probeMode": "network",
    });
    let stdin = bad_input.to_string();
    let out = cli_with_stdin(
        root,
        &[
            "providers",
            "test",
            "start",
            "--output",
            "json",
            "--input-json",
            "-",
            "--request-key",
            "key-mismatch-1",
        ],
        Some(&stdin),
    )
    .await;
    let error = expect_failed("provider_test.start", &out);
    assert_eq!(error["code"], "INSTANCE_MISMATCH");
    assert_eq!(mock.requests().len(), requests_before);
    let out = await_cli(
        root,
        &[
            "requests",
            "lookup",
            "--output",
            "json",
            "--method-id",
            "provider_test.start",
            "--idempotency-key",
            "key-mismatch-1",
        ],
    );
    let lookup = expect_ok("requests.lookup", &out);
    assert_eq!(
        lookup["requestState"], "result_unknown",
        "a rejected-before-acceptance request must not be observed as accepted: {lookup}"
    );
}

// ---------------------------------------------------------------------------
// Batch per-item failure + retry keeps the source and repairs the item
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_test_partial_failure_and_retry() {
    let home = TestHome::new("model-test-e2e-retry");
    let root = home.path();
    init_home(root);

    // First request answers 401 (auth failure), everything after succeeds —
    // the retried target lands on a healthy endpoint.
    let mock = MockProviderEndpoint::start(|index, _path, _body| {
        if index == 0 {
            (
                401,
                r#"{"error":{"message":"Invalid API key supplied"}}"#.to_owned(),
            )
        } else {
            (
                200,
                r#"{"choices":[{"message":{"content":"1"}}]}"#.to_owned(),
            )
        }
    });

    let p1 = write_provider(root, "prov-p1", &mock.endpoint(), None);
    let p2 = write_provider(root, "prov-p2", &mock.endpoint(), None);
    write_model(root, "prov-p1", "m-1");
    write_model(root, "prov-p2", "m-2");
    let targets = json!([
        {
            "targetId": "t-p1",
            "providerId": "prov-p1",
            "modelId": "m-1",
            "configRevision": p1["provider"]["revision"],
            "testKinds": ["text"],
        },
        {
            "targetId": "t-p2",
            "providerId": "prov-p2",
            "modelId": "m-2",
            "configRevision": p2["provider"]["revision"],
            "testKinds": ["text"],
        },
    ])
    .to_string();

    let out = await_cli(
        root,
        &[
            "providers",
            "test",
            "start",
            "--output",
            "json",
            "--targets",
            &targets,
            "--probe-mode",
            "network",
            "--request-key",
            "key-batch-1",
        ],
    );
    let receipt = expect_ok("provider_test.start", &out);
    let source_id = receipt["operationId"].as_str().unwrap().to_owned();
    let operation = wait_terminal_operation(root, &source_id);
    // 批量逐项失败: the operation records per-item outcomes; the failing
    // item does not sink the healthy one.
    assert_eq!(operation["operation"]["state"], "succeeded");
    assert_eq!(operation["operation"]["result"]["partial"], true);
    assert_eq!(
        operation["operation"]["result"]["targets"][0]["state"],
        "failed"
    );
    assert_eq!(
        operation["operation"]["result"]["targets"][0]["issues"][0]["code"],
        "AUTHENTICATION_FAILED"
    );
    assert_eq!(
        operation["operation"]["result"]["targets"][1]["state"],
        "succeeded"
    );
    assert_eq!(mock.requests().len(), 2);
    let source_result = operation["operation"]["result"].clone();
    let source_revision = operation["operation"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();

    // Retrying a succeeded target is refused (「重试显式失败测试项」).
    let out = await_cli(
        root,
        &[
            "providers",
            "test",
            "retry",
            "--output",
            "json",
            "--source-operation-id",
            &source_id,
            "--target-ids",
            "t-p2",
            "--probe-mode",
            "network",
            "--request-key",
            "key-retry-bad",
        ],
    );
    let error = expect_failed("provider_test.retry", &out);
    assert_eq!(error["code"], "INVALID_ARGUMENT");

    // Retry the failed item: a NEW operation that keeps the source lineage
    // and lands on the now-healthy endpoint; the original record is frozen.
    let out = await_cli(
        root,
        &[
            "providers",
            "test",
            "retry",
            "--output",
            "json",
            "--source-operation-id",
            &source_id,
            "--target-ids",
            "t-p1",
            "--probe-mode",
            "network",
            "--request-key",
            "key-retry-1",
        ],
    );
    let retry_receipt = expect_ok("provider_test.retry", &out);
    let retry_id = retry_receipt["operationId"].as_str().unwrap().to_owned();
    assert_ne!(retry_id, source_id);
    let retry_operation = wait_terminal_operation(root, &retry_id);
    assert_eq!(retry_operation["operation"]["state"], "succeeded");
    assert_eq!(
        retry_operation["operation"]["result"]["sourceOperationId"],
        json!(source_id),
        "新operation保留来源"
    );
    assert_eq!(
        retry_operation["operation"]["result"]["targets"][0]["state"],
        "succeeded"
    );
    assert_eq!(
        retry_operation["operation"]["result"]["targets"][0]["evidenceIds"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    // 原失败记录不改.
    let after = wait_terminal_operation(root, &source_id);
    assert_eq!(after["operation"]["result"], source_result);
    assert_eq!(after["operation"]["revision"], json!(source_revision));
    assert_eq!(mock.requests().len(), 3, "one more request for the retry");
}

// ---------------------------------------------------------------------------
// Negative image evidence (explicit server-side "unsupported" statement)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_test_image_unsupported_negative_evidence() {
    let home = TestHome::new("model-test-e2e-image");
    let root = home.path();
    init_home(root);

    let mock = MockProviderEndpoint::start(|_index, _path, body| {
        if body.contains("image_url") || body.contains("inlineData") {
            (
                400,
                r#"{"error":{"message":"Image input is not supported for this model"}}"#.to_owned(),
            )
        } else {
            (
                200,
                r#"{"choices":[{"message":{"content":"1"}}]}"#.to_owned(),
            )
        }
    });
    let created = write_provider(root, "prov-neg", &mock.endpoint(), None);
    write_model(root, "prov-neg", "m-neg");
    let targets = json!([{
        "targetId": "t-neg",
        "providerId": "prov-neg",
        "modelId": "m-neg",
        "configRevision": created["provider"]["revision"],
        "testKinds": ["text", "image"],
    }])
    .to_string();

    let out = await_cli(
        root,
        &[
            "providers",
            "test",
            "start",
            "--output",
            "json",
            "--targets",
            &targets,
            "--probe-mode",
            "network",
            "--request-key",
            "key-neg-1",
        ],
    );
    let receipt = expect_ok("provider_test.start", &out);
    let operation = wait_terminal_operation(root, receipt["operationId"].as_str().unwrap());
    // The server's explicit unsupported statement is a definitive answer:
    // the target completed (no partial), with evidence for both kinds.
    assert_eq!(operation["operation"]["state"], "succeeded");
    assert_eq!(operation["operation"]["result"]["partial"], false);
    let outcome = &operation["operation"]["result"]["targets"][0];
    assert_eq!(outcome["state"], "succeeded");
    let evidence_ids: Vec<String> = outcome["evidenceIds"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(evidence_ids.len(), 2);

    // Confirm the image capability as unsupported — and never as supported.
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
    let evidence_json = serde_json::to_string(&evidence_ids).unwrap();
    let out = await_cli(
        root,
        &[
            "providers",
            "test",
            "confirm-capabilities",
            "--output",
            "json",
            "--operation-id",
            &operation_id,
            "--model",
            r#"{"providerId":"prov-neg","modelId":"m-neg"}"#,
            "--expected-model-revision",
            // The model row is unchanged since the write; fetch its live
            // revision through the read path.
            model_revision_of(root, "prov-neg", "m-neg").as_str(),
            "--evidence-ids",
            &evidence_json,
            "--capabilities",
            r#"[{"key":"image","value":true}]"#,
        ],
    );
    let error = expect_failed("provider_test.confirm_capabilities", &out);
    assert_eq!(error["code"], "EVIDENCE_MISMATCH");

    let out = await_cli(
        root,
        &[
            "providers",
            "test",
            "confirm-capabilities",
            "--output",
            "json",
            "--operation-id",
            &operation_id,
            "--model",
            r#"{"providerId":"prov-neg","modelId":"m-neg"}"#,
            "--expected-model-revision",
            model_revision_of(root, "prov-neg", "m-neg").as_str(),
            "--evidence-ids",
            &evidence_json,
            "--capabilities",
            r#"[{"key":"text","value":true},{"key":"image","value":false}]"#,
        ],
    );
    let confirmed = expect_ok("provider_test.confirm_capabilities", &out);
    let evidence_pairs = confirmed["model"]["capabilityEvidence"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        evidence_pairs
            .iter()
            .any(|p| p["key"] == "image" && p["value"] == false)
    );
}

fn model_revision_of(root: &std::path::Path, provider_id: &str, model_id: &str) -> String {
    let out = await_cli(
        root,
        &[
            "models",
            "get",
            "--output",
            "json",
            "--model",
            &format!(r#"{{"providerId":"{provider_id}","modelId":"{model_id}"}}"#),
        ],
    );
    let model = expect_ok("model.get", &out);
    model["model"]["revision"]
        .as_str()
        .expect("model revision")
        .to_owned()
}

// ---------------------------------------------------------------------------
// model_references inspect / plan / apply
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_references_plan_apply_flow() {
    let home = TestHome::new("model-test-e2e-refs");
    let root = home.path();
    init_home(root);

    write_provider(root, "prov-old", "https://api.example.com/v1", None);
    write_model(root, "prov-old", "m-old");
    write_model(root, "prov-old", "m-new");

    // An instance-scope selection referencing the old model.
    let out = await_cli(
        root,
        &[
            "models",
            "selection",
            "get",
            "--output",
            "json",
            "--scope",
            r#"{"kind":"instance"}"#,
        ],
    );
    let selection = expect_ok("model_selection.get", &out);
    let out = await_cli(
        root,
        &[
            "models",
            "selection",
            "set",
            "--output",
            "json",
            "--scope",
            r#"{"kind":"instance"}"#,
            "--model",
            r#"{"providerId":"prov-old","modelId":"m-old"}"#,
            "--expected-revision",
            selection["revision"].as_str().unwrap(),
        ],
    );
    expect_ok("model_selection.set", &out);

    // inspect: the selection is the reference; the whole set has a revision.
    let out = await_cli(
        root,
        &[
            "models",
            "references",
            "inspect",
            "--output",
            "json",
            "--provider-id",
            "prov-old",
        ],
    );
    let inspected = expect_ok("model_references.inspect", &out);
    assert_eq!(inspected["references"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        inspected["references"][0]["resourceType"],
        "model_selection"
    );
    assert_eq!(inspected["references"][0]["settingKey"], "model");
    assert_eq!(inspected["pageInfo"]["complete"], true);
    let set_rev = inspected["referenceSetRevision"]
        .as_str()
        .unwrap()
        .to_owned();

    // plan (dry run): zero writes, one change, a commit digest.
    let out = await_cli(
        root,
        &[
            "models",
            "references",
            "plan",
            "--output",
            "json",
            "--provider-id",
            "prov-old",
            "--model-id",
            "m-old",
            "--replacement",
            r#"{"providerId":"prov-old","modelId":"m-new"}"#,
            "--expected-reference-set-revision",
            &set_rev,
        ],
    );
    let plan = expect_ok("model_references.plan", &out);
    assert_eq!(plan["changes"].as_array().map(Vec::len), Some(1));
    assert_eq!(plan["issues"].as_array().map(Vec::len), Some(0));
    assert!(
        plan["planDigest"].as_str().is_some_and(|d| d.len() == 64),
        "planDigest is a sha256 hex digest: {}",
        plan["planDigest"]
    );
    let digest = plan["planDigest"].as_str().unwrap().to_owned();

    // A stale plan is refused whole: a new reference (project-scope
    // selection) invalidates the set revision (「新引用使旧计划失效」).
    let out = await_cli(
        root,
        &[
            "projects",
            "register",
            "--output",
            "json",
            "--project-id",
            "p-refs",
            "--name",
            "refs project",
            "--environment-id",
            "local",
            "--root",
            &format!(
                r#"{{"instanceId":"{}","rootId":"r-refs","relativePath":"ws"}}"#,
                init_home_instance(root)
            ),
        ],
    );
    expect_ok("project.register", &out);
    let out = await_cli(
        root,
        &[
            "models",
            "selection",
            "get",
            "--output",
            "json",
            "--scope",
            r#"{"kind":"project","projectId":"p-refs"}"#,
        ],
    );
    let project_selection = expect_ok("model_selection.get", &out);
    let out = await_cli(
        root,
        &[
            "models",
            "selection",
            "set",
            "--output",
            "json",
            "--scope",
            r#"{"kind":"project","projectId":"p-refs"}"#,
            "--model",
            r#"{"providerId":"prov-old","modelId":"m-old"}"#,
            "--expected-revision",
            project_selection["revision"].as_str().unwrap(),
        ],
    );
    expect_ok("model_selection.set", &out);

    let out = await_cli(
        root,
        &[
            "models",
            "references",
            "apply",
            "--output",
            "json",
            "--provider-id",
            "prov-old",
            "--model-id",
            "m-old",
            "--replacement",
            r#"{"providerId":"prov-old","modelId":"m-new"}"#,
            "--expected-reference-set-revision",
            &set_rev,
            "--plan-digest",
            &digest,
            "--request-key",
            "key-refs-stale",
        ],
    );
    let error = expect_failed("model_references.apply", &out);
    assert_eq!(error["code"], "REVISION_CONFLICT");

    // Re-plan against the current set and apply for real: every reference
    // to the old model is replaced, no dangling reference is left.
    let out = await_cli(
        root,
        &[
            "models",
            "references",
            "inspect",
            "--output",
            "json",
            "--provider-id",
            "prov-old",
        ],
    );
    let inspected = expect_ok("model_references.inspect", &out);
    let set_rev2 = inspected["referenceSetRevision"]
        .as_str()
        .unwrap()
        .to_owned();
    let out = await_cli(
        root,
        &[
            "models",
            "references",
            "plan",
            "--output",
            "json",
            "--provider-id",
            "prov-old",
            "--model-id",
            "m-old",
            "--replacement",
            r#"{"providerId":"prov-old","modelId":"m-new"}"#,
            "--expected-reference-set-revision",
            &set_rev2,
        ],
    );
    let plan2 = expect_ok("model_references.plan", &out);
    let digest2 = plan2["planDigest"].as_str().unwrap().to_owned();
    let out = await_cli(
        root,
        &[
            "models",
            "references",
            "apply",
            "--output",
            "json",
            "--provider-id",
            "prov-old",
            "--model-id",
            "m-old",
            "--replacement",
            r#"{"providerId":"prov-old","modelId":"m-new"}"#,
            "--expected-reference-set-revision",
            &set_rev2,
            "--plan-digest",
            &digest2,
            "--request-key",
            "key-refs-1",
        ],
    );
    let receipt = expect_ok("model_references.apply", &out);
    let operation = wait_terminal_operation(root, receipt["operationId"].as_str().unwrap());
    assert_eq!(operation["operation"]["state"], "succeeded");
    assert_eq!(
        operation["operation"]["result"]["updatedReferences"]
            .as_array()
            .map(Vec::len),
        Some(2),
        "both the instance and project selections are replaced: {}",
        operation["operation"]["result"]
    );
    assert_eq!(operation["operation"]["result"]["sourceRemoved"], false);

    // The references now resolve to the replacement; nothing dangles.
    let out = await_cli(
        root,
        &[
            "models",
            "references",
            "inspect",
            "--output",
            "json",
            "--provider-id",
            "prov-old",
            "--model-id",
            "m-new",
        ],
    );
    let after = expect_ok("model_references.inspect", &out);
    assert_eq!(after["references"].as_array().map(Vec::len), Some(2));
    let out = await_cli(
        root,
        &[
            "models",
            "references",
            "inspect",
            "--output",
            "json",
            "--provider-id",
            "prov-old",
            "--model-id",
            "m-old",
        ],
    );
    let old_refs = expect_ok("model_references.inspect", &out);
    assert_eq!(old_refs["references"].as_array().map(Vec::len), Some(0));
    let out = await_cli(
        root,
        &[
            "models",
            "selection",
            "get",
            "--output",
            "json",
            "--scope",
            r#"{"kind":"instance"}"#,
        ],
    );
    let selection = expect_ok("model_selection.get", &out);
    assert_eq!(selection["overrideModel"]["modelId"], "m-new");
}
