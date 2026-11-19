//! S5 业务旅程 J2（模型、供应商与配置管理）端到端验收。
//!
//! 旅程正本是 `_tmp/20261119-s5/journeys-execution-plan.md` §3 J2：单一用户
//! 在一个实例上完成「读配置 → 登记供应商 → 写入凭据 → 登记模型 → 连通性
//! 探测与能力确认 → 切换默认模型 → 用量核对 → 改配置并重载」的完整业务
//! 闭环。命令名按计划 §2 的映射表改写为实际 CLI 形态（`configuration.*` →
//! `settings`、`provider_test.*` → `providers test`、复数组名连字符）。
//!
//! 两条场景（每条一个 `#[tokio::test]`，全部驱动真实 `agentcpm-pd` 二进制、
//! 隔离 `AGENTCPM_PD_HOME`、`--output json` envelope 经嵌入 registry 的
//! schema 校验）：
//!
//! * 正向旅程：settings 读面与 validate 只读性 → providers/credentials/
//!   models 登记（secret 只经 stdin、stdout 仅指纹）→ provider_test
//!   validateOnly 零网络 + network 预算探测 → confirm-capabilities →
//!   acknowledge → model_selection 切换 → usage 前后一致性 → settings set
//!   代际推进 + reload 幂等回执。
//! * 故障注入旅程：argv 明文秘密被拒（出口前拦截）→ 凭据禁用后探测
//!   fail-closed（AUTHENTICATION_FAILED、零网络）→ 过期 revision 的 CAS
//!   拒绝 → 重启用 `providers test retry` 恢复且原失败记录冻结。
//!
//! 覆盖缺口（与计划差异，见 `_tmp/20261119-s5/w1a-report.md`）：
//! * `--secret-output` 在本构建是 parse-only 全局旗标，无「秘密落新文件」
//!   的可观察行为——按 c026 的实际合同断言 stdout/stderr 无明文秘密；
//! * `v1_usage_records` 无生产写入方（run 完成/provider_test 均未接线），
//!   usage 数字核对只能断言「前后一致的空集」，聚合规行为的深断言在
//!   gateway 域内测试。

#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use common_v1::*;

// ---------------------------------------------------------------------------
// 本地装置：loopback mock provider（与 e2e_v1_model_test.rs 同形态）
// ---------------------------------------------------------------------------

struct HttpRequest {
    #[allow(dead_code)] // 装置保留 method 字段以便诊断
    method: String,
    #[allow(dead_code)] // 同上
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
    // 满载预算（判据不变）：mock 端等被测进程把请求写完，10s→60s。
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

/// 脚本化 loopback 供应商端点：`script(request_index, path, body)` 返回应答
/// (status, JSON body)；每个收到的请求都进日志，场景据此断言精确网络足迹。
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
// 本地装置：失败 envelope / 操作终态轮询 / settings 助手
// ---------------------------------------------------------------------------

/// 解析失败 envelope：恰好一行 stdout、`outcome=failed`、返回合同错误负载。
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

/// 轮询 `operations get` 至终态（provider_test 的异步收据落终态很快，但
/// 轮询上限保持宽松以容忍套件并发下的调度抖动）。
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

const INSTANCE: &str = "{\"kind\":\"instance\"}";

async fn settings_get(home: &std::path::Path, scope: &str) -> Value {
    let out = await_cli(
        home,
        &["settings", "get", "--output", "json", "--scope", scope],
    );
    expect_ok("configuration.resolve", &out)
}

fn value_of<'a>(result: &'a Value, key: &str) -> Option<&'a Value> {
    result["values"]
        .as_array()?
        .iter()
        .find(|v| v["key"] == key)
        .map(|v| &v["value"])
}

