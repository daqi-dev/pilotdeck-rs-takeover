//! `input.follow` — 非 TUI 逐行多轮输入编排（`t010`，local）。
//!
//! 合同正本 `state/cli-v1-operations.json#input.follow`
//! （`execution='local'` / `retry='nonReplayable'` / `caller='local_os_owner'`）：
//!
//! - 读 stdin 的行（`mode=lines`）或 NDJSON 记录（`mode=ndjson`），**在同一
//!   明确会话中顺序提交每行并等待本轮终态**（来源审计
//!   `cli.compat.default-stdin` 的 newOperations：「在同一明确会话中顺序提交
//!   每行并等待本轮终态」），逐项调用 `run.submit`——postcondition「仅客户端
//!   读取并调用 run.submit，不造新的后端 stdin 入口」。
//! - 每项一个 `InputFrame`（`accepted`/`error`），EOF 后一个 `end` 帧；帧序
//!   `sequence` 从 0 起，逐帧递增。
//! - **幂等与恢复**：逐项 idempotencyKey 在提交**前**持久化（本文件的批次
//!   日志 + 客户端幂等账本 `client/idempotency.json` 双层），NDJSON 记录可
//!   显式给 `idempotencyKey` 以接续，缺省由 CLI 生成；未知结果（RESULT_UNKNOWN
//!   或提交途中连接断开）**始终停止后续输入**（acceptance「未知提交始终停止
//!   后续输入」），恢复依赖每项回执经 `requests.lookup` 查询，不盲重放为新
//!   key——本操作不自动查询，停止即止。
//! - **acceptance**：EOF 无末换行的最后行仅提交一次（`next_line` 语义保证）；
//!   空行（两端去空白后为空）`INPUT_EMPTY`（注意：TS 基线 `CliChannel`
//!   跳过空行，合同把它改为显式错误——按合同）；`Ctrl-C` 只停止读取（SIGINT
//!   被捕获后发出 `end` 帧并以 130 退出），从不取消已受理 run。
//! - 帧流输出为 NDJSON（`--output json` 是用法错误——stream 操作同一规则）；
//!   `--request-key` 被拒绝：单一键会让所有行去重成同一个 run。
//!
//! ## 合同未明确处的实现选择（显式登记，供集成者复核）
//!
//! 1. **帧的 envelope outcome**：每一帧装在一个 `outcome=succeeded` 的
//!    envelope 里（本操作产出该帧即成功；逐项失败是 `type=error` 帧的
//!    数据，不是 envelope 失败）。连接建立失败等无帧可发的灾难性失败按
//!    惯例写单个 failed envelope。
//! 2. **等待本轮终态失败**：提交成功但 run.wait 失败（连接断开等）→ 发
//!    `error` 帧并停止（已受理 run 不受影响，键已在账本中）。
//! 3. **退出码**：干净 EOF=0；出现过任何 error 帧=1；因未知结果停止=3；
//!    SIGINT=130。
//! 4. **NDJSON 记录形态**：`{"input": [<parts>], "idempotencyKey": "<可选>"}`；
//!    其余字段不接受（sessionId 是操作级参数）。非对象/非 JSON/缺
//!    `input`/`idempotencyKey` 非字符串 → `MALFORMED_FRAME`。`input` 的
//!    parts 形状交给 run.submit 的 schema 校验（INVALID_ARGUMENT 逐项报）。
//! 5. **批次日志**：`<home>/client/input-follow/<batch>.ndjson`，提交前一行
//!    `{"batch","sequence","idempotencyKey","state":"submitted"}`，结果后
//!    一行 accepted/error 追记；不落业务正文（隐私）。

use std::io::Write;
use std::time::Duration;

use agentcpm_pd_client::config::resolve_pd_home;
use agentcpm_pd_client::error::ClientError;
use agentcpm_pd_client::{PdClient, PdClientOptions};
use agentcpm_pd_protocol::contract::{OPERATION_VERSION, OperationResponse, ResponseOutcome};
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, ContractError, EXIT_FAILED, EXIT_INTERRUPTED, EXIT_RESULT_UNKNOWN,
    EXIT_USAGE, EffectStatus, RetrySpec,
};
use agentcpm_pd_protocol::id::{IdGenerator, NodeId};
use agentcpm_pd_protocol::registry::OperationRegistry;
use agentcpm_pd_protocol::wire::MAX_FRAME_BYTES;
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;

