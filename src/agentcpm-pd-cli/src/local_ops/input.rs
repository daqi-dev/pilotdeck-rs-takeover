//! `input.submit` — read one explicit input source and submit a single run
//! (`t007`, capability `cli.capability.default-input`).
//!
//! 合同正本 `state/cli-v1-operations.json#input.submit`:
//! - `source=argument` 必须携带 `text`；`source=stdinText` 禁止 `text`（oneOf，
//!   由内嵌 schema 校验强制）。
//! - 读到完整输入后**恰好提交一次**（EOF 提交完整输入）；空输入
//!   `INPUT_EMPTY`、超限 `INPUT_TOO_LARGE`（零提交）、非 UTF-8 `INVALID_UTF8`、
//!   一个 stdin 承担两种输入 `AMBIGUOUS_INPUT`。
//! - postcondition: 仅客户端读取并调用 `run.submit`，不造新的后端 stdin 入口。
//!   因此本操作是 CLI 本地组合：读输入 → 组装 run.submit 输入 → 经 `/v1/ws`
//!   调用 run.submit（runId 由目标 Gateway 分配）→ 把 run 回执映射为本操作
//!   result 的 `receipt`（RunAck）。输入准入（会话关闭 `SESSION_CLOSED`、队列
//!   暂停 `QUEUE_PAUSED` 等）完全复用 run.submit 的受理状态机，不重复实现。
//! - 从不读取任何确认提示（「无TTY不读确认」）：本 handler 只读业务输入本身。
//!
//! 与 TS 基线（`src/cli/sdkCli.ts` `readPrompt` / `CliChannel`）的承接：argv
//! 提示词与 stdin 读取后两端去空白（`argv.join(" ").trim()` /
//! `readFile("/dev/stdin","utf8").trim()`），内部空白保持原样。

use std::io::Write;

use agentcpm_pd_client::config::{load_client_config, resolve_pd_home};
use agentcpm_pd_client::error::ClientError;
use agentcpm_pd_client::targets::TargetStore;
use agentcpm_pd_client::{PdClient, PdClientOptions};
use agentcpm_pd_protocol::contract::{OPERATION_VERSION, OperationResponse, ResponseOutcome};
use agentcpm_pd_protocol::errors::{ContractError, EffectStatus, RetrySpec};
use agentcpm_pd_protocol::registry::OperationRegistry;
use agentcpm_pd_protocol::wire::MAX_FRAME_BYTES;
use serde_json::{Value, json};

use crate::commands::{local_request_id, write_envelope_or_fail, write_error};
use crate::local_ops::LocalOpRegistry;
use crate::{GlobalOptions, OutputFormat};

/// 合同声明而协议 crate 尚无常量的错误码（本操作自有）。
const CODE_INPUT_EMPTY: &str = "INPUT_EMPTY";
const CODE_INPUT_TOO_LARGE: &str = "INPUT_TOO_LARGE";
const CODE_INVALID_UTF8: &str = "INVALID_UTF8";
const CODE_AMBIGUOUS_INPUT: &str = "AMBIGUOUS_INPUT";
/// 合同 precondition:「显式JSON若另给targetId必须相同，否则派发前拒绝
/// TARGET_MISMATCH」——客户端侧（派发前）拒绝码。
const CODE_TARGET_MISMATCH: &str = "TARGET_MISMATCH";

/// 提交前的尺寸预算：`run.submit` 请求帧除业务输入外还有 JSON-RPC 包装、
/// `instanceId`/`idempotencyKey`（schema 上限各 256 字符）等字段。预留 1 KiB
/// 足以保证最终帧不超过 `MAX_FRAME_BYTES`（`system.capabilities` 报告的
/// `maxFrameBytes` 是本产品对外可见的输入上限）。
const ENVELOPE_ALLOWANCE: usize = 1024;

pub(super) fn register(reg: &mut LocalOpRegistry) {
    reg.op("input.submit", |globals, input, format, out| {
        Box::pin(op_input_submit(globals, input, format, out))
    });
}

fn input_error(code: &str, message: String) -> ClientError {
    ClientError::Contract(ContractError::new(
        code,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    ))
}