/// 旅程 J2 正向主干：配置读面 → 登记 → 探测与能力确认 → 选型 → 用量 →
/// 配置写入与重载。每步的 stdout envelope 都过嵌入 registry 的 schema 校验，
/// 关键状态转移（revision/generation/enabled/capabilityEvidence）逐项断言。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journey_j2_model_provider_configuration_lifecycle() {
    let home = TestHome::new("j2-e2e-lifecycle");
    let root = home.path();
    init_home(root);

    // mock 供应商：text 探测回 "1"，image 探测回点名全部四种颜色的回答
    // （探测校验其随机颜色出现在答案里——报全部即「模型看到了图」）。
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

    // ---- 1. settings 读面：空实例的诚实状态 + layer 原始层 + validate 只读
    let resolved = settings_get(root, INSTANCE).await;
    assert_eq!(resolved["values"], json!([]));
    assert_eq!(resolved["storedRevision"], "cfg-0");
    assert_eq!(resolved["effectiveGeneration"], "cfggen-0");
    assert_eq!(resolved["reloadState"], "current");

    let out = await_cli(
        root,
        &["settings", "layer", "--output", "json", "--scope", INSTANCE],
    );
    let layer = expect_ok("configuration.read_layer", &out);
    assert_eq!(layer["values"].as_array().map(Vec::len), Some(0));

    // validate：非法候选出诊断（valid=false + issues），合法候选 digest 稳定，
    // 两者都不落盘（storedRevision 不动）。
    let out = await_cli(
        root,
        &[
            "settings",
            "validate",
            "--output",
            "json",
            "--scope",
            INSTANCE,
            "--expected-revision",
            "cfg-0",
            "--changes",
            r#"[{"key": "", "action": "set", "value": "x"}]"#,
        ],
    );
    let invalid = expect_ok("configuration.validate", &out);
    assert_eq!(invalid["valid"], false);
    assert!(
        invalid["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["code"] == "EMPTY_KEY")
    );

    let valid_args = |changes: &str| {
        vec![
            "settings".to_owned(),
            "validate".to_owned(),
            "--output".to_owned(),
            "json".to_owned(),
            "--scope".to_owned(),
            INSTANCE.to_owned(),
            "--expected-revision".to_owned(),
            "cfg-0".to_owned(),
            "--changes".to_owned(),
            changes.to_owned(),
        ]
    };
    let candidate = r#"[{"key": "theme", "action": "set", "value": "dark"}]"#;
    let refs = valid_args(candidate);
    let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
    let first = expect_ok("configuration.validate", &await_cli(root, &refs));
    let refs = valid_args(candidate);
    let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
    let second = expect_ok("configuration.validate", &await_cli(root, &refs));
    assert_eq!(first["valid"], true);
    assert_eq!(first["candidateDigest"], second["candidateDigest"]);
    let digest = first["candidateDigest"].as_str().unwrap().to_owned();
    assert_eq!(digest.len(), 64);
    assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
    let resolved = settings_get(root, INSTANCE).await;
    assert_eq!(
        resolved["storedRevision"], "cfg-0",
        "validate must not write"
    );

    // ---- 2. 供应商登记（保存不自动出网）+ 凭据写入（secret 只经 stdin）
    let out = await_cli(
        root,
        &[
            "providers",
            "write",
            "--output",
            "json",
            "--provider-id",
            "prov-j2",
            "--name",
            "J2 mock provider",
            "--protocol",
            "openai",
            "--endpoint",
            &mock.endpoint(),
        ],
    );
    let provider = expect_ok("provider.write", &out);
    assert_eq!(provider["provider"]["providerId"], "prov-j2");
    assert_eq!(provider["provider"]["credentialRef"], Value::Null);
    let prov_rev1 = provider["provider"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();

    // 凭据：secret 只允许经 --input-json（stdin）进入；--secret-output 显式
    // 声明「新秘密不落 stdout/日志」的输出策略（见文件头覆盖缺口说明）。
    let secret = "sk-j2-e2e-journey-secret-7f31";
    let input = json!({
        "credentialId": "cred-j2",
        "kind": "api_key",
        "name": "J2 journey key",
        "secret": secret,
    })
    .to_string();
    let out = cli_with_stdin(
        root,
        &[
            "credentials",
            "write",
            "--output",
            "json",
            "--input-json",
            "-",
            "--secret-output",
        ],
        Some(&input),
    )
    .await;
    let created = expect_ok("credential.write", &out);
    let credential = &created["credential"];
    assert_eq!(credential["credentialId"], "cred-j2");
    assert_eq!(credential["kind"], "api_key");
    assert_eq!(credential["enabled"], true);
    assert!(
        credential["fingerprint"]
            .as_str()
            .is_some_and(|f| f.starts_with("sha256:")),
        "fingerprint must be hash-derived: {credential}"
    );
    assert!(
        !out.stdout.contains(secret) && !out.stderr.contains(secret),
        "the secret must never appear in stdout or stderr"
    );
    let cred_rev1 = credential["revision"].as_str().unwrap().to_owned();

    // 供应商挂接凭据引用（revision CAS）：stdout 只回 credentialRef。
    let out = await_cli(
        root,
        &[
            "providers",
            "write",
            "--output",
            "json",
            "--provider-id",
            "prov-j2",
            "--expected-revision",
            &prov_rev1,
            "--name",
            "J2 mock provider",
            "--protocol",
            "openai",
            "--endpoint",
            &mock.endpoint(),
            "--credential-ref",
            "cred-j2",
        ],
    );
    let attached = expect_ok("provider.write", &out);
    assert_eq!(attached["provider"]["credentialRef"], "cred-j2");
    let prov_rev2 = attached["provider"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();

    // 凭据启停的状态转移：disable → enable，revision 单调推进。
    let out = await_cli(
        root,
        &[
            "credentials",
            "set-enabled",
            "--output",
            "json",
            "--credential-id",
            "cred-j2",
            "--expected-revision",
            &cred_rev1,
            "--enabled",
            "false",
        ],
    );
    let disabled = expect_ok("credential.set_enabled", &out);
    assert_eq!(disabled["credential"]["enabled"], false);
    let cred_rev2 = disabled["credential"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    let out = await_cli(
        root,
        &[
            "credentials",
            "set-enabled",
            "--output",
            "json",
            "--credential-id",
            "cred-j2",
            "--expected-revision",
            &cred_rev2,
            "--enabled",
            "true",
        ],
    );
    let enabled = expect_ok("credential.set_enabled", &out);
    assert_eq!(enabled["credential"]["enabled"], true);
    let cred_rev3 = enabled["credential"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(cred_rev1, cred_rev2);
    assert_ne!(cred_rev2, cred_rev3);

    // 列表面：凭据与模型各自的清单（凭据行只有元数据 + 指纹）。
    let out = await_cli(root, &["credentials", "list", "--output", "json"]);
    let creds = expect_ok("credential.list", &out);
    assert_eq!(creds["items"].as_array().map(Vec::len), Some(1));
    assert!(
        !out.stdout.contains(secret),
        "credential.list must stay secret-free"
    );

    // ---- 3. 模型登记 / list / get
    let out = await_cli(
        root,
        &[
            "models",
            "write",
            "--output",
            "json",
            "--model",
            r#"{"providerId":"prov-j2","modelId":"gpt-j2"}"#,
            "--name",
            "GPT J2",
            "--declared-capabilities",
            r#"[{"key":"text","value":true}]"#,
        ],
    );
    let model = expect_ok("model.write", &out);
    assert_eq!(model["model"]["name"], "GPT J2");
    assert_eq!(model["model"]["capabilities"][0]["key"], "text");
    let model_rev1 = model["model"]["revision"].as_str().unwrap().to_owned();

    let out = await_cli(root, &["models", "list", "--output", "json"]);
    let listed = expect_ok("model.list", &out);
    assert_eq!(listed["items"].as_array().map(Vec::len), Some(1));
    let out = await_cli(
        root,
        &[
            "models",
            "get",
            "--output",
            "json",
            "--model",
            r#"{"providerId":"prov-j2","modelId":"gpt-j2"}"#,
        ],
    );
    let got = expect_ok("model.get", &out);
    assert_eq!(got["model"]["revision"], model_rev1.as_str());

    // ---- 4. provider_test：validateOnly 零网络 → network 预算探测 →
    // confirm-capabilities → acknowledge
    let targets = json!([{
        "targetId": "t-j2",
        "providerId": "prov-j2",
        "modelId": "gpt-j2",
        "configRevision": prov_rev2,
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
            "validateOnly",
            "--request-key",
            "j2-validate-1",
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

    // 网络探测显式且受预算：text+image 各恰一个请求。
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
            "j2-network-1",
        ],
    );
    let receipt = expect_ok("provider_test.start", &out);
    assert_eq!(receipt["state"], "queued");
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
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

    // 能力确认：text+image 双证据把两项能力写进模型（revision 推进）。
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
            r#"{"providerId":"prov-j2","modelId":"gpt-j2"}"#,
            "--expected-model-revision",
            &model_rev1,
            "--evidence-ids",
            &evidence_json,
            "--capabilities",
            r#"[{"key":"text","value":true},{"key":"image","value":true}]"#,
        ],
    );
    let confirmed = expect_ok("provider_test.confirm_capabilities", &out);
    let model_rev2 = confirmed["model"]["revision"].as_str().unwrap().to_owned();
    assert_ne!(model_rev1, model_rev2, "confirm advances the model row");
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

    // acknowledge：人已看过结果；结果本身不变。
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
    let after = wait_terminal_operation(root, &operation_id);
    assert_eq!(
        after["operation"]["result"], result,
        "acknowledge never rewrites the recorded outcome"
    );

    // ---- 5. 模型选型：instance 作用域切换默认模型
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
    let unset = expect_ok("model_selection.get", &out);
    assert_eq!(unset["effectiveModel"], Value::Null);
    assert_eq!(unset["overrideModel"], Value::Null);
    let selection_rev = unset["revision"].as_str().unwrap().to_owned();

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
            "--expected-revision",
            &selection_rev,
            "--model",
            r#"{"providerId":"prov-j2","modelId":"gpt-j2"}"#,
        ],
    );
    let set = expect_ok("model_selection.set", &out);
    assert_eq!(set["overrideModel"]["modelId"], "gpt-j2");
    assert_eq!(set["effectiveModel"]["modelId"], "gpt-j2");
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
    let after = expect_ok("model_selection.get", &out);
    assert_eq!(after["overrideModel"]["modelId"], "gpt-j2");

    // ---- 6. usage：前后一致性（本构建无用量写入方——空集是诚实读数）
    // The CLI path is `usage snapshot` / `usage summary` (tree: ["usage", <sub>]).
    let usage_args = |command: &str, extra: &[&str]| {
        let mut args = vec![
            "usage".to_owned(),
            command.to_owned(),
            "--output".to_owned(),
            "json".to_owned(),
        ];
        for part in extra {
            args.push((*part).to_owned());
        }
        args
    };
    let refs = usage_args("snapshot", &["--scope", INSTANCE]);
    let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
    let usage_before = expect_ok("usage.snapshot", &await_cli(root, &refs));
    assert_eq!(usage_before["items"].as_array().map(Vec::len), Some(0));
    assert_eq!(usage_before["complete"], true);

    let refs = usage_args("snapshot", &["--scope", INSTANCE]);
    let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
    let usage_after = expect_ok("usage.snapshot", &await_cli(root, &refs));
    assert_eq!(
        usage_after["items"].as_array().map(Vec::len),
        Some(0),
        "no usage writer is wired in this build; the count must stay consistent"
    );
    assert_eq!(usage_after["complete"], true);
    assert_eq!(usage_after["items"], usage_before["items"]);
    assert!(
        usage_after["pageInfo"]["snapshotRevision"]
            .as_str()
            .is_some_and(|r| !r.is_empty())
    );

    let refs = usage_args("summary", &["--scope", INSTANCE, "--group-by", "model"]);
    let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
    let summary = expect_ok("usage.summary", &await_cli(root, &refs));
    assert_eq!(summary["rows"].as_array().map(Vec::len), Some(0));
    assert_eq!(summary["unknownRecordCount"], 0);
    assert!(summary["asOf"].as_str().is_some_and(|t| !t.is_empty()));

    // ---- 7. settings set → 代际推进 → reload 幂等回执
    let out = await_cli(
        root,
        &[
            "settings",
            "set",
            "--output",
            "json",
            "--scope",
            INSTANCE,
            "--expected-revision",
            "cfg-0",
            "--changes",
            r#"[{"key": "theme", "action": "set", "value": "dark"}]"#,
        ],
    );
    let stored = expect_ok("configuration.update", &out);
    assert_eq!(stored["storedRevision"], "cfg-1");
    assert_eq!(stored["effectiveGeneration"], "cfggen-1");
    let resolved = settings_get(root, INSTANCE).await;
    assert_eq!(value_of(&resolved, "theme"), Some(&json!("dark")));
    let theme = resolved["values"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["key"] == "theme")
        .unwrap()
        .clone();
    assert_eq!(theme["sourceScope"], json!({"kind": "instance"}));

    let reload_args = |key: &str| {
        vec![
            "settings".to_owned(),
            "reload".to_owned(),
            "--output".to_owned(),
            "json".to_owned(),
            "--scope".to_owned(),
            INSTANCE.to_owned(),
            "--expected-stored-revision".to_owned(),
            "cfg-1".to_owned(),
            "--request-key".to_owned(),
            key.to_owned(),
        ]
    };
    let refs = reload_args("j2-reload-1");
    let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
    let out = await_cli(root, &refs);
    let reload_receipt = expect_ok("configuration.reload", &out);
    assert_eq!(reload_receipt["kind"], "operation");
    assert_eq!(reload_receipt["state"], "succeeded");
    assert_eq!(reload_receipt["deduplicated"], false);
    let reload_operation_id = reload_receipt["operationId"].as_str().unwrap().to_owned();
    assert!(!reload_operation_id.is_empty());

    let refs = reload_args("j2-reload-1");
    let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
    let out = await_cli(root, &refs);
    let again = expect_ok("configuration.reload", &out);
    assert_eq!(again["operationId"], reload_operation_id.as_str());
    assert_eq!(again["deduplicated"], true);

    let out = await_cli(
        root,
        &[
            "requests",
            "lookup",
            "--output",
            "json",
            "--method-id",
            "configuration.reload",
            "--idempotency-key",
            "j2-reload-1",
        ],
    );
    let lookup = expect_ok("requests.lookup", &out);
    assert_eq!(lookup["requestState"], "accepted");
    assert_eq!(
        lookup["resource"]["operationId"],
        reload_operation_id.as_str()
    );

    // 业务落盘烟囱测试：整个旅程的状态都进了 <home>/v1/state.sqlite。
    assert!(
        root.join("v1").join("state.sqlite").is_file(),
        "the journey's business state must land in state.sqlite"
    );
}

/// 旅程 J2 故障注入：argv 明文秘密在出口前被拒；凭据禁用后探测 fail-closed
/// （零网络、AUTHENTICATION_FAILED）；重启用 retry 恢复且原失败记录冻结；
/// 过期 revision 的 set-enabled 被 CAS 拒绝。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journey_j2_fault_disabled_credential_fail_closed_and_recovery() {
    let home = TestHome::new("j2-e2e-fault");
    let root = home.path();
    init_home(root);

    let mock = MockProviderEndpoint::start(|_index, _path, _body| {
        (
            200,
            r#"{"choices":[{"message":{"content":"1"}}]}"#.to_owned(),
        )
    });

    // 供应商 + 凭据 + 模型（凭据经 stdin 写入并挂接到供应商）。
    let out = await_cli(
        root,
        &[
            "providers",
            "write",
            "--output",
            "json",
            "--provider-id",
            "prov-j2f",
            "--name",
            "J2 fault provider",
            "--protocol",
            "openai",
            "--endpoint",
            &mock.endpoint(),
        ],
    );
    let provider = expect_ok("provider.write", &out);
    let prov_rev1 = provider["provider"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();

    let secret = "sk-j2f-e2e-fault-secret-9c2a";
    let input = json!({
        "credentialId": "cred-j2f",
        "kind": "api_key",
        "name": "J2 fault key",
        "secret": secret,
    })
    .to_string();
    let out = cli_with_stdin(
        root,
        &[
            "credentials",
            "write",
            "--output",
            "json",
            "--input-json",
            "-",
        ],
        Some(&input),
    )
    .await;
    let created = expect_ok("credential.write", &out);
    let cred_rev1 = created["credential"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        !out.stdout.contains(secret) && !out.stderr.contains(secret),
        "the secret must never appear in stdout or stderr"
    );

    // ---- 故障 A：argv 明文秘密在请求离开 CLI 之前被拒（无泄漏）。
    let out = await_cli(
        root,
        &[
            "credentials",
            "write",
            "--output",
            "json",
            "--credential-id",
            "cred-argv",
            "--kind",
            "api_key",
            "--name",
            "argv",
            "--secret-file",
            secret,
        ],
    );
    assert_ne!(out.code, 0, "argv secret must be refused: {}", out.stdout);
    assert!(
        !out.stdout.contains(secret) && !out.stderr.contains(secret),
        "the refusal must echo the flag, never the value"
    );
    let combined = out.stdout + &out.stderr;
    assert!(
        combined.contains("secret"),
        "the refusal must name the secret policy: {combined}"
    );
    // 拒绝是出口前拦截：凭据清单里没有 cred-argv。
    let out = await_cli(root, &["credentials", "list", "--output", "json"]);
    let creds = expect_ok("credential.list", &out);
    let ids: Vec<&str> = creds["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["credentialId"].as_str())
        .collect();
    assert!(!ids.contains(&"cred-argv"));

    // 挂接凭据 + 登记模型。
    let out = await_cli(
        root,
        &[
            "providers",
            "write",
            "--output",
            "json",
            "--provider-id",
            "prov-j2f",
            "--expected-revision",
            &prov_rev1,
            "--name",
            "J2 fault provider",
            "--protocol",
            "openai",
            "--endpoint",
            &mock.endpoint(),
            "--credential-ref",
            "cred-j2f",
        ],
    );
    let attached = expect_ok("provider.write", &out);
    let prov_rev2 = attached["provider"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();

    let out = await_cli(
        root,
        &[
            "models",
            "write",
            "--output",
            "json",
            "--model",
            r#"{"providerId":"prov-j2f","modelId":"gpt-j2f"}"#,
            "--name",
            "GPT J2F",
        ],
    );
    expect_ok("model.write", &out);

    // ---- 故障 B：禁用凭据 → 网络探测 fail-closed（零网络、明确错误）。
    let out = await_cli(
        root,
        &[
            "credentials",
            "set-enabled",
            "--output",
            "json",
            "--credential-id",
            "cred-j2f",
            "--expected-revision",
            &cred_rev1,
            "--enabled",
            "false",
        ],
    );
    let disabled = expect_ok("credential.set_enabled", &out);
    assert_eq!(disabled["credential"]["enabled"], false);
    let cred_rev2 = disabled["credential"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();

    let targets = json!([{
        "targetId": "t-j2f",
        "providerId": "prov-j2f",
        "modelId": "gpt-j2f",
        "configRevision": prov_rev2,
        "testKinds": ["text"],
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
            "j2f-disabled-1",
        ],
    );
    let failed_receipt = expect_ok("provider_test.start", &out);
    let failed_operation_id = failed_receipt["operationId"].as_str().unwrap().to_owned();
    let failed_operation = wait_terminal_operation(root, &failed_operation_id);
    let failed_result = failed_operation["operation"]["result"].clone();
    let failed_revision = failed_operation["operation"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    let failed_target = &failed_result["targets"][0];
    assert_eq!(failed_target["state"], "failed");
    assert_eq!(failed_target["issues"][0]["code"], "AUTHENTICATION_FAILED");
    assert!(
        failed_target["issues"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("disabled")),
        "the issue must name the disabled credential: {failed_target}"
    );
    assert_eq!(
        failed_target["evidenceIds"].as_array().map(Vec::len),
        Some(0),
        "a failed probe produces no evidence"
    );
    assert_eq!(
        mock.requests().len(),
        0,
        "a disabled credential must fail closed before any network"
    );

    // ---- 故障 C：过期 revision 的 set-enabled 被 CAS 拒绝（凭据保持禁用）。
    let out = await_cli(
        root,
        &[
            "credentials",
            "set-enabled",
            "--output",
            "json",
            "--credential-id",
            "cred-j2f",
            "--expected-revision",
            &cred_rev1,
            "--enabled",
            "true",
        ],
    );
    let error = expect_failed("credential.set_enabled", &out);
    assert_eq!(error["code"], "REVISION_CONFLICT");

    // ---- 恢复：重新启用凭据，retry 落在健康路径上。
    let out = await_cli(
        root,
        &[
            "credentials",
            "set-enabled",
            "--output",
            "json",
            "--credential-id",
            "cred-j2f",
            "--expected-revision",
            &cred_rev2,
            "--enabled",
            "true",
        ],
    );
    let enabled = expect_ok("credential.set_enabled", &out);
    assert_eq!(enabled["credential"]["enabled"], true);

    let out = await_cli(
        root,
        &[
            "providers",
            "test",
            "retry",
            "--output",
            "json",
            "--source-operation-id",
            &failed_operation_id,
            "--target-ids",
            "t-j2f",
            "--probe-mode",
            "network",
            "--request-key",
            "j2f-retry-1",
        ],
    );
    let retry_receipt = expect_ok("provider_test.retry", &out);
    let retry_operation_id = retry_receipt["operationId"].as_str().unwrap().to_owned();
    assert_ne!(retry_operation_id, failed_operation_id);
    let retry_operation = wait_terminal_operation(root, &retry_operation_id);
    assert_eq!(retry_operation["operation"]["state"], "succeeded");
    assert_eq!(
        retry_operation["operation"]["result"]["sourceOperationId"],
        json!(failed_operation_id),
        "the retry keeps its source lineage"
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
    assert_eq!(
        mock.requests().len(),
        1,
        "exactly one more request for the retried probe"
    );

    // 原失败记录冻结（结果与 revision 都不动）。
    let frozen = wait_terminal_operation(root, &failed_operation_id);
    assert_eq!(frozen["operation"]["result"], failed_result);
    assert_eq!(frozen["operation"]["revision"], json!(failed_revision));

    // retry 的幂等回执可查。
    let out = await_cli(
        root,
        &[
            "requests",
            "lookup",
            "--output",
            "json",
            "--method-id",
            "provider_test.retry",
            "--idempotency-key",
            "j2f-retry-1",
        ],
    );
    let lookup = expect_ok("requests.lookup", &out);
    assert_eq!(lookup["requestState"], "accepted");
    assert_eq!(
        lookup["resource"]["operationId"],
        retry_operation_id.as_str()
    );
}
