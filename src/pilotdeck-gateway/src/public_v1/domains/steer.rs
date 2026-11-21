//! `steer.*` + `run.continue` — 运行中引导与中断显式续接（`t009`）。
//!
//! 合同正本 `state/cli-v1-operations.json`：
//!
//! - **`steer.submit`**（sync / idempotentSubmit / owner:write）：向一个
//!   **非终态** run 提交中间引导输入，落库为「steer 队列」中一行
//!   （postcondition「按steerId防重，受理不等于已消费」→ `pending`）。
//!   终态 run 上的新提交拒绝 `RUN_TERMINAL`；同 `(runId, steerId)` 重提交
//!   幂等返回当前状态、不同输入拒绝 `IDEMPOTENCY_CONFLICT`（「同ID不同
//!   输入冲突」）。受理即持久写（「不支持零写入」）。dispatch 层要求
//!   idempotencyKey（retry=idempotentSubmit），本域再按 run.submit 先例登记
//!   `idempotency_records`（key 内容冲突拒绝；确定性拒绝可重放）。
//! - **`steer.get`**（sync / readOnly / owner:read）：读挂起引导的状态，
//!   无业务状态修改——终态 run 上的 `pending` 引导在读时**计算**为
//!   `rejected`（不落库），`consumed` 的 `state` 同样按 run 当前终态计算。
//! - **`steer.withdraw`**（sync / conditionalWrite / owner:write）：对存储
//!   `phase='pending'` 的引导做 CAS 撤销（`expectedRevision` 复验）；已
//!   `consumed` 拒绝 `ALREADY_CONSUMED`（「消费与撤销仅一方成功」「已消费
//!   不回滚工具效果」）。撤回后 `phase='withdrawn'`、`state='cancelled'`。
//! - **`run.continue`**（async / idempotentSubmit / owner:runSubmit，D9「保留
//!   记录，显式续接，不自动重放」）：从 `interrupted` 的源 run 显式发起
//!   **新 run**（合同 postcondition「新run持久记录continuedFrom；旧记录不
//!   覆盖」，acceptance「新runId必须不同于sourceRunId」「不重放旧工具」）。
//!   新 run 的落库形状复刻 `store.rs::submit_run`（`submit_run` 自持锁不能
//!   在本事务内调用，形状由本文件测试与 `run.submit` 回执对拍保证——
//!   `replacement.commit` 同一先例）。
//!
//! ## 合同未明确处的实现选择（显式登记，供集成者复核）
//!
//! 1. **steer 结果的 `state` 语义**：合同只给枚举
//!    （queued/succeeded/cancelled/failed/interrupted/result_unknown，无
//!    running/waiting/cancelling）。实现为**引导自身的处置态**：`pending` →
//!    `queued`；`consumed`/`rejected` → 所属 run 的终态（run 未终态时
//!    `queued`）；`withdrawn` → `cancelled`。依据：steer.submit 结果 phase 枚举
//!    （pending|consumed）与 steer.get（+withdrawn|rejected）构成引导生命周期，
//!    state 只能是该生命周期各阶段的处置结果。
//! 2. **`RUN_TERMINAL` 闸的作用域**：合同只声明终态错误，未列举可接受的非终态。
//!    实现为一切非终态 run（queued/running/waiting/cancelling）可受理 steer——
//!    排队中的 run 到启动时消费（TS 基线 `SteerMailbox` 为 turn 级邮箱，本
//!    合同按 runId 绑定）。同 `(runId, steerId)` 的重提交先于终态闸判定
//!    （幂等重放不因 run 已终态而误报 `RUN_TERMINAL`）。
//! 3. **`run.continue` 的 `UNRESOLVED_EFFECTS`**：合同未定义「未知效果」的
//!    标识集合。本存储中 `result_unknown` run 自身是唯一的效果载体
//!    （`effect_status='unknown'`），故实现为：源 run 处于 `result_unknown`
//!    时，`acknowledgedUnknownEffects` 必须包含 `sourceRunId`，否则
//!    `UNRESOLVED_EFFECTS`（错误 details 列出待处置 id）。
//! 4. **`run.continue` 的会话准入**：镜像 `run.submit`（缺失/删除 →
//!    NOT_FOUND、closed → SESSION_CLOSED、queue paused → QUEUE_PAUSED，均
//!    before-side-effect）。崩溃恢复会暂停会话队列（D9 防自动重放），续接
//!    前需 `queue.resume` 显式恢复——显式续接不隐式解除暂停。
//! 5. **`SOURCE_NOT_INTERRUPTED` 与修订复验顺序**：状态闸先于
//!    `expectedRevision` 复验（非 interrupted 的 run 即便修订过期也先报
//!    SOURCE_NOT_INTERRUPTED）；`result_unknown` 走 UNRESOLVED_EFFECTS 路径，
//!    不算 SOURCE_NOT_INTERRUPTED（acceptance「未知效果未处置拒绝」隐含
//!    已处置则可续接）。
//! 6. **幂等重放不冻结 steer 状态**：steer.submit 的幂等重放返回**当前**
//!    引导状态而非受理时快照（result schema `additionalProperties:false`
//!    禁止注入 `deduplicated` 标记；「受理不等于已消费」要求重试可见
//!    阶段推进）。run.continue 的 receipt 重放则完全按 run.submit 先例注入
//!    `deduplicated=true`（receipt schema 有该字段）。
//!
//! ## 与 run 域的缝合（集成者接线点，`store.rs`/`runtime.rs` 不改）
//!
//! - **消费原语**：[`V1Store::take_pending_steer`] 在事务内 CAS
//!   `pending → consumed`（修订推进），供驱动方调用。离线驱动经
//!   `[pd-driver:await_steer]` 指令消费（`run_driver.rs`）；真实 turn 机器
//!   （`runtime.rs::drive_owned_turn`）目前不感知 steer——接线点：turn 循环
//!   在模型步边界调用 `take_pending_steer` 并把 parts 注入下一轮输入。
//! - **唤醒**：steer.submit/withdraw/consume 提交后 `state_notifier()
//!   .notify_waiters()`（`store` 既有 pub 面），`await_steer` 驱动与任何
//!   `run.wait` 式轮询者以此感知到达。
//! - **run.continue 的新 run 驱动**：handler 在会话空闲时调
//!   `rt.start_session_worker(run_id)`（`run.submit` 相同唤醒点）。

use std::sync::Arc;

use agentcpm_pd_protocol::contract::{ResponseOutcome, RunState};
use agentcpm_pd_protocol::envelope::compute_input_digest;
use agentcpm_pd_protocol::errors::{
    CODE_CAPABILITY_UNAVAILABLE, CODE_IDEMPOTENCY_CONFLICT, CODE_NOT_FOUND, CODE_QUEUE_PAUSED,
    CODE_REVISION_CONFLICT, CODE_UNAVAILABLE, ContractError, EffectStatus, RetrySpec,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::session_admin::CODE_SESSION_CLOSED;
use crate::public_v1::domains::workspace_dirs::{bump_rev_in_tx, domain_not_found, sql_err};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, now_rfc3339, parse_run_state, run_state_str};

/// 合同声明而协议 crate 尚无常量的错误码。
pub(crate) const CODE_RUN_TERMINAL: &str = "RUN_TERMINAL";
pub(crate) const CODE_ALREADY_CONSUMED: &str = "ALREADY_CONSUMED";
pub(crate) const CODE_SOURCE_NOT_INTERRUPTED: &str = "SOURCE_NOT_INTERRUPTED";
pub(crate) const CODE_UNRESOLVED_EFFECTS: &str = "UNRESOLVED_EFFECTS";

/// steer 存储的 phase（终态 run 上 `pending` 的读时计算值见
/// [`effective_status`]；`rejected` 不落库，保证 steer.get 只读）。
const PHASE_PENDING: &str = "pending";
const PHASE_CONSUMED: &str = "consumed";
const PHASE_WITHDRAWN: &str = "withdrawn";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema(
        "steers",
        "CREATE TABLE IF NOT EXISTS steers (
            run_id TEXT NOT NULL,
            steer_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            instance_id TEXT NOT NULL,
            input_json TEXT NOT NULL,
            input_digest TEXT NOT NULL,
            phase TEXT NOT NULL,
            state TEXT NOT NULL,
            revision TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (run_id, steer_id)
        );
        CREATE INDEX IF NOT EXISTS idx_steers_pending ON steers(run_id, phase);",
    );
    reg.handler("steer.submit", |rt, input, key, _rev| {
        Box::pin(steer_submit(rt, input, key))
    });
    reg.handler("steer.get", |rt, input, _key, _rev| {
        Box::pin(steer_get(rt, input))
    });
    reg.handler("steer.withdraw", |rt, input, _key, rev| {
        Box::pin(steer_withdraw(rt, input, rev))
    });
    reg.handler("run.continue", |rt, input, key, rev| {
        Box::pin(run_continue(rt, input, key, rev))
    });
}