async fn op_input_submit(
    globals: &GlobalOptions,
    input: &Value,
    format: OutputFormat,
    out: &mut dyn Write,
) -> i32 {
    match input_submit(globals, input).await {
        Ok((outcome, result)) => {
            let resp = OperationResponse {
                version: OPERATION_VERSION.to_owned(),
                request_id: local_request_id(),
                outcome,
                result: Some(result),
                error: None,
            };
            write_envelope_or_fail(out, format, &resp)
        }
        Err(err) => write_error(out, format, &err),
    }
}

/// Validates the assembled input against the embedded operation schema
/// (`local_ops/schema.rs` precedent: local ops re-apply the validation the
/// gateway dispatch would have done).
fn validate_input(input: &Value) -> Result<(), ClientError> {
    OperationRegistry::global()
        .validate_input("input.submit", input)
        .map(|_| ())
        .map_err(|errs| {
            ClientError::Contract(
                agentcpm_pd_protocol::registry::validation_errors_to_contract_error(errs),
            )
        })
}

/// 派生字段注入：合同 required 里有 `targetId`，且命令树声明它「由全局
/// --target 所选 profile 派生」——typed-options 路径组装的输入不含它，由本
/// handler 在校验前填入（instanceId 在远程路径的等价物）。
fn input_with_derived_target(input: &Value, target_name: &str) -> Value {
    let mut enriched = input.clone();
    if let Some(obj) = enriched.as_object_mut() {
        obj.insert("targetId".to_owned(), json!(target_name));
    }
    enriched
}

async fn input_submit(
    globals: &GlobalOptions,
    input: &Value,
) -> Result<(ResponseOutcome, Value), ClientError> {
    // 派发前 targetId 核对（合同 precondition 3）：显式 JSON 携带的 targetId
    // 必须与全局 --target 所选 profile 一致；一致或缺省则派生填入。
    let home = resolve_pd_home(None)?;
    let target_name = resolved_target_name(&home, globals.target.as_deref())?;
    ensure_target_id(input, &target_name)?;
    let input = input_with_derived_target(input, &target_name);

    // local 操作不经 Gateway dispatch，按 targets.rs 先例重放 schema 校验
    // （含 source/text 的 oneOf 互斥）。
    validate_input(&input)?;

    let session_id = input
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let source = input
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    // 读取输入源。
    let raw_text = match source.as_str() {
        "argument" => input
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        "stdinText" => {
            // 一个 stdin 只能承担一种输入：`--input-json -` 已经消费了 stdin，
            // 再按 stdinText 读就是双载。
            if globals.input_json.as_deref() == Some("-") {
                return Err(input_error(
                    CODE_AMBIGUOUS_INPUT,
                    "stdin cannot serve both `--input-json -` and `source=stdinText`; pass one input through stdin".to_owned(),
                ));
            }
            read_stdin_text().await?
        }
        other => {
            return Err(input_error(
                agentcpm_pd_protocol::errors::CODE_INVALID_ARGUMENT,
                format!("unsupported source `{other}`"),
            ));
        }
    };
    // 与 TS 基线承接：两端去空白；空输入零提交。
    let text = normalize_text(&raw_text);
    if text.is_empty() {
        return Err(input_error(
            CODE_INPUT_EMPTY,
            format!("the {source} input is empty; nothing was submitted"),
        ));
    }
    ensure_submit_size(&session_id, text)?;

    // 组装 run.submit 输入并经 /v1/ws 提交（instanceId 由客户端按
    // envelope.target 注入；幂等键按 idempotentSubmit 生成并持久化）。
    let run_input = json!({
        "sessionId": session_id,
        "input": [{"kind": "text", "text": text}],
    });
    let client = PdClient::connect(PdClientOptions {
        home_override: None,
        target: globals.target.clone(),
        no_start: globals.no_start,
        spawner: None,
    })
    .await?;
    let response = client
        .invoke(
            "run.submit",
            run_input,
            globals.request_key.as_deref(),
            None,
            globals.timeout_ms.map(std::time::Duration::from_millis),
        )
        .await;
    let _ = client.close().await;
    let response = response?;

    // 把 run.submit 的 receipt 映射为 input.submit result 的 RunAck。run.submit
    // 受理即持久（`durable=true`），故 `accepted` 恒为 "durable"；`state` /
    // `acceptedRevision` 原样透传。queuePosition 为可选字段：run.submit 回执
    // 不携带队列位置，且本地无权威值可查，省略而不是编造。
    let receipt = response.result.clone().unwrap_or(Value::Null);
    let run_ack = json!({
        "runId": receipt.get("runId").cloned().unwrap_or(Value::Null),
        "sessionId": session_id,
        "accepted": "durable",
        "state": receipt.get("state").cloned().unwrap_or(Value::Null),
        "acceptedRevision": receipt.get("acceptedRevision").cloned().unwrap_or(Value::Null),
    });
    let result = json!({"receipt": run_ack});
    // 组装方对 result 负责：交给内嵌 schema 校验把关（e2e expect_envelope 会
    // 再校验一次）。
    if let Err(errs) = OperationRegistry::global().validate_result("input.submit", &result) {
        return Err(ClientError::Contract(
            agentcpm_pd_protocol::registry::validation_errors_to_contract_error(errs),
        ));
    }
    Ok((response.outcome, result))
}