use crate::commands::{local_request_id, write_envelope_or_fail, write_error};
use crate::local_ops::LocalOpRegistry;
use crate::local_ops::input::{ensure_target_id, resolved_target_name};
use crate::{BIN_NAME, GlobalOptions, OutputFormat};

/// 合同声明而协议 crate 尚无常量的错误码。
const CODE_INPUT_EMPTY: &str = "INPUT_EMPTY";
const CODE_INPUT_TOO_LARGE: &str = "INPUT_TOO_LARGE";
const CODE_INVALID_UTF8: &str = "INVALID_UTF8";
const CODE_MALFORMED_FRAME: &str = "MALFORMED_FRAME";
/// 提交前的尺寸预算（与 `input.submit` 的 ENVELOPE_ALLOWANCE 同一理由）。
const ENVELOPE_ALLOWANCE: usize = 1024;
/// 单次 run.wait 的观察窗；客户端请求限期留 5 s 余量。
const WAIT_CHUNK_MS: u64 = 25_000;
const WAIT_CALL_TIMEOUT: Duration = Duration::from_millis(30_000);

pub(super) fn register(reg: &mut LocalOpRegistry) {
    reg.op("input.follow", |globals, input, _unary_format, out| {
        Box::pin(op_input_follow(globals, input, out))
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

/// 一个被解析好的待提交项。
struct FollowItem {
    parts: Value,
    explicit_key: Option<String>,
}

/// 未知提交的判据：合同 RESULT_UNKNOWN、效果未知，或提交途中连接断开
/// （受理与否不可判定——账本中该键保持 pending，恢复经 requests.lookup）。
fn is_unknown_submission(err: &ClientError) -> bool {
    err.is_result_unknown() || matches!(err, ClientError::ConnectionFailed(_))
}

fn issue_from(err: &ClientError) -> Value {
    let contract = err.into_contract_error();
    json!({"code": contract.code, "message": contract.message})
}

async fn op_input_follow(globals: &GlobalOptions, input: &Value, out: &mut dyn Write) -> i32 {
    // 帧流形态：NDJSON（--output json 是用法错误，stream 先例）。
    let format = match OutputFormat::resolve_stream(globals.output) {
        Ok(format) => format,
        Err(message) => {
            eprintln!("{BIN_NAME}: usage error: {message}");
            return EXIT_USAGE;
        }
    };
    // 单一 --request-key 会让所有行去重到同一个 run：拒绝。
    if globals.request_key.is_some() {
        eprintln!(
            "{BIN_NAME}: usage error: `--request-key` cannot be combined with `input follow`; \
             per-item keys are generated (or provided per NDJSON record)"
        );
        return EXIT_USAGE;
    }
    // stdin 只能承担一种输入（--input-json - 已消费 stdin）。
    if globals.input_json.as_deref() == Some("-") {
        let err = input_error(
            CODE_INVALID_ARGUMENT,
            "stdin cannot serve both `--input-json -` and the `input follow` stream; \
             pass one input through stdin"
                .to_owned(),
        );
        return write_error(out, format, &err);
    }

    match input_follow(globals, input, format, out).await {
        Ok(code) => code,
        Err(err) => write_error(out, format, &err),
    }
}

async fn input_follow(
    globals: &GlobalOptions,
    input: &Value,
    format: OutputFormat,
    out: &mut dyn Write,
) -> Result<i32, ClientError> {
    // 派发前 targetId 核对 + 派生注入（input.submit 同一前置）。
    let home = resolve_pd_home(None)?;
    let target_name = resolved_target_name(&home, globals.target.as_deref())?;
    ensure_target_id(input, &target_name)?;
    let mut enriched = input.clone();
    if let Some(obj) = enriched.as_object_mut() {
        obj.insert("targetId".to_owned(), json!(target_name));
    }
    // local 操作不经 Gateway dispatch，重放 schema 校验（local_ops 先例）。
    OperationRegistry::global()
        .validate_input("input.follow", &enriched)
        .map_err(|errs| {
            ClientError::Contract(
                agentcpm_pd_protocol::registry::validation_errors_to_contract_error(errs),
            )
        })?;

    let session_id = enriched
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let mode = enriched
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("lines")
        .to_owned();
    let continue_on_error = enriched
        .get("continueOnError")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let client = PdClient::connect(PdClientOptions {
        home_override: None,
        target: globals.target.clone(),
        no_start: globals.no_start,
        spawner: None,
    })
    .await?;

    // 批次日志 + 逐项键生成器。
    let batch_id = local_request_id();
    let journal = FollowJournal::create(&home, &batch_id)?;
    let key_gen = IdGenerator::new(NodeId::local());

    // 读任务独占 stdin（select 中取消安全），行经 channel 送达。
    let (tx, mut rx) = mpsc::channel::<Result<String, std::io::Error>>(16);
    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        loop {
            match reader.next_line().await {
                // EOF：channel 关闭即 EOF 信号；无末换行的最后行已被产出一次。
                Ok(Some(line)) => {
                    if tx.send(Ok(line)).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    break;
                }
            }
        }
    });

    let mut sequence: u64 = 0;
    let mut any_error = false;
    let mut stopped_unknown = false;
    let mut interrupted = false;

    'main: loop {
        let line = tokio::select! {
            received = rx.recv() => match received {
                None => break, // EOF
                Some(Ok(line)) => line,
                Some(Err(e)) => {
                    let code = if e.kind() == std::io::ErrorKind::InvalidData {
                        CODE_INVALID_UTF8
                    } else {
                        agentcpm_pd_protocol::errors::CODE_UNAVAILABLE
                    };
                    let err = input_error(
                        code,
                        format!("failed to read the follow input stream: {e}"),
                    );
                    write_frame(out, format, &mut sequence, frame_error("", &err))?;
                    any_error = true;
                    break;
                }
            },
            _ = tokio::signal::ctrl_c() => {
                // Ctrl-C：只停止读取；已受理 run 继续（acceptance）。
                interrupted = true;
                break;
            }
        };

        let item = match parse_item(&mode, &line, &session_id) {
            Ok(item) => item,
            Err(err) => {
                write_frame(out, format, &mut sequence, frame_error("", &err))?;
                any_error = true;
                if !continue_on_error {
                    break;
                }
                continue;
            }
        };

        // 逐项键：NDJSON 显式或 CLI 生成；提交前持久化（恢复依赖）。
        // item_seq 固定为该项的帧序（write_frame 会推进 sequence）。
        let item_seq = sequence;
        let key = item
            .explicit_key
            .clone()
            .unwrap_or_else(|| key_gen.next_id());
        journal.append_submitted(item_seq, &key)?;

        let submit = async {
            let run_input = json!({"sessionId": session_id, "input": item.parts});
            client
                .invoke(
                    "run.submit",
                    run_input,
                    Some(&key),
                    None,
                    globals.timeout_ms.map(Duration::from_millis),
                )
                .await
        };
        // 提交也在 Ctrl-C 的可中断窗口内（中断不取消、只放弃观察）。
        let submitted = tokio::select! {
            result = submit => result,
            _ = tokio::signal::ctrl_c() => {
                interrupted = true;
                break 'main;
            }
        };

        match submitted {
            Ok(response) => {
                let receipt = response.result.clone().unwrap_or(Value::Null);
                let run_id = receipt
                    .get("runId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let ack = run_ack(&receipt, &session_id);
                write_frame(out, format, &mut sequence, frame_accepted(&run_id, &ack))?;
                journal.append_outcome(item_seq, &key, "accepted", Some(&run_id))?;

                // 等待本轮终态（同一明确会话的串行多轮）。
                let wait = async {
                    loop {
                        let waited = client
                            .invoke(
                                "run.wait",
                                json!({"runId": run_id, "timeoutMs": WAIT_CHUNK_MS}),
                                None,
                                None,
                                Some(WAIT_CALL_TIMEOUT),
                            )
                            .await?;
                        let result = waited.result.clone().unwrap_or(Value::Null);
                        if result["conditionMet"] == true {
                            return Ok(());
                        }
                    }
                };
                let waited = tokio::select! {
                    result = wait => result,
                    _ = tokio::signal::ctrl_c() => {
                        interrupted = true;
                        break 'main;
                    }
                };
                if let Err(err) = waited {
                    write_frame(out, format, &mut sequence, frame_error(&run_id, &err))?;
                    journal.append_outcome(item_seq, &key, "wait_error", None)?;
                    any_error = true;
                    break;
                }
            }
            Err(err) => {
                let unknown = is_unknown_submission(&err);
                write_frame(out, format, &mut sequence, frame_error("", &err))?;
                journal.append_outcome(
                    item_seq,
                    &key,
                    if unknown { "unknown" } else { "error" },
                    None,
                )?;
                any_error = true;
                if unknown {
                    stopped_unknown = true;
                    break;
                }
                if !continue_on_error {
                    break;
                }
            }
        }
    }

    let _ = client.close().await;
    write_frame(out, format, &mut sequence, frame_end())?;
    if interrupted {
        return Ok(EXIT_INTERRUPTED);
    }
    if stopped_unknown {
        return Ok(EXIT_RESULT_UNKNOWN);
    }
    if any_error {
        return Ok(EXIT_FAILED);
    }
    Ok(0)
}

/// 解析一行输入为待提交项（空行/坏帧是逐项错误，不是致命错误）。
fn parse_item(mode: &str, line: &str, session_id: &str) -> Result<FollowItem, ClientError> {
    match mode {
        "ndjson" => {
            let value: Value = serde_json::from_str(line).map_err(|e| {
                input_error(
                    CODE_MALFORMED_FRAME,
                    format!("NDJSON record is not valid JSON: {e}"),
                )
            })?;
            let Some(obj) = value.as_object() else {
                return Err(input_error(
                    CODE_MALFORMED_FRAME,
                    "NDJSON record must be a JSON object".to_owned(),
                ));
            };
            let parts = match obj.get("input") {
                Some(parts) if parts.is_array() => parts.clone(),
                _ => {
                    return Err(input_error(
                        CODE_MALFORMED_FRAME,
                        "NDJSON record must carry an `input` array of parts".to_owned(),
                    ));
                }
            };
            let explicit_key = match obj.get("idempotencyKey") {
                None => None,
                Some(Value::String(key)) => Some(key.clone()),
                Some(_) => {
                    return Err(input_error(
                        CODE_MALFORMED_FRAME,
                        "`idempotencyKey` must be a string when present".to_owned(),
                    ));
                }
            };
            ensure_parts_size(session_id, &parts)?;
            Ok(FollowItem {
                parts,
                explicit_key,
            })
        }
        // lines：两端去空白（TS 基线 `readPrompt` 的 line.trim() 承接），
        // 内部空白保留；空行 → INPUT_EMPTY。
        _ => {
            let text = line.trim();
            if text.is_empty() {
                return Err(input_error(
                    CODE_INPUT_EMPTY,
                    "the input line is empty; nothing was submitted".to_owned(),
                ));
            }
            let parts = json!([{"kind": "text", "text": text}]);
            ensure_parts_size(session_id, &parts)?;
            Ok(FollowItem {
                parts,
                explicit_key: None,
            })
        }
    }
}

/// 单项尺寸闸（与 input.submit 同一预算：业务输入 + 包装不得超过帧上限）。
/// lines 与 ndjson 两种形态同一闸（提交前零副作用拒绝）。
fn ensure_parts_size(session_id: &str, parts: &Value) -> Result<(), ClientError> {
    let probe = json!({
        "sessionId": session_id,
        "input": parts,
    });
    let serialized = serde_json::to_string(&probe).unwrap_or_default();
    if serialized.len() + ENVELOPE_ALLOWANCE > MAX_FRAME_BYTES {
        return Err(input_error(
            CODE_INPUT_TOO_LARGE,
            format!(
                "input record is {} bytes; with the request envelope it exceeds the \
                 {MAX_FRAME_BYTES}-byte frame cap; nothing was submitted",
                serialized.len()
            ),
        ));
    }
    Ok(())
}

/// run.submit 回执 → RunAck（`accepted` 恒为 durable；queuePosition 无本地
/// 权威值，省略而非编造——input.submit 同一投影）。
fn run_ack(receipt: &Value, session_id: &str) -> Value {
    json!({
        "runId": receipt.get("runId").cloned().unwrap_or(Value::Null),
        "sessionId": session_id,
        "accepted": "durable",
        "state": receipt.get("state").cloned().unwrap_or(Value::Null),
        "acceptedRevision": receipt.get("acceptedRevision").cloned().unwrap_or(Value::Null),
    })
}

fn frame_accepted(run_id: &str, receipt: &Value) -> Value {
    let run_id = if run_id.is_empty() {
        Value::Null
    } else {
        json!(run_id)
    };
    json!({
        "sequence": 0,
        "runId": run_id,
        "type": "accepted",
        "receipt": receipt,
        "error": null,
    })
}

fn frame_error(run_id: &str, err: &ClientError) -> Value {
    let run_id = if run_id.is_empty() {
        Value::Null
    } else {
        json!(run_id)
    };
    json!({
        "sequence": 0,
        "runId": run_id,
        "type": "error",
        "receipt": null,
        "error": issue_from(err),
    })
}

fn frame_end() -> Value {
    json!({
        "sequence": 0,
        "runId": null,
        "type": "end",
        "receipt": null,
        "error": null,
    })
}

/// 写一帧（独立 envelope），推进 sequence。写失败按 P2-11 响亮退出。
fn write_frame(
    out: &mut dyn Write,
    format: OutputFormat,
    sequence: &mut u64,
    mut frame: Value,
) -> Result<(), ClientError> {
    if let Some(obj) = frame.as_object_mut() {
        obj.insert("sequence".to_owned(), json!(*sequence));
    }
    *sequence += 1;
    let resp = OperationResponse {
        version: OPERATION_VERSION.to_owned(),
        request_id: local_request_id(),
        outcome: ResponseOutcome::Succeeded,
        result: Some(json!({"frame": frame})),
        error: None,
    };
    let code = write_envelope_or_fail(out, format, &resp);
    if code != 0 {
        return Err(ClientError::usage(
            "failed to write an input.follow frame envelope to stdout",
        ));
    }
    Ok(())
}

/// 批次日志：`<home>/client/input-follow/<batch>.ndjson`（append + flush）。
struct FollowJournal {
    file: std::fs::File,
    batch: String,
}

impl FollowJournal {
    fn create(home: &std::path::Path, batch: &str) -> Result<Self, ClientError> {
        let dir = home.join("client").join("input-follow");
        std::fs::create_dir_all(&dir).map_err(|e| {
            ClientError::connection_failed(format!("failed to create {}: {e}", dir.display()))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("{batch}.ndjson")))
            .map_err(|e| {
                ClientError::connection_failed(format!(
                    "failed to open the input-follow journal: {e}"
                ))
            })?;
        Ok(Self {
            file,
            batch: batch.to_owned(),
        })
    }

    fn append_submitted(&self, sequence: u64, key: &str) -> Result<(), ClientError> {
        self.append(&json!({
            "batch": self.batch,
            "sequence": sequence,
            "idempotencyKey": key,
            "state": "submitted",
        }))
    }

    fn append_outcome(
        &self,
        sequence: u64,
        key: &str,
        state: &str,
        run_id: Option<&str>,
    ) -> Result<(), ClientError> {
        self.append(&json!({
            "batch": self.batch,
            "sequence": sequence,
            "idempotencyKey": key,
            "state": state,
            "runId": run_id,
        }))
    }

    fn append(&self, record: &Value) -> Result<(), ClientError> {
        let mut file = &self.file;
        writeln!(file, "{}", record).map_err(|e| {
            ClientError::connection_failed(format!(
                "failed to persist the input-follow journal: {e}"
            ))
        })?;
        file.flush().map_err(|e| {
            ClientError::connection_failed(format!("failed to flush the input-follow journal: {e}"))
        })?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests（纯函数面；连接/网关路径在 e2e）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn contract_code(err: &ClientError) -> String {
        err.into_contract_error().code
    }

    #[test]
    fn frames_match_the_contract_result_schema() {
        let accepted = frame_accepted(
            "run-1",
            &json!({
                "runId": "run-1", "kind": "run", "durable": true,
                "acceptedRevision": "rev-1", "deduplicated": false, "state": "queued",
            }),
        );
        OperationRegistry::global()
            .validate_result("input.follow", &json!({"frame": accepted}))
            .expect("accepted frame matches the schema");
        let error = frame_error("", &input_error(CODE_INPUT_EMPTY, "empty".to_owned()));
        OperationRegistry::global()
            .validate_result("input.follow", &json!({"frame": error}))
            .expect("error frame matches the schema");
        let end = frame_end();
        OperationRegistry::global()
            .validate_result("input.follow", &json!({"frame": end}))
            .expect("end frame matches the schema");
    }

    #[test]
    fn lines_mode_trims_and_rejects_blank_lines() {
        let item = parse_item("lines", "  hello world  ", "s-1").unwrap();
        assert_eq!(item.parts[0]["text"], "hello world");
        // 内部空白保留。
        let item = parse_item("lines", "line one\n\t line two ", "s-1").unwrap();
        assert_eq!(item.parts[0]["text"], "line one\n\t line two");
        assert!(item.explicit_key.is_none());
        let err = parse_item("lines", "   \t ", "s-1").unwrap_err();
        assert_eq!(contract_code(&err), CODE_INPUT_EMPTY);
        assert_eq!(
            err.into_contract_error().effect,
            EffectStatus::NotApplied,
            "空行零提交"
        );
    }

    #[test]
    fn ndjson_mode_parses_records_and_flags_malformed_frames() {
        let item = parse_item(
            "ndjson",
            r#"{"input": [{"kind": "text", "text": "hi"}], "idempotencyKey": "k-1"}"#,
            "s-1",
        )
        .unwrap();
        assert_eq!(item.parts[0]["text"], "hi");
        assert_eq!(item.explicit_key.as_deref(), Some("k-1"));

        for bad in [
            "not json",
            "",
            r#""a string""#,
            r#"{"noinput": 1}"#,
            r#"{"input": "not-an-array"}"#,
            r#"{"input": [], "idempotencyKey": 7}"#,
        ] {
            let err = parse_item("ndjson", bad, "s-1").unwrap_err();
            assert_eq!(
                contract_code(&err),
                CODE_MALFORMED_FRAME,
                "record `{bad}` must be MALFORMED_FRAME"
            );
        }
        // input 的 parts 形状交给 run.submit 校验（这里是合法帧、非法业务输入）。
        assert!(parse_item("ndjson", r#"{"input": []}"#, "s-1").is_ok());
    }

    #[test]
    fn size_gate_rejects_frames_that_cannot_fit() {
        let small = json!([{"kind": "text", "text": "hello"}]);
        ensure_parts_size("s-1", &small).unwrap();
        let big = "x".repeat(MAX_FRAME_BYTES - ENVELOPE_ALLOWANCE + 8);
        let parts = json!([{"kind": "text", "text": big}]);
        let err = ensure_parts_size("s-1", &parts).unwrap_err();
        assert_eq!(contract_code(&err), CODE_INPUT_TOO_LARGE);
        // ndjson 记录同样过闸。
        let err = parse_item(
            "ndjson",
            &format!(
                "{{\"input\": [{{\"kind\": \"text\", \"text\": \"{}\"}}]}}",
                big
            ),
            "s-1",
        )
        .unwrap_err();
        assert_eq!(contract_code(&err), CODE_INPUT_TOO_LARGE);
    }

    #[test]
    fn unknown_submissions_are_classified_for_the_hard_stop() {
        let unknown = ClientError::Contract(ContractError::new(
            agentcpm_pd_protocol::errors::CODE_RESULT_UNKNOWN,
            "unknown".to_owned(),
            RetrySpec::never(),
            EffectStatus::Unknown,
        ));
        assert!(is_unknown_submission(&unknown));
        assert!(is_unknown_submission(&ClientError::connection_failed(
            "dropped"
        )));
        // 已知拒绝不触发硬停（continueOnError 可继续）。
        let known = input_error(CODE_INPUT_EMPTY, "x".to_owned());
        assert!(!is_unknown_submission(&known));
    }

    #[test]
    fn journal_persists_batch_and_keys() {
        let dir = std::env::temp_dir().join(format!(
            "input-follow-journal-{}-{}",
            std::process::id(),
            local_request_id()
        ));
        let journal = FollowJournal::create(&dir, "batch-x").unwrap();
        journal.append_submitted(0, "key-0").unwrap();
        journal
            .append_outcome(0, "key-0", "accepted", Some("run-0"))
            .unwrap();
        let raw = std::fs::read_to_string(
            dir.join("client")
                .join("input-follow")
                .join("batch-x.ndjson"),
        )
        .unwrap();
        let lines: Vec<Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["state"], "submitted");
        assert_eq!(lines[0]["idempotencyKey"], "key-0");
        assert_eq!(lines[1]["state"], "accepted");
        assert_eq!(lines[1]["runId"], "run-0");
        assert_eq!(lines[1]["batch"], "batch-x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_ack_projects_the_submit_receipt() {
        let ack = run_ack(
            &json!({
                "kind": "run", "runId": "run-9", "durable": true,
                "acceptedRevision": "rev-3", "state": "queued", "deduplicated": false,
            }),
            "s-1",
        );
        assert_eq!(ack["runId"], "run-9");
        assert_eq!(ack["sessionId"], "s-1");
        assert_eq!(ack["accepted"], "durable");
        assert_eq!(ack["state"], "queued");
        assert_eq!(ack["acceptedRevision"], "rev-3");
        // RunAck 必过 result schema（receipt 位）。
        let frame = frame_accepted("run-9", &ack);
        OperationRegistry::global()
            .validate_result("input.follow", &json!({"frame": frame}))
            .expect("RunAck projection must satisfy the schema");
    }
}