async fn run_blocking<T, F>(task: F) -> Result<T, ContractError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ContractError> + Send + 'static,
{
    tokio::task::spawn_blocking(task).await.map_err(|err| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("steer worker failed: {err}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?
}

async fn steer_submit(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let supports_steer = rt.supports_steer();
    let result = run_blocking(move || {
        rt.store()
            .submit_steer_with_capability(&input, idempotency_key.as_deref(), supports_steer)
    })
    .await?;
    Ok((ResponseOutcome::Succeeded, result))
}

async fn steer_get(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let run_id = input["runId"].as_str().unwrap_or_default().to_owned();
    let steer_id = input["steerId"].as_str().unwrap_or_default().to_owned();
    let result = run_blocking(move || rt.store().get_steer_status(&run_id, &steer_id)).await?;
    Ok((ResponseOutcome::Succeeded, result))
}

async fn steer_withdraw(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    expected_revision: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    // envelope 级 expectedRevision 优先，缺省回落到 params 内（interaction.rs
    // 先例）；schema 已保证 params.expectedRevision 必填。
    let rev = expected_revision
        .as_deref()
        .or_else(|| input.get("expectedRevision").and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned();
    let run_id = input["runId"].as_str().unwrap_or_default().to_owned();
    let steer_id = input["steerId"].as_str().unwrap_or_default().to_owned();
    let result = run_blocking(move || rt.store().withdraw_steer(&run_id, &steer_id, &rev)).await?;
    Ok((ResponseOutcome::Succeeded, result))
}

async fn run_continue(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
    expected_revision: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let effective = expected_revision
        .as_deref()
        .or_else(|| input.get("expectedRevision").and_then(Value::as_str))
        .map(ToOwned::to_owned);
    let rt_store = rt.clone();
    let outcome = run_blocking(move || {
        rt_store
            .store()
            .continue_run(&input, idempotency_key.as_deref(), effective.as_deref())
    })
    .await?;
    if outcome.start_worker {
        rt.start_session_worker(outcome.run_id.clone());
    }
    Ok((ResponseOutcome::Accepted, outcome.receipt))
}

/// [`V1Store::take_pending_steer`] 的返回：一次被消费的引导输入。
pub(crate) struct ConsumedSteer {
    pub(crate) steer_id: String,
    pub(crate) input: Value,
}

impl ConsumedSteer {
    /// 引导输入的文本部分（attachment 部分无文本，跳过——与
    /// `run_driver.rs::extract_prompt_text` 相同的投影）。
    pub(crate) fn text(&self) -> String {
        self.input
            .as_array()
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }
}

/// [`V1Store::continue_run`] 的返回。
pub(crate) struct ContinueOutcome {
    /// run 形态 receipt（与 `run.submit` 同构）。
    pub receipt: Value,
    pub run_id: String,
    /// 会话空闲（无 active、无 queued）时立即驱动新 run。
    pub start_worker: bool,
}

/// 引导的读时生效状态：`(phase, state)`。存储 phase 只有
/// pending/consumed/withdrawn；终态 run 上的 pending 引导计算为 rejected
/// （不落库——steer.get 只读），consumed/终态的 state 跟随 run 终态。
fn effective_status(
    stored_phase: &str,
    stored_state: &str,
    run_state: RunState,
) -> (&'static str, String) {
    let terminal = run_state_str(run_state).to_owned();
    match stored_phase {
        PHASE_PENDING if run_state.is_terminal() => ("rejected", terminal),
        PHASE_PENDING => ("pending", "queued".to_owned()),
        PHASE_CONSUMED if run_state.is_terminal() => ("consumed", terminal),
        PHASE_CONSUMED => ("consumed", "queued".to_owned()),
        _ => (PHASE_WITHDRAWN, stored_state.to_owned()),
    }
}

/// `(input_json, input_digest, phase, state, revision)` — steers 行的业务列。
type SteerColumns = (String, String, String, String, String);

const STEER_SELECT: &str = "SELECT input_json, input_digest, phase, state, revision FROM steers WHERE run_id = ?1 AND steer_id = ?2";

fn load_steer(
    conn: &Connection,
    run_id: &str,
    steer_id: &str,
) -> Result<Option<SteerColumns>, ContractError> {
    conn.query_row(STEER_SELECT, params![run_id, steer_id], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    })
    .optional()
    .map_err(sql_err)
}

fn steer_status_json(steer_id: &str, phase: &str, state: &str, revision: &str) -> Value {
    json!({
        "steerId": steer_id,
        "state": state,
        "revision": revision,
        "phase": phase,
    })
}

/// 幂等拒绝登记（run.submit 的 `record_rejection_in_tx` 同形，method_id 参数化）。
fn record_rejection(
    conn: &Connection,
    method_id: &str,
    key: &str,
    digest: &str,
    rejection_code: &str,
    observed_at: &str,
    retained_until: &str,
) -> Result<(), ContractError> {
    conn.execute(
        "INSERT OR IGNORE INTO idempotency_records (idempotency_key, method_id, input_digest, request_state, reason, resource_json, receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
         VALUES (?1, ?2, ?3, 'notAccepted', 'definitivelyRejected', NULL, NULL, ?4, ?5, ?6, 'correctRejectedInputWithNewKey')",
        params![key, method_id, digest, rejection_code, observed_at, retained_until],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// run 事件追加（`store.rs::append_event_in_tx` 的领域内镜像：占位游标 +
/// 回填，schema id 与 run 事件一致），返回确定游标。
fn append_run_event_in_tx(
    conn: &Connection,
    resource_id: &str,
    session_id: &str,
    event_type: &str,
    payload: &Value,
    occurred_at: &str,
) -> Result<String, ContractError> {
    let placeholder = format!(
        "tmp-{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    );
    let payload_str = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
    conn.execute(
        "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at)
         VALUES (?1, ?2, ?3, ?4, 'agentcpm-pd.run.event/1', ?5, ?5, ?6)",
        params![placeholder, resource_id, session_id, event_type, payload_str, occurred_at],
    )
    .map_err(sql_err)?;
    let seq = conn.last_insert_rowid();
    let cursor = crate::public_v1::store::format_cursor(seq);
    conn.execute(
        "UPDATE events SET cursor = ?1 WHERE seq = ?2",
        params![cursor, seq],
    )
    .map_err(sql_err)?;
    Ok(cursor)
}

impl V1Store {
    /// `steer.submit`：一个事务内完成能力闸、幂等闸、run 存在性/终态闸、
    /// `steerId` 防重与新引导落库 + 幂等记录。
    #[allow(dead_code)]
    pub(crate) fn submit_steer(
        &self,
        input: &Value,
        idempotency_key: Option<&str>,
    ) -> Result<Value, ContractError> {
        self.submit_steer_with_capability(
            input,
            idempotency_key,
            crate::public_v1::run_driver::offline_directives_enabled(),
        )
    }

    pub(crate) fn submit_steer_with_capability(
        &self,
        input: &Value,
        idempotency_key: Option<&str>,
        supports_steer: bool,
    ) -> Result<Value, ContractError> {
        if !supports_steer {
            return Err(ContractError::new(
                CODE_CAPABILITY_UNAVAILABLE,
                "active run driver does not support mid-run steer injection",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }

        let run_id = input["runId"].as_str().unwrap_or_default();
        let steer_id = input["steerId"].as_str().unwrap_or_default();
        let digest = compute_input_digest("steer.submit", input);
        let now = now_rfc3339();
        let retained_until = (chrono::Utc::now() + chrono::Duration::days(7)).to_rfc3339();

        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;

        // 1. 幂等闸（先于一切资源查找，run.submit 同序）。
        if let Some(key) = idempotency_key {
            let existing: Option<(String, String, Option<String>, Option<String>)> = tx
                .query_row(
                    "SELECT input_digest, request_state, receipt_json, rejection_code
                     FROM idempotency_records WHERE idempotency_key = ?1 AND method_id = 'steer.submit'",
                    params![key],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()
                .map_err(sql_err)?;
            if let Some((stored_digest, req_state, receipt_json, rej_code)) = existing {
                if stored_digest != digest {
                    return Err(ContractError::new(
                        CODE_IDEMPOTENCY_CONFLICT,
                        format!(
                            "idempotencyKey `{key}` already used for steer.submit with different input"
                        ),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
                if req_state != "accepted" {
                    return Err(ContractError::new(
                        rej_code.unwrap_or_else(|| CODE_NOT_FOUND.to_owned()),
                        "steer submit previously rejected definitively",
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
                // accepted + 同 key 重放：若已进入 consumed 则投影真实 consumed 状态；
                // 若未消费或已撤回/终态拒绝，重放原接受回执以恪守 steer.submit schema
                // （schema 仅允许 pending/consumed）。
                if let Some((_, _, phase, state, revision)) = load_steer(&tx, run_id, steer_id)?
                    && phase == PHASE_CONSUMED
                {
                    let state_str: Option<String> = tx
                        .query_row(
                            "SELECT state FROM runs WHERE run_id = ?1",
                            params![run_id],
                            |r| r.get(0),
                        )
                        .optional()
                        .map_err(sql_err)?;
                    let run_state = parse_run_state(state_str.as_deref().unwrap_or("running"));
                    let (phase_eff, state_eff) = effective_status(&phase, &state, run_state);
                    return Ok(steer_status_json(
                        steer_id, phase_eff, &state_eff, &revision,
                    ));
                }
                if let Some(raw_receipt) = receipt_json
                    && let Ok(parsed) = serde_json::from_str::<Value>(&raw_receipt)
                {
                    return Ok(parsed);
                }
            }
        }

        // 2. run 存在性（同 steerId 的重提交也要能查到 run 以计算状态）。
        let run: Option<(String, String, i64)> = tx
            .query_row(
                "SELECT session_id, state, terminal FROM runs WHERE run_id = ?1",
                params![run_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(sql_err)?;
        let Some((session_id, state_str, terminal)) = run else {
            if let Some(key) = idempotency_key {
                record_rejection(
                    &tx,
                    "steer.submit",
                    key,
                    &digest,
                    CODE_NOT_FOUND,
                    &now,
                    &retained_until,
                )?;
                tx.commit().map_err(sql_err)?;
            }
            return Err(domain_not_found("run", run_id));
        };

        // 3. steerId 防重（允许同一 `(runId, steerId)` 携带新幂等键重入）：
        //    - consumed → 返回 consumed（并回填 idempotency_records）；
        //    - withdrawn → IDEMPOTENCY_CONFLICT（已撤回不可恢复）；
        //    - pending + live → 返回 pending（并回填 idempotency_records）；
        //    - pending + terminal/cancelling → RUN_TERMINAL（严守 schema）。
        if let Some((_stored_json, stored_digest, phase, state, revision)) =
            load_steer(&tx, run_id, steer_id)?
        {
            if stored_digest != digest {
                return Err(ContractError::new(
                    CODE_IDEMPOTENCY_CONFLICT,
                    format!(
                        "steerId `{steer_id}` already used for run `{run_id}` with different input"
                    ),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            let run_state = parse_run_state(&state_str);
            let status = match phase.as_str() {
                PHASE_CONSUMED => {
                    let (phase_eff, state_eff) = effective_status(&phase, &state, run_state);
                    steer_status_json(steer_id, phase_eff, &state_eff, &revision)
                }
                PHASE_WITHDRAWN => {
                    if let Some(key) = idempotency_key {
                        record_rejection(
                            &tx,
                            "steer.submit",
                            key,
                            &digest,
                            CODE_IDEMPOTENCY_CONFLICT,
                            &now,
                            &retained_until,
                        )?;
                        tx.commit().map_err(sql_err)?;
                    }
                    return Err(ContractError::new(
                        CODE_IDEMPOTENCY_CONFLICT,
                        format!(
                            "steerId `{steer_id}` on run `{run_id}` was withdrawn; pick a new steerId"
                        ),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
                _ => {
                    if terminal != 0 || state_str == "cancelling" {
                        if let Some(key) = idempotency_key {
                            record_rejection(
                                &tx,
                                "steer.submit",
                                key,
                                &digest,
                                CODE_RUN_TERMINAL,
                                &now,
                                &retained_until,
                            )?;
                            tx.commit().map_err(sql_err)?;
                        }
                        return Err(ContractError::new(
                            CODE_RUN_TERMINAL,
                            format!(
                                "run `{run_id}` is terminal or cancelling ({state_str}); unconsumed steer `{steer_id}` can no longer be submitted"
                            ),
                            RetrySpec::never(),
                            EffectStatus::NotApplied,
                        ));
                    }
                    steer_status_json(steer_id, PHASE_PENDING, "queued", &revision)
                }
            };
            if let Some(key) = idempotency_key {
                let resource_val = json!({"kind": "steer", "runId": run_id, "steerId": steer_id});
                tx.execute(
                    "INSERT OR IGNORE INTO idempotency_records (idempotency_key, method_id, input_digest, request_state, reason, resource_json, receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
                     VALUES (?1, 'steer.submit', ?2, 'accepted', 'receiptFound', ?3, ?4, NULL, ?5, ?6, 'doNotReplay')",
                    params![
                        key,
                        digest,
                        serde_json::to_string(&resource_val).unwrap(),
                        serde_json::to_string(&status).unwrap(),
                        now,
                        retained_until
                    ],
                )
                .map_err(sql_err)?;
                tx.commit().map_err(sql_err)?;
            }
            return Ok(status);
        }

        // 4. 终态/取消中闸（新提交）：cancelling 与一切终态均不再接受引导。
        if terminal != 0 || state_str == "cancelling" {
            if let Some(key) = idempotency_key {
                record_rejection(
                    &tx,
                    "steer.submit",
                    key,
                    &digest,
                    CODE_RUN_TERMINAL,
                    &now,
                    &retained_until,
                )?;
                tx.commit().map_err(sql_err)?;
            }
            return Err(ContractError::new(
                CODE_RUN_TERMINAL,
                format!(
                    "run `{run_id}` is terminal or cancelling ({state_str}); steers need a live non-terminal run"
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }

        // 5. 新引导落库（「不支持零写入」：受理即持久行）。
        let rev = bump_rev_in_tx(&tx)?;
        let input_json = serde_json::to_string(&input.get("input").cloned().unwrap_or(Value::Null))
            .unwrap_or_else(|_| "null".into());
        tx.execute(
            "INSERT INTO steers (run_id, steer_id, session_id, instance_id, input_json, input_digest, phase, state, revision, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', 'queued', ?7, ?8, ?9)",
            params![
                run_id,
                steer_id,
                session_id,
                self.meta().instance_id,
                input_json,
                digest,
                rev,
                now,
                now
            ],
        )
        .map_err(sql_err)?;

        let status = steer_status_json(steer_id, PHASE_PENDING, "queued", &rev);
        if let Some(key) = idempotency_key {
            let resource_val = json!({"kind": "steer", "runId": run_id, "steerId": steer_id});
            tx.execute(
                "INSERT INTO idempotency_records (idempotency_key, method_id, input_digest, request_state, reason, resource_json, receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
                 VALUES (?1, 'steer.submit', ?2, 'accepted', 'receiptFound', ?3, ?4, NULL, ?5, ?6, 'doNotReplay')",
                params![
                    key,
                    digest,
                    serde_json::to_string(&resource_val).unwrap(),
                    serde_json::to_string(&status).unwrap(),
                    now,
                    retained_until
                ],
            )
            .map_err(sql_err)?;
        }

        tx.commit().map_err(sql_err)?;
        // 唤醒等待引导的驱动方（await_steer / 任何 state 轮询者）。
        self.state_notifier().notify_waiters();
        Ok(status)
    }

    /// `steer.get`：只读查询。终态 run 上的 pending 计算为 rejected，
    /// consumed 的 state 跟随 run 终态——均不落库（postcondition「无业务
    /// 状态修改」）。
    pub(crate) fn get_steer_status(
        &self,
        run_id: &str,
        steer_id: &str,
    ) -> Result<Value, ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        let Some((_input_json, _digest, phase, state, revision)) =
            load_steer(&conn, run_id, steer_id)?
        else {
            return Err(domain_not_found("steer", steer_id));
        };
        let state_str: Option<String> = conn
            .query_row(
                "SELECT state FROM runs WHERE run_id = ?1",
                params![run_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        let Some(state_str) = state_str else {
            // 引导的所属 run 不存在（runs 行不删除，防御性兜底）。
            return Err(domain_not_found("steer", steer_id));
        };
        let run_state = parse_run_state(&state_str);
        let (phase_eff, state_eff) = effective_status(&phase, &state, run_state);
        Ok(steer_status_json(
            steer_id, phase_eff, &state_eff, &revision,
        ))
    }

    /// `steer.withdraw`：对存储 `pending` 且 run 未终态的引导做条件写撤销。
    /// 消费与撤销经同一 sqlite 写事务串行化，仅一方成功；run 已终态化时未消费
    /// 的引导读面已是 `rejected`，不可再篡改为 `withdrawn`/`cancelled`。
    pub(crate) fn withdraw_steer(
        &self,
        run_id: &str,
        steer_id: &str,
        expected_revision: &str,
    ) -> Result<Value, ContractError> {
        let now = now_rfc3339();
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;

        let Some((_input_json, _digest, phase, state, revision)) =
            load_steer(&tx, run_id, steer_id)?
        else {
            return Err(domain_not_found("steer", steer_id));
        };
        let run_terminal: i64 = tx
            .query_row(
                "SELECT terminal FROM runs WHERE run_id = ?1",
                params![run_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?
            .unwrap_or(1);

        match phase.as_str() {
            // 「已消费不可伪装撤回」：终局事实优先于修订复验。
            PHASE_CONSUMED => Err(ContractError::new(
                CODE_ALREADY_CONSUMED,
                format!(
                    "steer `{steer_id}` on run `{run_id}` was already consumed; its effects cannot be rolled back"
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )),
            // 已撤销的幂等重放（修订须与当前一致）。
            PHASE_WITHDRAWN => {
                if revision != expected_revision {
                    return Err(revision_conflict(expected_revision, &revision));
                }
                Ok(steer_status_json(
                    steer_id,
                    PHASE_WITHDRAWN,
                    &state,
                    &revision,
                ))
            }
            _ => {
                // pending：run 已终态化则该引导已转为 rejected，不再允许改写成 withdrawn。
                if revision != expected_revision {
                    return Err(revision_conflict(expected_revision, &revision));
                }
                if run_terminal != 0 {
                    return Err(ContractError::new(
                        CODE_REVISION_CONFLICT,
                        format!(
                            "run `{run_id}` is already terminal; pending steer `{steer_id}` is already rejected and cannot be withdrawn"
                        ),
                        RetrySpec::after_reconcile(),
                        EffectStatus::NotApplied,
                    ));
                }
                let rev = bump_rev_in_tx(&tx)?;
                tx.execute(
                    "UPDATE steers SET phase = 'withdrawn', state = 'cancelled', revision = ?1, updated_at = ?2
                     WHERE run_id = ?3 AND steer_id = ?4 AND phase = 'pending'",
                    params![rev, now, run_id, steer_id],
                )
                .map_err(sql_err)?;
                tx.commit().map_err(sql_err)?;
                self.state_notifier().notify_waiters();
                Ok(steer_status_json(
                    steer_id,
                    PHASE_WITHDRAWN,
                    "cancelled",
                    &rev,
                ))
            }
        }
    }

    /// 消费原语（驱动方缝合点）：取走该 run 最早的一条 pending 引导，
    /// 事务内 CAS `pending → consumed`（修订推进）。无 pending 返回 None。
    /// 返回的 [`ConsumedSteer`] 携带原始 input parts 供注入。
    pub(crate) fn take_pending_steer(
        &self,
        run_id: &str,
    ) -> Result<Option<ConsumedSteer>, ContractError> {
        let now = now_rfc3339();
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;

        let row: Option<(String, String)> = tx
            .query_row(
                "SELECT steer_id, input_json FROM steers
                 WHERE run_id = ?1 AND phase = 'pending'
                 ORDER BY created_at ASC, rowid ASC LIMIT 1",
                params![run_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        let Some((steer_id, input_json)) = row else {
            return Ok(None);
        };

        let rev = bump_rev_in_tx(&tx)?;
        let updated = tx
            .execute(
                "UPDATE steers SET phase = 'consumed', revision = ?1, updated_at = ?2
                 WHERE run_id = ?3 AND steer_id = ?4 AND phase = 'pending'",
                params![rev, now, run_id, steer_id],
            )
            .map_err(sql_err)?;
        if updated == 0 {
            // CAS 失败（并发下被他人取走）——本调用无消费。
            return Ok(None);
        }
        tx.commit().map_err(sql_err)?;
        self.state_notifier().notify_waiters();
        let input: Value = serde_json::from_str(&input_json).unwrap_or(Value::Null);
        Ok(Some(ConsumedSteer { steer_id, input }))
    }

    /// 只读查看最早一条 `pending` 引导（用于 `TurnSubmitter` 投递成功后再标 consumed）。
    pub(crate) fn peek_pending_steer(
        &self,
        run_id: &str,
    ) -> Result<Option<ConsumedSteer>, ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT steer_id, input_json FROM steers
                 WHERE run_id = ?1 AND phase = 'pending'
                 ORDER BY created_at ASC, rowid ASC LIMIT 1",
                params![run_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        Ok(row.map(|(steer_id, input_json)| ConsumedSteer {
            steer_id,
            input: serde_json::from_str(&input_json).unwrap_or(Value::Null),
        }))
    }

    /// CAS 把已成功递交到 turn mailbox 的引导标为 `consumed`。
    pub(crate) fn mark_steer_consumed(
        &self,
        run_id: &str,
        steer_id: &str,
    ) -> Result<bool, ContractError> {
        let now = now_rfc3339();
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let rev = bump_rev_in_tx(&tx)?;
        let updated = tx
            .execute(
                "UPDATE steers SET phase = 'consumed', revision = ?1, updated_at = ?2
                 WHERE run_id = ?3 AND steer_id = ?4 AND phase = 'pending'",
                params![rev, now, run_id, steer_id],
            )
            .map_err(sql_err)?;
        if updated == 0 {
            return Ok(false);
        }
        tx.commit().map_err(sql_err)?;
        self.state_notifier().notify_waiters();
        Ok(true)
    }

    /// `run.continue`：一个事务内完成幂等闸、源 run 状态/修订/未知效果
    /// 复验、会话准入与新 run 受理（复刻 `submit_run` 落库形状）。
    pub(crate) fn continue_run(
        &self,
        input: &Value,
        idempotency_key: Option<&str>,
        expected_revision: Option<&str>,
    ) -> Result<ContinueOutcome, ContractError> {
        let source_run_id = input["sourceRunId"].as_str().unwrap_or_default();
        let parts = input.get("input").cloned().unwrap_or(Value::Null);
        let effective_rev = expected_revision.unwrap_or_default();
        let digest = compute_input_digest("run.continue", input);
        let now = now_rfc3339();
        let retained_until = (chrono::Utc::now() + chrono::Duration::days(7)).to_rfc3339();

        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;

        // 1. 幂等闸（run.submit 同序）。
        if let Some(key) = idempotency_key {
            let existing: Option<(String, String, Option<String>, Option<String>)> = tx
                .query_row(
                    "SELECT input_digest, request_state, receipt_json, rejection_code
                     FROM idempotency_records WHERE idempotency_key = ?1 AND method_id = 'run.continue'",
                    params![key],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()
                .map_err(sql_err)?;
            if let Some((stored_digest, req_state, receipt_json, rej_code)) = existing {
                if stored_digest != digest {
                    return Err(ContractError::new(
                        CODE_IDEMPOTENCY_CONFLICT,
                        format!(
                            "idempotencyKey `{key}` already used for run.continue with different input"
                        ),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
                if req_state == "accepted"
                    && let Some(raw_receipt) = receipt_json
                    && let Ok(mut receipt) = serde_json::from_str::<Value>(&raw_receipt)
                {
                    if let Some(obj) = receipt.as_object_mut() {
                        obj.insert("deduplicated".into(), json!(true));
                    }
                    return Ok(ContinueOutcome {
                        receipt,
                        run_id: String::new(),
                        start_worker: false,
                    });
                }
                return Err(ContractError::new(
                    rej_code.unwrap_or_else(|| CODE_NOT_FOUND.to_owned()),
                    "continue request previously rejected definitively",
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
        }

        // 2. 源 run 存在性。
        let src: Option<(String, String, String)> = tx
            .query_row(
                "SELECT session_id, state, revision FROM runs WHERE run_id = ?1",
                params![source_run_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(sql_err)?;
        let Some((session_id, src_state, src_revision)) = src else {
            if let Some(key) = idempotency_key {
                record_rejection(
                    &tx,
                    "run.continue",
                    key,
                    &digest,
                    CODE_NOT_FOUND,
                    &now,
                    &retained_until,
                )?;
                tx.commit().map_err(sql_err)?;
            }
            return Err(domain_not_found("run", source_run_id));
        };

        // 3. 状态闸：interrupted 可续接；result_unknown 需处置未知效果；
        //    其余（含一切非终态）拒绝 SOURCE_NOT_INTERRUPTED。
        if src_state != "interrupted" && src_state != "result_unknown" {
            return Err(source_not_interrupted(source_run_id, &src_state));
        }
        if src_state == "result_unknown" {
            let acknowledged = input
                .get("acknowledgedUnknownEffects")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(ToOwned::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if !acknowledged.iter().any(|id| id == source_run_id) {
                let mut details = serde_json::Map::new();
                details.insert("unresolvedEffects".into(), json!([source_run_id]));
                return Err(ContractError::new(
                    CODE_UNRESOLVED_EFFECTS,
                    format!(
                        "source run `{source_run_id}` is result_unknown; acknowledge its unknown effects \
                         via acknowledgedUnknownEffects before continuing"
                    ),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )
                .with_details(details));
            }
        }

        // 4. 修订复验（观察到的源 run 版本）。
        if src_revision != effective_rev {
            return Err(revision_conflict(effective_rev, &src_revision));
        }

        // 5. 会话准入（事务内镜像 check_session_admission + submit_run 队列
        //    检查——replacement.commit 同一镜像先例）。
        #[allow(clippy::type_complexity)]
        let sess: Option<(String, bool, Option<String>, Option<String>, String)> = tx
            .query_row(
                "SELECT project_id, queue_paused, pause_reason, active_run_id, session_state
                 FROM sessions WHERE session_id = ?1",
                params![session_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get::<_, i64>(1)? != 0,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                    ))
                },
            )
            .optional()
            .map_err(sql_err)?;
        let Some((project_id, paused, pause_reason, active_run_id, session_state)) = sess else {
            if let Some(key) = idempotency_key {
                record_rejection(
                    &tx,
                    "run.continue",
                    key,
                    &digest,
                    CODE_NOT_FOUND,
                    &now,
                    &retained_until,
                )?;
                tx.commit().map_err(sql_err)?;
            }
            return Err(domain_not_found("session", &session_id));
        };
        if session_state == "deleted" {
            if let Some(key) = idempotency_key {
                record_rejection(
                    &tx,
                    "run.continue",
                    key,
                    &digest,
                    CODE_NOT_FOUND,
                    &now,
                    &retained_until,
                )?;
                tx.commit().map_err(sql_err)?;
            }
            return Err(domain_not_found("session", &session_id));
        }
        let project_exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM projects WHERE project_id = ?1)",
                params![project_id],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        if !project_exists {
            if let Some(key) = idempotency_key {
                record_rejection(
                    &tx,
                    "run.continue",
                    key,
                    &digest,
                    CODE_NOT_FOUND,
                    &now,
                    &retained_until,
                )?;
                tx.commit().map_err(sql_err)?;
            }
            return Err(domain_not_found("project", &project_id));
        }
        if session_state == "closed" {
            return Err(ContractError::new(
                CODE_SESSION_CLOSED,
                format!("session `{session_id}` is closed; reopen it before continuing"),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        if paused {
            return Err(ContractError::new(
                CODE_QUEUE_PAUSED,
                format!(
                    "session `{session_id}` queue is paused ({}); call queue.resume first",
                    pause_reason.as_deref().unwrap_or("paused")
                ),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }

        // 6. 新 run 受理（复刻 submit_run 的落库形状；D9「不重放旧工具」：
        //    新 run 输入只含调用方显式 parts，不复制源 run 输入）。
        let run_id = self.next_id();
        let rev = bump_rev_in_tx(&tx)?;
        let queued_exists: i64 = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM run_queue WHERE session_id = ?1)",
                params![session_id],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        // P1 #15: 只把真实运行中 (`running`/`waiting`/`cancelling`) 且非终态的
        // pin 视作占用，queued 态残留 pin 不得拖延续接；同时入队必推进
        // sessions.queue_revision 以保护 queue.* CAS。
        let pin_is_live: bool = match active_run_id.as_deref() {
            Some(pin_id) => tx
                .query_row(
                    "SELECT terminal = 0 AND state IN ('running', 'waiting', 'cancelling')
                     FROM runs WHERE run_id = ?1",
                    params![pin_id],
                    |r| r.get::<_, i64>(0).map(|v| v != 0),
                )
                .optional()
                .map_err(sql_err)?
                .unwrap_or(false),
            None => false,
        };
        let start_immediately = !pin_is_live && queued_exists == 0;
        let run_input = json!({
            "instanceId": self.meta().instance_id,
            "sessionId": session_id,
            "input": parts,
        });
        let input_json = serde_json::to_string(&run_input).unwrap_or_else(|_| "{}".into());
        let config_json = json!({
            "configRevision": self.meta().initialization_revision,
            "effectiveGeneration": self.meta().generation,
        });
        let config_str = serde_json::to_string(&config_json).unwrap();
        let queued_cursor = append_run_event_in_tx(
            &tx,
            &run_id,
            &session_id,
            "run.queued",
            &json!({"runId": run_id, "sessionId": session_id, "state": "queued"}),
            &now,
        )?;
        tx.execute(
            "INSERT INTO runs (run_id, session_id, project_id, state, revision, cancel_requested, waiting_for_json, effect_status, config_json, final_cursor, continued_from, outcome_code, result_json, input_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'queued', ?4, 0, '[]', 'known', ?5, ?6, ?7, NULL, NULL, ?8, ?9, ?10)",
            params![
                run_id,
                session_id,
                project_id,
                rev,
                config_str,
                queued_cursor,
                // 新 run 与被续接 run 可追溯（postcondition：continuedFrom）。
                source_run_id,
                input_json,
                now,
                now
            ],
        )
        .map_err(sql_err)?;
        let max_pos: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(position), 0) FROM run_queue WHERE session_id = ?1",
                params![session_id],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        tx.execute(
            "INSERT INTO run_queue (session_id, position, run_id, disposition) VALUES (?1, ?2, ?3, 'normal')",
            params![session_id, max_pos + 1, run_id],
        )
        .map_err(sql_err)?;
        let new_qrev = bump_rev_in_tx(&tx)?;
        if start_immediately {
            tx.execute(
                "UPDATE sessions SET active_run_id = ?1, queue_revision = ?2 WHERE session_id = ?3",
                params![run_id, new_qrev, session_id],
            )
            .map_err(sql_err)?;
        } else {
            tx.execute(
                "UPDATE sessions SET queue_revision = ?1 WHERE session_id = ?2",
                params![new_qrev, session_id],
            )
            .map_err(sql_err)?;
        }

        let receipt = json!({
            "kind": "run",
            "runId": run_id,
            "durable": true,
            "acceptedRevision": rev,
            "deduplicated": false,
            "state": "queued",
        });

        if let Some(key) = idempotency_key {
            let resource_val = json!({"kind": "run", "runId": run_id});
            tx.execute(
                "INSERT INTO idempotency_records (idempotency_key, method_id, input_digest, request_state, reason, resource_json, receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
                 VALUES (?1, 'run.continue', ?2, 'accepted', 'receiptFound', ?3, ?4, NULL, ?5, ?6, 'doNotReplay')",
                params![
                    key,
                    digest,
                    serde_json::to_string(&resource_val).unwrap(),
                    serde_json::to_string(&receipt).unwrap(),
                    now,
                    retained_until
                ],
            )
            .map_err(sql_err)?;
        }

        tx.commit().map_err(sql_err)?;
        self.event_notifier().notify_waiters();
        self.state_notifier().notify_waiters();

        Ok(ContinueOutcome {
            receipt,
            run_id,
            start_worker: start_immediately,
        })
    }
}

/// Forwards currently pending steers for `(session_id, run_id)` into the live
/// `SessionOwner` turn mailbox, marking each steer `consumed` only once `enqueue`
/// accepts it (`runtime.rs::drive_owned_turn` integration hook).
pub(crate) async fn forward_pending_steers_for_owned_turn(
    store: &V1Store,
    owner: &crate::runtime::session::SessionOwner,
    session_id: &str,
    run_id: &str,
) -> usize {
    let mut forwarded = 0usize;
    let session_key = pilotdeck_protocol::JsString::from(session_id);
    let run_key = pilotdeck_protocol::JsString::from(run_id);
    while let Ok(Some(steer)) = store.peek_pending_steer(run_id) {
        let Some(info) = owner.active_steer_info(&session_key, &run_key) else {
            break;
        };
        let payload = json!({
            "role": "user",
            "content": steer.text(),
            "metadata": {
                "queueItemId": steer.steer_id,
                "purpose": "mid_turn_steer",
            },
        })
        .to_string();
        let message = pilotdeck_protocol::parse_json(&payload).unwrap_or_else(|_| {
            pilotdeck_protocol::JsValue::String(pilotdeck_protocol::JsString::from(
                steer.text().as_str(),
            ))
        });
        let msg = crate::runtime::steer::SteerMessage {
            item_id: pilotdeck_protocol::JsString::from(steer.steer_id.as_str()),
            message,
            allowed_read_files: None,
        };
        match info.enqueue(msg).await {
            Ok(res) if res.accepted => {
                if store
                    .mark_steer_consumed(run_id, &steer.steer_id)
                    .unwrap_or(false)
                {
                    forwarded += 1;
                } else {
                    break;
                }
            }
            _ => break,
        }
    }
    forwarded
}

fn source_not_interrupted(run_id: &str, state: &str) -> ContractError {
    ContractError::new(
        CODE_SOURCE_NOT_INTERRUPTED,
        format!(
            "source run `{run_id}` is `{state}`; only an interrupted (or acknowledged \
             result_unknown) run can be continued"
        ),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

fn revision_conflict(expected: &str, actual: &str) -> ContractError {
    ContractError::new(
        CODE_REVISION_CONFLICT,
        format!("expectedRevision `{expected}` does not match `{actual}`"),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

// ---------------------------------------------------------------------------
// Tests（store 直连 + dispatch 断言，`replacement.rs` 先例）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::runtime::DispatchError;
    use crate::public_v1::store::InstanceMeta;
    use crate::public_v1::test_support::TempDir;
    use agentcpm_pd_protocol::registry::OperationRegistry;
    use serde_json::json;

    fn open_runtime(root_dir: &std::path::Path) -> (std::sync::Arc<PublicV1Runtime>, String) {
        let (file, _token) = load_local_instance_and_token(root_dir, None).unwrap();
        let meta = InstanceMeta {
            instance_id: file.instance_id.clone(),
            owner_id: file.owner_id.clone(),
            product: file.product.clone(),
            protocol_version: file.protocol_version.clone(),
            node_id: file.node_id.clone(),
            generation: "gen-test-1".to_owned(),
            server_revision: "srv-test-1".to_owned(),
            initialization_revision: file.initialization_revision.clone(),
        };
        let store = V1Store::open_and_recover(root_dir, meta).unwrap();
        let rt = PublicV1Runtime::new(store);
        (rt, file.instance_id.clone())
    }

    fn contract_code(error: &DispatchError) -> String {
        match error {
            DispatchError::Application(e) => e.code.clone(),
            DispatchError::Rpc { data, .. } => data
                .as_ref()
                .and_then(|d| d["code"].as_str())
                .unwrap_or_default()
                .to_owned(),
        }
    }

    async fn seeded(rt: &std::sync::Arc<PublicV1Runtime>, iid: &str, tag: &str) -> String {
        rt.dispatch(
            "project.register",
            json!({
                "instanceId": iid,
                "projectId": format!("p-{tag}"),
                "name": "R",
                "environmentId": "local",
                "root": {"instanceId": iid, "rootId": "root-1", "relativePath": "ws"},
            }),
            None,
            None,
        )
        .await
        .unwrap();
        rt.dispatch(
            "session.create",
            json!({
                "instanceId": iid,
                "sessionId": format!("s-{tag}"),
                "projectId": format!("p-{tag}"),
            }),
            None,
            None,
        )
        .await
        .unwrap();
        format!("s-{tag}")
    }

    async fn submit_run(
        rt: &std::sync::Arc<PublicV1Runtime>,
        iid: &str,
        sid: &str,
        text: &str,
        key: &str,
    ) -> String {
        let (_, receipt) = rt
            .dispatch(
                "run.submit",
                json!({
                    "instanceId": iid,
                    "sessionId": sid,
                    "input": [{"kind": "text", "text": text}],
                }),
                Some(key),
                None,
            )
            .await
            .unwrap();
        receipt["runId"].as_str().unwrap().to_owned()
    }

    async fn run_state(rt: &std::sync::Arc<PublicV1Runtime>, iid: &str, run_id: &str) -> String {
        let (_, got) = rt
            .dispatch(
                "run.get",
                json!({"instanceId": iid, "runId": run_id}),
                None,
                None,
            )
            .await
            .unwrap();
        got["run"]["state"].as_str().unwrap().to_owned()
    }

    async fn wait_terminal(rt: &std::sync::Arc<PublicV1Runtime>, iid: &str, run_id: &str) -> Value {
        let (_, waited) = rt
            .dispatch(
                "run.wait",
                json!({"instanceId": iid, "runId": run_id, "timeoutMs": 8000}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            waited["conditionMet"], true,
            "run must reach terminal: {waited}"
        );
        waited["run"].clone()
    }

    fn steer_input(iid: &str, run_id: &str, steer_id: &str, text: &str) -> Value {
        json!({
            "instanceId": iid,
            "runId": run_id,
            "steerId": steer_id,
            "input": [{"kind": "text", "text": text}],
        })
    }

    /// 合同声明的错误码在 registry 错误清单内（防本地常量拼错）。
    #[test]
    fn declared_codes_exist_in_the_contract_error_lists() {
        let errors_of = |op: &str| {
            OperationRegistry::global()
                .get(op)
                .unwrap_or_else(|| panic!("{op} in registry"))
                .errors
                .clone()
        };
        for (op, code) in [
            ("steer.submit", CODE_RUN_TERMINAL),
            ("run.continue", CODE_SOURCE_NOT_INTERRUPTED),
            ("run.continue", CODE_UNRESOLVED_EFFECTS),
            ("steer.withdraw", CODE_ALREADY_CONSUMED),
        ] {
            assert!(
                errors_of(op).contains(&code.to_owned()),
                "`{code}` must be in the contract's `{op}` error list"
            );
        }
    }

    #[test]
    fn execution_and_retry_modes_match_the_contract() {
        let reg = OperationRegistry::global();
        for (op, execution, retry) in [
            ("steer.submit", "sync", "idempotentSubmit"),
            ("steer.get", "sync", "readOnly"),
            ("steer.withdraw", "sync", "conditionalWrite"),
            ("run.continue", "async", "idempotentSubmit"),
        ] {
            let spec = reg.get(op).unwrap();
            assert_eq!(spec.execution.as_str(), execution, "{op} execution");
            assert_eq!(spec.retry.as_str(), retry, "{op} retry");
        }
    }

    /// 完整生命周期：await_steer run 挂起 → steer.submit（pending/queued）
    /// → 驱动消费 → run 成功 → steer.get（consumed/succeeded）。
    /// 覆盖 acceptance「pending与consumed不同」「不支持零写入（行落库）」。
    #[tokio::test]
    async fn steer_lifecycle_pending_then_consumed_via_await_steer() {
        let tmp = TempDir::new("st-life");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "st1").await;

        let run_id = submit_run(&rt, &iid, &sid, "[pd-driver:await_steer]", "st-life-1").await;
        let (_, status) = rt
            .dispatch(
                "steer.submit",
                steer_input(&iid, &run_id, "steer-1", "change of plan"),
                Some("st-life-2"),
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("steer.submit", &status)
            .expect("steer.submit result matches the contract schema");
        assert_eq!(status["steerId"], "steer-1");
        assert_eq!(status["phase"], "pending");
        assert_eq!(status["state"], "queued");
        let rev = status["revision"].as_str().unwrap().to_owned();

        // 受理即持久行（「不支持零写入」）。
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            let (phase, state): (String, String) = conn
                .query_row(
                    "SELECT phase, state FROM steers WHERE run_id = ?1 AND steer_id = 'steer-1'",
                    params![run_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!((phase.as_str(), state.as_str()), ("pending", "queued"));
        }

        // 驱动消费引导并完成 run（消费先于终态落库，轮询到终态后必为
        // consumed；pending 的稳定读面在 sleep run 场景单独覆盖）。
        let run = wait_terminal(&rt, &iid, &run_id).await;
        assert_eq!(run["state"], "succeeded");

        let (_, after) = rt
            .dispatch(
                "steer.get",
                json!({"instanceId": iid, "runId": run_id, "steerId": "steer-1"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(after["phase"], "consumed", "{after}");
        assert_eq!(after["state"], "succeeded", "{after}");
        // 消费推进修订：读到的修订不同于受理时的修订。
        assert_ne!(after["revision"].as_str().unwrap(), rev);

        // 消费文本进入 run 事件流（run.output.delta 进度事件）。
        let (_, events) = rt
            .dispatch(
                "run.events",
                json!({"instanceId": iid, "runId": run_id, "page": {"limit": 100}}),
                None,
                None,
            )
            .await
            .unwrap();
        let texts: Vec<String> = events["frame"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == "run.output.delta")
            .filter_map(|e| e["data"]["text"].as_str().map(str::to_owned))
            .collect();
        assert!(
            texts.iter().any(|t| t.contains("change of plan")),
            "{texts:?}"
        );
    }

    /// 防重与终态闸：同 steerId 不同输入冲突；终态 run 拒 RUN_TERMINAL；
    /// 缺失 run 拒 NOT_FOUND；缺幂等键被 dispatch 层拒绝（-32086）；
    /// 幂等键不同输入冲突。sleep run 不消费引导，pending 读面稳定。
    #[tokio::test]
    async fn steer_submit_conflicts_and_terminal_gate() {
        let tmp = TempDir::new("st-conf");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "st2").await;

        // dispatch 层幂等键要求（retry=idempotentSubmit）。
        let err = rt
            .dispatch(
                "steer.submit",
                steer_input(&iid, "no-run", "s-x", "t"),
                None,
                None,
            )
            .await
            .unwrap_err();
        match &err {
            DispatchError::Rpc { code, .. } => assert_eq!(*code, -32086),
            other => panic!("expected -32086 missing idempotency key, got {other:?}"),
        }

        // 缺失 run。
        let err = rt
            .dispatch(
                "steer.submit",
                steer_input(&iid, "no-run", "s-x", "t"),
                Some("st-conf-0"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "NOT_FOUND");

        // sleep run：不消费引导，pending 状态在读侧稳定。
        let run_id = submit_run(&rt, &iid, &sid, "[pd-driver:sleep=60000]", "st-conf-1").await;
        let (_, first) = rt
            .dispatch(
                "steer.submit",
                steer_input(&iid, &run_id, "st-a", "first"),
                Some("st-conf-2"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(first["phase"], "pending");
        assert_eq!(first["state"], "queued");

        // 同 steerId 不同输入：IDEMPOTENCY_CONFLICT（「同ID不同输入冲突」）。
        let err = rt
            .dispatch(
                "steer.submit",
                steer_input(&iid, &run_id, "st-a", "different"),
                Some("st-conf-3"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "IDEMPOTENCY_CONFLICT");

        // 同幂等键不同语义输入：IDEMPOTENCY_CONFLICT（key 绑定规范化输入）。
        let err = rt
            .dispatch(
                "steer.submit",
                steer_input(&iid, &run_id, "st-b", "other"),
                Some("st-conf-2"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "IDEMPOTENCY_CONFLICT");

        // 同 steerId 同输入：幂等返回当前状态。
        let (_, replay) = rt
            .dispatch(
                "steer.submit",
                steer_input(&iid, &run_id, "st-a", "first"),
                Some("st-conf-4"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(replay["phase"], "pending");
        assert_eq!(replay["state"], "queued");
        assert_eq!(replay["revision"], first["revision"]);

        // requests.lookup 同 methodId/key 返回原资源。
        let (_, lookup) = rt
            .dispatch(
                "requests.lookup",
                json!({"instanceId": iid, "methodId": "steer.submit", "idempotencyKey": "st-conf-2"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(lookup["requestState"], "accepted");
        assert_eq!(lookup["resource"]["steerId"], "st-a");
        assert_eq!(lookup["resource"]["runId"].as_str().unwrap(), run_id);

        // 终态 run：RUN_TERMINAL（取消 sleep run 并等终态）。
        rt.dispatch(
            "run.cancel",
            json!({"instanceId": iid, "runId": run_id}),
            Some("st-conf-cancel"),
            None,
        )
        .await
        .unwrap();
        let cancelled = wait_terminal(&rt, &iid, &run_id).await;
        assert_eq!(cancelled["state"], "cancelled");
        let err = rt
            .dispatch(
                "steer.submit",
                steer_input(&iid, &run_id, "st-c", "late"),
                Some("st-conf-5"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "RUN_TERMINAL");

        // `st-conf-4` 也应已回填 idempotency_records。
        let (_, lookup_backfill) = rt
            .dispatch(
                "requests.lookup",
                json!({"instanceId": iid, "methodId": "steer.submit", "idempotencyKey": "st-conf-4"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(lookup_backfill["requestState"], "accepted");

        // 同幂等键在终态后重放原受理回执（严守 steer.submit schema：phase 仅 pending/consumed），
        // steer.get 读面反映真实的 rejected/cancelled，而新幂等键再提未消费引导拒为 RUN_TERMINAL。
        let (_, same_key_after) = rt
            .dispatch(
                "steer.submit",
                steer_input(&iid, &run_id, "st-a", "first"),
                Some("st-conf-2"),
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("steer.submit", &same_key_after)
            .expect("idempotent replay must satisfy steer.submit schema");
        assert_eq!(same_key_after["phase"], "pending", "{same_key_after}");

        let err = rt
            .dispatch(
                "steer.submit",
                steer_input(&iid, &run_id, "st-a", "first"),
                Some("st-conf-6"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "RUN_TERMINAL");
    }

    /// 未消费引导在 run 终态化后计算为 rejected（读时、不落库）；
    /// steer.get 对缺失/错误归属的引导返回 NOT_FOUND。
    #[tokio::test]
    async fn unconsumed_steer_reads_rejected_after_run_terminalizes() {
        let tmp = TempDir::new("st-rej");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "st3").await;

        // 普通驱动：2 秒后成功，不消费任何引导。
        let run_id = submit_run(&rt, &iid, &sid, "[pd-driver:sleep=2000]", "st-rej-1").await;
        let (_, status) = rt
            .dispatch(
                "steer.submit",
                steer_input(&iid, &run_id, "st-late", "ignored"),
                Some("st-rej-2"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(status["phase"], "pending");

        let run = wait_terminal(&rt, &iid, &run_id).await;
        assert_eq!(run["state"], "succeeded");

        let (_, got) = rt
            .dispatch(
                "steer.get",
                json!({"instanceId": iid, "runId": run_id, "steerId": "st-late"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(got["phase"], "rejected", "{got}");
        assert_eq!(got["state"], "succeeded", "{got}");
        OperationRegistry::global()
            .validate_result("steer.get", &got)
            .expect("rejected steer still matches the result schema");

        // 归属错误（steer 存在但 runId 不匹配）与缺失：NOT_FOUND。
        let err = rt
            .dispatch(
                "steer.get",
                json!({"instanceId": iid, "runId": "other-run", "steerId": "st-late"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "NOT_FOUND");
        let err = rt
            .dispatch(
                "steer.get",
                json!({"instanceId": iid, "runId": run_id, "steerId": "nope"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "NOT_FOUND");

        // rejected 是读时计算：存储行仍是 pending（steer.get 无业务状态修改）。
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            let phase: String = conn
                .query_row(
                    "SELECT phase FROM steers WHERE run_id = ?1 AND steer_id = 'st-late'",
                    params![run_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(phase, "pending");
        }
    }

    /// withdraw：pending 可撤（revision CAS）；已消费不可撤；修订过期冲突；
    /// 撤回后的引导不被误送。sleep run 保证 pending 稳定，await_steer run
    /// 提供确定性的消费面（「消费与撤销仅一方成功」的顺序面）。
    #[tokio::test]
    async fn steer_withdraw_semantics() {
        let tmp = TempDir::new("st-wd");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "st4").await;

        // sleep run：不消费引导，withdraw 的对象稳定 pending。
        let run_id = submit_run(&rt, &iid, &sid, "[pd-driver:sleep=60000]", "st-wd-1").await;
        let (_, first) = rt
            .dispatch(
                "steer.submit",
                steer_input(&iid, &run_id, "st-1", "one"),
                Some("st-wd-2"),
                None,
            )
            .await
            .unwrap();
        let rev = first["revision"].as_str().unwrap().to_owned();

        // 修订过期：REVISION_CONFLICT。
        let err = rt
            .dispatch(
                "steer.withdraw",
                json!({"instanceId": iid, "runId": run_id, "steerId": "st-1", "expectedRevision": "rev-bogus"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "REVISION_CONFLICT");

        // 正确修订：撤回成功，phase=withdrawn、state=cancelled。
        let (_, withdrawn) = rt
            .dispatch(
                "steer.withdraw",
                json!({"instanceId": iid, "runId": run_id, "steerId": "st-1", "expectedRevision": rev}),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("steer.withdraw", &withdrawn)
            .expect("withdraw result matches the contract schema");
        assert_eq!(withdrawn["phase"], "withdrawn");
        assert_eq!(withdrawn["state"], "cancelled");
        let rev2 = withdrawn["revision"].as_str().unwrap().to_owned();

        // 撤回后的引导不被误送：读侧可见 withdrawn，run 保持运行。
        let (_, s1) = rt
            .dispatch(
                "steer.get",
                json!({"instanceId": iid, "runId": run_id, "steerId": "st-1"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            (s1["phase"].as_str().unwrap(), s1["state"].as_str().unwrap()),
            ("withdrawn", "cancelled")
        );

        // 已撤回的幂等重放（携带撤回后的修订）。
        let (_, again) = rt
            .dispatch(
                "steer.withdraw",
                json!({"instanceId": iid, "runId": run_id, "steerId": "st-1", "expectedRevision": rev2}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(again["phase"], "withdrawn");
        assert_eq!(again["state"], "cancelled");

        // await_steer run：确定性消费面（独立会话——sleep run 占用中的会话
        // 会把新 run 排队，无法确定性驱动）。已消费不可撤。
        let sid2 = seeded(&rt, &iid, "st4b").await;
        let consume_id = submit_run(&rt, &iid, &sid2, "[pd-driver:await_steer]", "st-wd-3").await;
        let (_, second) = rt
            .dispatch(
                "steer.submit",
                steer_input(&iid, &consume_id, "st-2", "two"),
                Some("st-wd-4"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(second["phase"], "pending");
        let run = wait_terminal(&rt, &iid, &consume_id).await;
        assert_eq!(run["state"], "succeeded");

        let (_, s2) = rt
            .dispatch(
                "steer.get",
                json!({"instanceId": iid, "runId": consume_id, "steerId": "st-2"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            (s2["phase"].as_str().unwrap(), s2["state"].as_str().unwrap()),
            ("consumed", "succeeded")
        );

        // 已消费不可撤：ALREADY_CONSUMED。
        let err = rt
            .dispatch(
                "steer.withdraw",
                json!({"instanceId": iid, "runId": consume_id, "steerId": "st-2", "expectedRevision": s2["revision"].as_str().unwrap()}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "ALREADY_CONSUMED");

        // 缺失引导：NOT_FOUND。
        let err = rt
            .dispatch(
                "steer.withdraw",
                json!({"instanceId": iid, "runId": run_id, "steerId": "nope", "expectedRevision": rev2}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "NOT_FOUND");
    }

    /// run.continue 主线：interrupted 源 run → 新 run（continuedFrom 落库、
    /// runId 不同、旧 run 保持 interrupted）；幂等重放与 requests.lookup。
    #[tokio::test]
    async fn run_continue_from_interrupted_creates_traceable_new_run() {
        let tmp = TempDir::new("rc-cont");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "rc1").await;

        let source_id = submit_run(&rt, &iid, &sid, "[pd-driver:sleep=60000]", "rc-cont-1").await;
        // 等驱动把 run 推到 running，再用崩溃恢复同款终态标记打断它。
        for _ in 0..100 {
            if run_state(&rt, &iid, &source_id).await == "running" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        rt.store()
            .transition_run_state(
                &source_id,
                crate::public_v1::store::RunTransition::new(
                    RunState::Interrupted,
                    "run.interrupted",
                    &json!({"runId": source_id, "state": "interrupted", "reason": "gateway_restart"}),
                ),
            )
            .unwrap();
        let source = rt.store().get_run(&source_id).unwrap();
        assert_eq!(run_state_str(source.state), "interrupted");
        let source_rev = source.revision.clone();

        // 崩溃恢复同款会话暂停：QUEUE_PAUSED 先拒，queue.resume 后放行。
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "UPDATE sessions SET queue_paused = 1, pause_reason = 'gateway_restart' WHERE session_id = ?1",
                params![sid],
            )
            .unwrap();
        }
        let err = rt
            .dispatch(
                "run.continue",
                json!({"instanceId": iid, "sourceRunId": source_id, "expectedRevision": source_rev,
                       "input": [{"kind": "text", "text": "recovered prompt"}]}),
                Some("rc-cont-2"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "QUEUE_PAUSED");

        // queue.resume 需要队列修订与暂停原因（queue.get 读取后复验）。
        let (_, qget) = rt
            .dispatch(
                "queue.get",
                json!({"instanceId": iid, "sessionId": sid}),
                None,
                None,
            )
            .await
            .unwrap();
        let queue_rev = qget["queue"]["revision"].as_str().unwrap().to_owned();
        rt.dispatch(
            "queue.resume",
            json!({"instanceId": iid, "sessionId": sid, "expectedRevision": queue_rev,
                   "acknowledgedPauseReason": "gateway_restart"}),
            None,
            None,
        )
        .await
        .unwrap();

        let (outcome, receipt) = rt
            .dispatch(
                "run.continue",
                json!({"instanceId": iid, "sourceRunId": source_id, "expectedRevision": source_rev,
                       "input": [{"kind": "text", "text": "recovered prompt"}]}),
                Some("rc-cont-2"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome, ResponseOutcome::Accepted);
        OperationRegistry::global()
            .validate_result("run.continue", &receipt)
            .expect("run.continue receipt matches the contract schema");
        assert_eq!(receipt["kind"], "run");
        assert_eq!(receipt["durable"], true);
        assert_eq!(receipt["deduplicated"], false);
        assert_eq!(receipt["state"], "queued");
        let new_id = receipt["runId"].as_str().unwrap().to_owned();
        assert_ne!(
            new_id, source_id,
            "Gateway 分配的新 runId 必须不同于 sourceRunId"
        );

        // 不重放旧工具：新 run 输入只含显式 parts；旧 run 保持 interrupted。
        let new_run = wait_terminal(&rt, &iid, &new_id).await;
        assert_eq!(new_run["state"], "succeeded");
        assert_eq!(new_run["continuedFrom"].as_str().unwrap(), source_id);
        let stored_new = rt.store().get_run(&new_id).unwrap();
        assert_eq!(
            stored_new.input["input"][0]["text"].as_str().unwrap(),
            "recovered prompt"
        );
        assert_eq!(run_state(&rt, &iid, &source_id).await, "interrupted");
        let stored_source = rt.store().get_run(&source_id).unwrap();
        assert_eq!(
            stored_source.continued_from, None,
            "旧记录不覆盖：源 run 自身无 continuedFrom"
        );

        // 不误送继任 run：终态源 run 不再接受引导，继任 run 也不会收到它。
        let err = rt
            .dispatch(
                "steer.submit",
                steer_input(&iid, &source_id, "st-x", "for the old run"),
                Some("rc-cont-st"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "RUN_TERMINAL");
        let err = rt
            .dispatch(
                "steer.get",
                json!({"instanceId": iid, "runId": new_id, "steerId": "st-x"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "NOT_FOUND");

        // 同 key 同输入：去重回放（deduplicated=true，同一 runId）。
        let (_, replay) = rt
            .dispatch(
                "run.continue",
                json!({"instanceId": iid, "sourceRunId": source_id, "expectedRevision": source_rev,
                       "input": [{"kind": "text", "text": "recovered prompt"}]}),
                Some("rc-cont-2"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(replay["deduplicated"], true);
        assert_eq!(replay["runId"].as_str().unwrap(), new_id);

        // requests.lookup 同 methodId/key 返回原 Gateway 资源 ID。
        let (_, lookup) = rt
            .dispatch(
                "requests.lookup",
                json!({"instanceId": iid, "methodId": "run.continue", "idempotencyKey": "rc-cont-2"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(lookup["requestState"], "accepted");
        assert_eq!(lookup["resource"]["runId"].as_str().unwrap(), new_id);

        // 同 key 不同语义输入：IDEMPOTENCY_CONFLICT。
        let err = rt
            .dispatch(
                "run.continue",
                json!({"instanceId": iid, "sourceRunId": source_id, "expectedRevision": source_rev,
                       "input": [{"kind": "text", "text": "different prompt"}]}),
                Some("rc-cont-2"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "IDEMPOTENCY_CONFLICT");
    }

    /// run.continue 拒绝面：非 interrupted 源（状态闸先于修订闸）、缺失源、
    /// 修订过期。
    #[tokio::test]
    async fn run_continue_refuses_non_interrupted_and_stale_revision() {
        let tmp = TempDir::new("rc-ref");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "rc2").await;

        let done_id = submit_run(&rt, &iid, &sid, "plain prompt", "rc-ref-1").await;
        let done = wait_terminal(&rt, &iid, &done_id).await;
        assert_eq!(done["state"], "succeeded");
        let _done_rev = done["revision"].as_str().unwrap().to_owned();

        // 状态闸先于修订闸：非 interrupted 且修订过期 → SOURCE_NOT_INTERRUPTED。
        let err = rt
            .dispatch(
                "run.continue",
                json!({"instanceId": iid, "sourceRunId": done_id, "expectedRevision": "rev-bogus",
                       "input": [{"kind": "text", "text": "again"}]}),
                Some("rc-ref-2"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "SOURCE_NOT_INTERRUPTED");

        // 缺失源：NOT_FOUND。
        let err = rt
            .dispatch(
                "run.continue",
                json!({"instanceId": iid, "sourceRunId": "no-run", "expectedRevision": "rev-1",
                       "input": [{"kind": "text", "text": "again"}]}),
                Some("rc-ref-3"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "NOT_FOUND");

        // interrupted + 修订过期：REVISION_CONFLICT。
        let live_id = submit_run(&rt, &iid, &sid, "[pd-driver:sleep=60000]", "rc-ref-4").await;
        for _ in 0..100 {
            if run_state(&rt, &iid, &live_id).await == "running" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        rt.store()
            .transition_run_state(
                &live_id,
                crate::public_v1::store::RunTransition::new(
                    RunState::Interrupted,
                    "run.interrupted",
                    &json!({"runId": live_id, "state": "interrupted", "reason": "gateway_restart"}),
                ),
            )
            .unwrap();
        let err = rt
            .dispatch(
                "run.continue",
                json!({"instanceId": iid, "sourceRunId": live_id, "expectedRevision": "rev-bogus",
                       "input": [{"kind": "text", "text": "again"}]}),
                Some("rc-ref-5"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "REVISION_CONFLICT");
    }

    /// result_unknown 源：未处置未知效果拒绝 UNRESOLVED_EFFECTS；处置后
    /// （acknowledgedUnknownEffects 含 sourceRunId）受理，源保持 result_unknown。
    #[tokio::test]
    async fn run_continue_result_unknown_requires_acknowledgment() {
        let tmp = TempDir::new("rc-unk");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "rc3").await;

        let source_id = submit_run(&rt, &iid, &sid, "[pd-driver:sleep=60000]", "rc-unk-1").await;
        for _ in 0..100 {
            if run_state(&rt, &iid, &source_id).await == "running" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        rt.store()
            .transition_run_state(
                &source_id,
                crate::public_v1::store::RunTransition::new(
                    RunState::ResultUnknown,
                    "run.result_unknown",
                    &json!({"runId": source_id, "state": "result_unknown", "reason": "gateway_restart"}),
                ),
            )
            .unwrap();
        let source_rev = rt.store().get_run(&source_id).unwrap().revision;

        let err = rt
            .dispatch(
                "run.continue",
                json!({"instanceId": iid, "sourceRunId": source_id, "expectedRevision": source_rev,
                       "input": [{"kind": "text", "text": "after unknown"}]}),
                Some("rc-unk-2"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "UNRESOLVED_EFFECTS");
        if let DispatchError::Application(e) = &err {
            let details = e.details.as_ref().expect("unresolvedEffects details");
            assert_eq!(details["unresolvedEffects"][0].as_str().unwrap(), source_id);
        } else {
            panic!("expected application error, got {err:?}");
        }

        let (outcome, receipt) = rt
            .dispatch(
                "run.continue",
                json!({"instanceId": iid, "sourceRunId": source_id, "expectedRevision": source_rev,
                       "input": [{"kind": "text", "text": "after unknown"}],
                       "acknowledgedUnknownEffects": [source_id]}),
                Some("rc-unk-3"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome, ResponseOutcome::Accepted);
        let new_id = receipt["runId"].as_str().unwrap().to_owned();
        wait_terminal(&rt, &iid, &new_id).await;
        assert_eq!(run_state(&rt, &iid, &source_id).await, "result_unknown");
    }

    /// schema 拒绝客户端额外传入新 runId/operationId（additionalProperties:false，
    /// dispatch 层统一校验）——run.continue 与 steer.submit 两个入口。
    #[tokio::test]
    async fn schema_rejects_client_supplied_new_resource_ids() {
        let tmp = TempDir::new("st-schema");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "st5").await;

        let err = rt
            .dispatch(
                "run.continue",
                json!({"instanceId": iid, "sourceRunId": "whatever", "expectedRevision": "rev-1",
                       "input": [{"kind": "text", "text": "x"}], "runId": "client-chosen"}),
                Some("st-schema-1"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "INVALID_ARGUMENT");

        let run_id = submit_run(&rt, &iid, &sid, "[pd-driver:await_steer]", "st-schema-2").await;
        let err = rt
            .dispatch(
                "steer.submit",
                json!({"instanceId": iid, "runId": run_id, "steerId": "st-s", "operationId": "op-x",
                       "input": [{"kind": "text", "text": "x"}]}),
                Some("st-schema-3"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "INVALID_ARGUMENT");
    }

    #[tokio::test]
    async fn capability_unavailable_rejects_steer_with_zero_writes_and_peek_consume_works() {
        let tmp = TempDir::new("st-cap");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "stcap").await;

        let run_id = submit_run(&rt, &iid, &sid, "[pd-driver:sleep=60000]", "stcap-run").await;
        let err = rt
            .store()
            .submit_steer_with_capability(
                &steer_input(&iid, &run_id, "st-no-cap", "ignored"),
                Some("stcap-key-1"),
                false,
            )
            .unwrap_err();
        assert_eq!(err.code, CODE_CAPABILITY_UNAVAILABLE);

        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            let steers_cnt: i64 = conn
                .query_row("SELECT COUNT(*) FROM steers", [], |r| r.get(0))
                .unwrap();
            let idem_cnt: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM idempotency_records WHERE idempotency_key = 'stcap-key-1'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(steers_cnt, 0);
            assert_eq!(idem_cnt, 0);
        }

        // Verify peek + mark_steer_consumed primitives used by owned-turn forwarding.
        rt.store()
            .submit_steer_with_capability(
                &steer_input(&iid, &run_id, "st-peek-1", "hello forward"),
                Some("stcap-key-2"),
                true,
            )
            .unwrap();
        let peeked = rt
            .store()
            .peek_pending_steer(&run_id)
            .unwrap()
            .expect("pending steer");
        assert_eq!(peeked.steer_id, "st-peek-1");
        assert_eq!(peeked.text(), "hello forward");
        assert!(
            rt.store()
                .mark_steer_consumed(&run_id, "st-peek-1")
                .unwrap()
        );
        assert!(rt.store().peek_pending_steer(&run_id).unwrap().is_none());
    }

    #[tokio::test]
    async fn cancelling_gate_withdrawn_resubmit_and_terminal_withdraw_invariants() {
        let tmp = TempDir::new("st-invar");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "stinvar").await;

        let run_id = submit_run(&rt, &iid, &sid, "[pd-driver:sleep=60000]", "stinvar-r").await;
        // 1) Withdrawn steer submitted again under a NEW key refuses instead of
        //    emitting schema-invalid `phase = withdrawn`.
        let (_, submitted) = rt
            .dispatch(
                "steer.submit",
                steer_input(&iid, &run_id, "st-wd", "will withdraw"),
                Some("stinvar-s1"),
                None,
            )
            .await
            .unwrap();
        let rev = submitted["revision"].as_str().unwrap().to_owned();
        rt.dispatch(
            "steer.withdraw",
            json!({"instanceId": iid, "runId": run_id, "steerId": "st-wd", "expectedRevision": rev}),
            None,
            None,
        )
        .await
        .unwrap();
        let err = rt
            .dispatch(
                "steer.submit",
                steer_input(&iid, &run_id, "st-wd", "will withdraw"),
                Some("stinvar-s1-newkey"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "IDEMPOTENCY_CONFLICT");

        // Same original key replays schema-valid initial receipt.
        let (_, replay_same) = rt
            .dispatch(
                "steer.submit",
                steer_input(&iid, &run_id, "st-wd", "will withdraw"),
                Some("stinvar-s1"),
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("steer.submit", &replay_same)
            .expect("same-key replay must always satisfy steer.submit schema");

        // 2) Submit a second pending steer before cancelling.
        let (_, s2) = rt
            .dispatch(
                "steer.submit",
                steer_input(&iid, &run_id, "st-late-wd", "stay rejected"),
                Some("stinvar-s2"),
                None,
            )
            .await
            .unwrap();
        let rev2 = s2["revision"].as_str().unwrap().to_owned();

        // Simulate `cancelling` before terminal=1: fresh steer must fail with RUN_TERMINAL.
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "UPDATE runs SET state = 'cancelling' WHERE run_id = ?1",
                params![run_id],
            )
            .unwrap();
        }
        let err = rt
            .dispatch(
                "steer.submit",
                steer_input(&iid, &run_id, "st-cancelling", "too late"),
                Some("stinvar-s3"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "RUN_TERMINAL");

        // Now terminalize run (`terminal = 1`): withdraw of unconsumed pending steer
        // must fail with REVISION_CONFLICT and NOT mutate phase to withdrawn.
        rt.store()
            .transition_run_state(
                &run_id,
                crate::public_v1::store::RunTransition::new(
                    RunState::Cancelled,
                    "run.cancelled",
                    &json!({"runId": run_id, "state": "cancelled"}),
                ),
            )
            .unwrap();
        let err = rt
            .dispatch(
                "steer.withdraw",
                json!({"instanceId": iid, "runId": run_id, "steerId": "st-late-wd", "expectedRevision": rev2}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "REVISION_CONFLICT");

        let (_, still_rejected) = rt
            .dispatch(
                "steer.get",
                json!({"instanceId": iid, "runId": run_id, "steerId": "st-late-wd"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(still_rejected["phase"], "rejected");
    }

    #[tokio::test]
    async fn continue_run_bumps_queue_revision_ignores_queued_pin_and_checks_project() {
        let tmp = TempDir::new("rc-hooks");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "rch").await;

        let source_id = submit_run(&rt, &iid, &sid, "[pd-driver:sleep=60000]", "rch-src").await;
        for _ in 0..100 {
            if run_state(&rt, &iid, &source_id).await == "running" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        rt.store()
            .transition_run_state(
                &source_id,
                crate::public_v1::store::RunTransition::new(
                    RunState::Interrupted,
                    "run.interrupted",
                    &json!({"runId": source_id, "state": "interrupted", "reason": "gateway_restart"}),
                ),
            )
            .unwrap();
        let source_rev = rt.store().get_run(&source_id).unwrap().revision;

        // Read queue revision before continue, then continue: queue_revision must advance.
        let (_, q_before) = rt
            .dispatch(
                "queue.get",
                json!({"instanceId": iid, "sessionId": sid}),
                None,
                None,
            )
            .await
            .unwrap();
        let qrev_before = q_before["revision"].as_str().unwrap().to_owned();

        let (outcome, continued) = rt
            .dispatch(
                "run.continue",
                json!({
                    "instanceId": iid,
                    "sourceRunId": source_id,
                    "expectedRevision": source_rev,
                    "input": [{"kind": "text", "text": "resume once"}]
                }),
                Some("rch-cont-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome, ResponseOutcome::Accepted);
        let new_id = continued["runId"].as_str().unwrap().to_owned();

        let err = rt
            .dispatch(
                "queue.pause",
                json!({
                    "instanceId": iid,
                    "sessionId": sid,
                    "expectedRevision": qrev_before,
                    "reason": "stale_rev"
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "REVISION_CONFLICT");

        let settled = wait_terminal(&rt, &iid, &new_id).await;
        assert_eq!(settled["state"], "succeeded");

        // Remove project (simulate removal after interruption): run.continue fails NOT_FOUND.
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute("DELETE FROM projects WHERE project_id = 'p-rch'", [])
                .unwrap();
        }
        let err = rt
            .dispatch(
                "run.continue",
                json!({
                    "instanceId": iid,
                    "sourceRunId": source_id,
                    "expectedRevision": source_rev,
                    "input": [{"kind": "text", "text": "after remove"}]
                }),
                Some("rch-cont-after-remove"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "NOT_FOUND");
    }
}