/// 两端去空白（TS 基线 `argv.join(" ").trim()` / `readFile(...).trim()` 的
/// 承接），内部空白原样保留。
fn normalize_text(raw: &str) -> &str {
    raw.trim()
}

/// Reads stdin to EOF as raw bytes, bounded by the frame cap: an oversized
/// stdin is `INPUT_TOO_LARGE` with zero submission (never an unbounded read).
async fn read_stdin_text() -> Result<String, ClientError> {
    use tokio::io::AsyncReadExt;
    let cap = MAX_FRAME_BYTES as u64 + 1;
    let mut buf = Vec::new();
    let mut stdin = tokio::io::stdin().take(cap);
    stdin
        .read_to_end(&mut buf)
        .await
        .map_err(|e| ClientError::usage(format!("failed to read stdin: {e}")))?;
    text_from_stdin_bytes(buf)
}

/// UTF-8 校验（`INVALID_UTF8`）与超限拒绝（`INPUT_TOO_LARGE`），纯函数便于
/// 单元测试。
fn text_from_stdin_bytes(bytes: Vec<u8>) -> Result<String, ClientError> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(input_error(
            CODE_INPUT_TOO_LARGE,
            format!(
                "stdin input is {} bytes and exceeds the {MAX_FRAME_BYTES}-byte frame cap; nothing was submitted",
                bytes.len(),
            ),
        ));
    }
    String::from_utf8(bytes).map_err(|_| {
        input_error(
            CODE_INVALID_UTF8,
            "stdin is not valid UTF-8 (encoding `utf8` is the only supported encoding); nothing was submitted".to_owned(),
        )
    })
}

/// Zero-submission size gate: the assembled `run.submit` frame (business input
/// plus envelope fields) must stay within `MAX_FRAME_BYTES`.
fn ensure_submit_size(session_id: &str, text: &str) -> Result<(), ClientError> {
    let probe = json!({
        "sessionId": session_id,
        "input": [{"kind": "text", "text": text}],
    });
    let serialized = serde_json::to_string(&probe).unwrap_or_default();
    if serialized.len() + ENVELOPE_ALLOWANCE > MAX_FRAME_BYTES {
        return Err(input_error(
            CODE_INPUT_TOO_LARGE,
            format!(
                "input is {} bytes; with the request envelope it exceeds the {MAX_FRAME_BYTES}-byte frame cap; nothing was submitted",
                text.len(),
            ),
        ));
    }
    Ok(())
}

/// 显式 `targetId` 与所选 profile 不一致时派发前拒绝（TARGET_MISMATCH）。
///
/// `pub(crate)`（t010 seam）：`input.follow` 复用同一 target 解析与核对，
/// 保持单源（除可见性外无任何行为变化）。
pub(crate) fn ensure_target_id(input: &Value, resolved_name: &str) -> Result<(), ClientError> {
    let Some(explicit) = input.get("targetId").and_then(Value::as_str) else {
        return Ok(());
    };
    if explicit != resolved_name {
        return Err(input_error(
            CODE_TARGET_MISMATCH,
            format!(
                "input targetId `{explicit}` does not match the selected target profile `{resolved_name}`"
            ),
        ));
    }
    Ok(())
}

/// 解析当前调用所选 target profile 名，与 `agentcpm_pd_client::config::
/// resolve_target` 的名字解析链保持一致：显式 `--target` → `config.json`
/// `defaultTarget` → 目标档案库 `target.select` 的选择 → `"local"`。
///
/// `pub(crate)`（t010 seam）：`input.follow` 复用同一解析链，保持单源
/// （除可见性外无任何行为变化）。
pub(crate) fn resolved_target_name(
    home: &std::path::Path,
    explicit: Option<&str>,
) -> Result<String, ClientError> {
    if let Some(name) = explicit.map(str::trim).filter(|t| !t.is_empty()) {
        return Ok(name.to_owned());
    }
    let cfg = load_client_config(home)?;
    if let Some(default) = cfg.default_target {
        return Ok(default);
    }
    if let Some(selected) = TargetStore::open(home).selected().ok().flatten() {
        return Ok(selected);
    }
    Ok("local".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_trims_both_ends_and_keeps_interior_whitespace() {
        assert_eq!(normalize_text("  hello world  "), "hello world");
        assert_eq!(normalize_text("line one\nline two\n"), "line one\nline two");
        assert_eq!(normalize_text(""), "");
        assert_eq!(normalize_text(" \n\t "), "");
    }

    #[test]
    fn stdin_bytes_validate_utf8_and_size_before_any_submission() {
        assert_eq!(text_from_stdin_bytes(b"hello".to_vec()).unwrap(), "hello");
        let bad = b"\xff\xfe not utf8".to_vec();
        let err = text_from_stdin_bytes(bad).unwrap_err();
        let contract = err.into_contract_error();
        assert_eq!(contract.code, CODE_INVALID_UTF8);
        assert_eq!(contract.effect, EffectStatus::NotApplied);

        let oversized = vec![b'x'; MAX_FRAME_BYTES + 1];
        let err = text_from_stdin_bytes(oversized).unwrap_err();
        let contract = err.into_contract_error();
        assert_eq!(contract.code, CODE_INPUT_TOO_LARGE);
        assert_eq!(contract.effect, EffectStatus::NotApplied);
    }

    #[test]
    fn size_gate_rejects_frames_that_cannot_fit_the_cap() {
        // 留足余量的正常提交。
        ensure_submit_size("s-1", "hello").unwrap();
        // 业务输入 + 包装 + 预留超过 1 MiB → INPUT_TOO_LARGE（零提交）。
        let big = "x".repeat(MAX_FRAME_BYTES - ENVELOPE_ALLOWANCE + 8);
        let err = ensure_submit_size("s-1", &big).unwrap_err();
        let contract = err.into_contract_error();
        assert_eq!(contract.code, CODE_INPUT_TOO_LARGE);
        assert_eq!(contract.effect, EffectStatus::NotApplied);
    }

    #[test]
    fn explicit_target_id_must_match_the_selected_profile() {
        assert!(ensure_target_id(&json!({}), "local").is_ok());
        assert!(ensure_target_id(&json!({"targetId": "local"}), "local").is_ok());
        let err = ensure_target_id(&json!({"targetId": "remote-a"}), "local").unwrap_err();
        let contract = err.into_contract_error();
        assert_eq!(contract.code, CODE_TARGET_MISMATCH);
        assert_eq!(contract.effect, EffectStatus::NotApplied);
    }

    #[test]
    fn schema_oneof_rejects_source_text_combinations() {
        // 校验前先填入派生 targetId（required 字段）。
        let enriched = |body: Value| input_with_derived_target(&body, "local");
        // argument 无 text / stdinText 带 text 都过不了 oneOf。
        assert!(
            validate_input(&enriched(json!({"sessionId": "s", "source": "argument"}))).is_err()
        );
        assert!(
            validate_input(&enriched(
                json!({"sessionId": "s", "source": "stdinText", "text": "x"})
            ))
            .is_err()
        );
        assert!(
            validate_input(&enriched(
                json!({"sessionId": "s", "source": "argument", "text": "x"})
            ))
            .is_ok()
        );
        assert!(
            validate_input(&enriched(json!({"sessionId": "s", "source": "stdinText"}))).is_ok()
        );
        // 未注入 targetId 的原始输入过不了 required。
        assert!(validate_input(&json!({"sessionId": "s", "source": "stdinText"})).is_err());
    }
}
