//! `queue.pause/move/remove/promote_to_steer` — 会话队列干预与转引导（`t010`）。
//!
//! 合同正本 `state/cli-v1-operations.json`（S2 已交付 `queue.get/resume`，归
//! `domains/queue.rs`；本文件只做四个干预操作）：
//!
//! - **`queue.pause`**（sync / conditionalWrite / owner:write）：CAS
//!   （`expectedRevision` = `sessions.queue_revision`，与 `queue.resume` 同一
//!   修订面）后置 `queue_paused=1` 与调用方的 `reason`。acceptance
//!   「当前run继续」：active run 完全不受影响（本操作只动 sessions 行，
//!   `runs.queue_paused` 是「run 被 pause 事件打断」的标记——`gateway_restart`
//!   恢复与失败暂停才会置位——用户暂停不打断任何 run，故不置位）；
//!   「后续并发入队不越过暂停边界」由 `store.rs::submit_run` 既有
//!   `QUEUE_PAUSED` 闸与 worker 的 paused 检查承担，本域不重复实现。
//! - **`queue.move`**（sync / conditionalWrite）：队列内重排。`beforeRunId`
//!   省略/null = **前移到队首**（`ui.ws-message:move-queued-input` 的重设计
//!   承接语义「前移待执行项」，见 `cli-v1-sources/ui_desktop.json`）；给出
//!   `beforeRunId` 则移动到该项之前（必须同队列，否则 NOT_FOUND）。
//!   `runId` 已启动（running/waiting/cancelling）→ `RUN_ALREADY_STARTED`
//!   （acceptance「running项拒绝」）。「移动不复制输入」：只重排
//!   `run_queue` 的 position，`runs.input_json` 原样不动。位置重排在一个
//!   事务内整段重写（`run_queue` 的 PK 是 `(session_id, position)`，原地
//!   换位会撞唯一约束）。
//! - **`queue.remove`**（sync / conditionalWrite）：移除排队 run——终态化为
//!   `cancelled`（`outcome_code='CANCELLED'`，镜像 `run.cancel` 的
//!   queued 分支），出队，清 `active_run_id` pin（wp4 review 先例）。排队
//!   run 从未到达任何模型 provider → acceptance「零模型请求」天然成立。
//!   running 项拒绝而非取消：`RUN_ALREADY_STARTED`。
//! - **`queue.promote_to_steer`**（sync / conditionalWrite）：把排队 run 原子
//!   转为 active run 的引导输入（`t009` 的 `steers` 表，复用 `steer.submit`
//!   的落库形状与消费原语 `take_pending_steer`）。postcondition「原子建立
//!   steer 与移除排队资格，不产生两次执行」：同一事务内插入 steer 行
//!   （`pending`）+ 排队 run 终态化（`cancelled` / `outcome_code=
//!   'CONVERTED_TO_STEER'`，事件 `run.cancelled` 载荷携带 `steerId`/
//!   `activeRunId` 保留转换来源）+ 出队 + 队列修订推进；失败路径在写之前
//!   全部拒绝，事务回滚（acceptance「目标结束或不支持时队列不变」）。
//!   `activeRunId` 必须是本会话当前 active run 且非终态（终态 →
//!   `RUN_TERMINAL`「目标结束」；非当前 active → `INVALID_ARGUMENT`）。
//!
//! ## 合同未明确处的实现选择（显式登记，供集成者复核）
//!
//! 1. **错误清单未含 NOT_FOUND**：queue.pause 与 queue.promote_to_steer 的
//!    合同 errors 数组没有 NOT_FOUND，但资源缺失必须可报告——沿用 S2
//!    `queue.resume` 先例（其错误清单同样没有 NOT_FOUND，实现返回
//!    NOT_FOUND），缺失会话/run 一律 NOT_FOUND。
//! 2. **已暂停会话再 pause**：覆写 pause_reason 并推进修订（最近一次暂停
//!    生效；`queue.resume` 按**当前** reason 确认）。与 `gateway_restart`
//!    暂停的交互：用户 pause 覆写恢复原因后，resume 需确认新原因。
//! 3. **promote 的已启动排队项**：合同错误清单无 RUN_ALREADY_STARTED，但
//!    「已启动的排队 run 不能再转引导」与 move/remove 是同一条件；沿用
//!    兄弟操作的精确码 RUN_ALREADY_STARTED（本文件测试只断言声明过的码）。
//! 4. **`steerId` 重复**：同 `(activeRunId, steerId)` 已存在 →
//!    INVALID_ARGUMENT（重试安全由 expectedRevision CAS 承担：成功后队列
//!    修订已推进，重放先在 CAS 拒绝）。
//! 5. **转换来源的保留形式**：合同只说「保留转换来源」。实现为三项持久
//!    记录——run 的 `outcome_code='CONVERTED_TO_STEER'`、`run.cancelled`
//!    事件载荷（`steerId`/`activeRunId`）、`steers` 行本身（承载原输入）。
//!    排队行本身出队：终态 run 留在 `run_queue` 会让既有 worker 的
//!    `list_pending_runs_for_session`/`get_queue` 取件循环空转（共享路径，
//!    不改），且 S2 以来一切终态转换都出队（`transition_run_state` 不变量）。
//!    因此 `QueueItem.queueDisposition` 枚举中的 `convertedToSteer` 值在
//!    本实现中不会出现在队列里（schema 允许而非要求）。
//! 6. **paused 会话上的 promote**：TS 基线 steer 拒绝 paused 队列，合同
//!    未声明 QUEUE_PAUSED——按合同允许（active run 的引导与队列调度无关）。
//! 7. **move 的锚可以是 running 头**：acceptance 只要求 beforeRunId 同队列。
//!    锚在 running 头上时，移动项成为下一顺位（worker 在当前 run 终态化后
//!    取 position 最小的 queued 项）。
//!
//! ## input.follow（本任务 CLI 侧操作）
//!
//! `execution='local'`（`agentcpm-pd-cli/src/local_ops/input_follow.rs`），
//! gateway 侧只登记可用性（`domains/input.rs` 对 `input.submit` 的同一先例），
//! `/v1/ws` 拒绝 local 操作在前。本文件不含其逻辑。

use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::envelope::compute_input_digest;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_REVISION_CONFLICT, ContractError, EffectStatus, RetrySpec,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::steer::CODE_RUN_TERMINAL;
use crate::public_v1::domains::workspace_dirs::{bump_rev_in_tx, domain_not_found, sql_err};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, now_rfc3339};

/// 合同声明而协议 crate 尚无常量的错误码（queue.move/remove/promote：
/// 「running项拒绝」/ 已启动的排队项拒绝）。
pub(crate) const CODE_RUN_ALREADY_STARTED: &str = "RUN_ALREADY_STARTED";

/// `queue.remove` 成功后的返回。
pub(crate) struct RemoveQueueItemOutcome {
    /// 终态化后的 run 合同 JSON。
    pub run: Value,
    /// 推进后的队列修订。
    pub queue_revision: String,
}

/// `queue.promote_to_steer` 成功后的返回。
pub(crate) struct PromoteToSteerOutcome {
    pub steer_id: String,
    /// 转换后（cancelled / CONVERTED_TO_STEER）的源 run 合同 JSON。
    pub queued_run: Value,
    pub queue_revision: String,
}

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.handler("queue.pause", |rt, input, _key, rev| {
        Box::pin(queue_pause(rt, input, rev))
    });
    reg.handler("queue.move", |rt, input, _key, rev| {
        Box::pin(queue_move(rt, input, rev))
    });
    reg.handler("queue.remove", |rt, input, _key, rev| {
        Box::pin(queue_remove(rt, input, rev))
    });
    reg.handler("queue.promote_to_steer", |rt, input, _key, rev| {
        Box::pin(queue_promote_to_steer(rt, input, rev))
    });
    // input.follow 是 execution='local' 的 CLI 组合操作（本任务的 CLI 侧
    // 落点）；/v1/ws 在 dispatch 前置就拒绝 local 操作，这里只登记可用性，
    // 保持 `system.capabilities` 与实现集合真实（input.submit 先例）。
    reg.available("input.follow");
}

/// envelope 级 expectedRevision 优先，缺省回落到 params 内（`queue.resume` /
/// `steer.withdraw` 先例）；schema 已保证 params.expectedRevision 必填。
fn effective_revision(input: &Value, envelope_rev: Option<&str>) -> String {
    envelope_rev
        .or_else(|| input.get("expectedRevision").and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned()
}

async fn queue_pause(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    expected_revision: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    Ok((
        ResponseOutcome::Succeeded,
        rt.store().pause_queue(
            input["sessionId"].as_str().unwrap_or_default(),
            &effective_revision(&input, expected_revision.as_deref()),
            input["reason"].as_str().unwrap_or_default(),
        )?,
    ))
}

async fn queue_move(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    expected_revision: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    Ok((
        ResponseOutcome::Succeeded,
        rt.store().move_queue_item(
            input["sessionId"].as_str().unwrap_or_default(),
            input["runId"].as_str().unwrap_or_default(),
            &effective_revision(&input, expected_revision.as_deref()),
            input.get("beforeRunId").and_then(Value::as_str),
        )?,
    ))
}

async fn queue_remove(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    expected_revision: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let outcome = rt.store().remove_queue_item(
        input["sessionId"].as_str().unwrap_or_default(),
        input["runId"].as_str().unwrap_or_default(),
        &effective_revision(&input, expected_revision.as_deref()),
    )?;
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "run": outcome.run,
            "queueRevision": outcome.queue_revision,
        }),
    ))
}

async fn queue_promote_to_steer(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    expected_revision: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let outcome = rt.store().promote_queued_run_to_steer(
        input["sessionId"].as_str().unwrap_or_default(),
        input["queuedRunId"].as_str().unwrap_or_default(),
        input["activeRunId"].as_str().unwrap_or_default(),
        input["steerId"].as_str().unwrap_or_default(),
        &effective_revision(&input, expected_revision.as_deref()),
    )?;
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "steerId": outcome.steer_id,
            "queuedRun": outcome.queued_run,
            "queueRevision": outcome.queue_revision,
        }),
    ))
}

// ---------------------------------------------------------------------------
// Store 方法（S3 底座模式：写在领域文件内，`store.rs` 不动）
// ---------------------------------------------------------------------------

/// `(queue_revision, session_state)` — 会话的队列面。
type SessionQueueFace = (String, String);

fn load_queue_face_in_tx(
    tx: &Connection,
    session_id: &str,
) -> Result<SessionQueueFace, ContractError> {
    let row: Option<SessionQueueFace> = tx
        .query_row(
            "SELECT queue_revision, session_state FROM sessions WHERE session_id = ?1",
            params![session_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(sql_err)?;
    match row {
        // 删除态会话与缺失同码（S2 `check_session_admission` 的 NOT_FOUND 面）。
        Some((rev, state)) if state != "deleted" => Ok((rev, state)),
        _ => Err(domain_not_found("session", session_id)),
    }
}

fn ensure_queue_revision(expected: &str, actual: &str) -> Result<(), ContractError> {
    if expected != actual {
        return Err(ContractError::new(
            CODE_REVISION_CONFLICT,
            format!("queue expectedRevision `{expected}` does not match `{actual}`"),
            RetrySpec::after_reconcile(),
            EffectStatus::NotApplied,
        ));
    }
    Ok(())
}

/// `(run_id, state, disposition)` — 一条排队行（JOIN runs 取实时状态，
/// 与 `build_queue_json` 同一查询面）。
type QueueSnapshotItem = (String, String, String);

fn load_queue_items_in_tx(
    tx: &Connection,
    session_id: &str,
) -> Result<Vec<QueueSnapshotItem>, ContractError> {
    let mut stmt = tx
        .prepare(
            "SELECT q.run_id, r.state, q.disposition
             FROM run_queue q
             JOIN runs r ON r.run_id = q.run_id
             WHERE q.session_id = ?1
             ORDER BY q.position ASC",
        )
        .map_err(sql_err)?;
    let items = stmt
        .query_map(params![session_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .map_err(sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err)?;
    Ok(items)
}

/// 「不在本会话队列」的错误分诊：run 存在、属本会话且非终态 → 它已启动
/// （running/waiting/cancelling，`RUN_ALREADY_STARTED`）；不存在 / 他会话 /
/// 已终态出队 → NOT_FOUND（不再是排队项）。
fn missing_queue_item_error(conn: &Connection, session_id: &str, run_id: &str) -> ContractError {
    let row: Option<(String, i64)> = conn
        .query_row(
            "SELECT session_id, terminal FROM runs WHERE run_id = ?1",
            params![run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .ok()
        .flatten();
    match row {
        Some((sid, terminal)) if sid == session_id && terminal == 0 => run_already_started(run_id),
        _ => domain_not_found("run", run_id),
    }
}

fn run_already_started(run_id: &str) -> ContractError {
    ContractError::new(
        CODE_RUN_ALREADY_STARTED,
        format!("run `{run_id}` has already started; only a queued run can be repositioned"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

/// run 事件追加（`store.rs::append_event_in_tx` 的领域内镜像，
/// `steer.rs::append_run_event_in_tx` 同一先例），返回确定游标。
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
    /// `queue.pause`：CAS 后置位暂停与调用方 reason，返回最新队列。
    pub(crate) fn pause_queue(
        &self,
        session_id: &str,
        expected_revision: &str,
        reason: &str,
    ) -> Result<Value, ContractError> {
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(sql_err)?;
        let (queue_revision, _) = load_queue_face_in_tx(&tx, session_id)?;
        ensure_queue_revision(expected_revision, &queue_revision)?;
        let new_rev = bump_rev_in_tx(&tx)?;
        tx.execute(
            "UPDATE sessions SET queue_paused = 1, pause_reason = ?1, queue_revision = ?2
             WHERE session_id = ?3",
            params![reason, new_rev, session_id],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        drop(conn);
        // 唤醒队列观察者（run.wait 式轮询 / queue.get 轮询）。
        self.state_notifier().notify_waiters();
        self.get_queue(session_id, None)
    }

    /// `queue.move`：CAS 后整段重排 `run_queue`，返回最新队列。
    pub(crate) fn move_queue_item(
        &self,
        session_id: &str,
        run_id: &str,
        expected_revision: &str,
        before_run_id: Option<&str>,
    ) -> Result<Value, ContractError> {
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(sql_err)?;
        let (queue_revision, _) = load_queue_face_in_tx(&tx, session_id)?;
        ensure_queue_revision(expected_revision, &queue_revision)?;

        let items = load_queue_items_in_tx(&tx, session_id)?;
        let position_of = |id: &str| items.iter().position(|(rid, _, _)| rid == id);
        let Some(src) = position_of(run_id) else {
            return Err(missing_queue_item_error(&tx, session_id, run_id));
        };
        // acceptance「running项拒绝」：只有仍处 queued 的排队项可重排。
        if items[src].1 != "queued" {
            return Err(run_already_started(run_id));
        }
        if let Some(anchor) = before_run_id.filter(|a| !a.is_empty()) {
            if anchor == run_id {
                return Err(ContractError::new(
                    CODE_INVALID_ARGUMENT,
                    "runId and beforeRunId refer to the same run; nothing to move".to_owned(),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            // acceptance「beforeRunId必须同队列」。
            if position_of(anchor).is_none() {
                return Err(domain_not_found("run", anchor));
            }
        }

        // 计算新顺序：摘下移动项，插到锚（或队首）之前。
        let mut order: Vec<(String, String)> = items
            .iter()
            .map(|(rid, _, disp)| (rid.clone(), disp.clone()))
            .collect();
        let moved = order.remove(src);
        let insert_at = match before_run_id.filter(|a| !a.is_empty()) {
            // 省略锚 = 前移（move-queued-input 的「前移」承接）。
            None => 0usize,
            Some(anchor) => order
                .iter()
                .position(|(rid, _)| rid == anchor)
                .expect("anchor verified above"),
        };
        order.insert(insert_at, moved);

        // PK (session_id, position) 不能原地换位：事务内整段重写。
        tx.execute(
            "DELETE FROM run_queue WHERE session_id = ?1",
            params![session_id],
        )
        .map_err(sql_err)?;
        for (idx, (rid, disp)) in order.iter().enumerate() {
            tx.execute(
                "INSERT INTO run_queue (session_id, position, run_id, disposition)
                 VALUES (?1, ?2, ?3, ?4)",
                params![session_id, idx as i64 + 1, rid, disp],
            )
            .map_err(sql_err)?;
        }
        let new_rev = bump_rev_in_tx(&tx)?;
        tx.execute(
            "UPDATE sessions SET queue_revision = ?1 WHERE session_id = ?2",
            params![new_rev, session_id],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        drop(conn);
        self.state_notifier().notify_waiters();
        self.get_queue(session_id, None)
    }

    /// `queue.remove`：CAS 后把排队 run 终态化为 cancelled 并出队。
    pub(crate) fn remove_queue_item(
        &self,
        session_id: &str,
        run_id: &str,
        expected_revision: &str,
    ) -> Result<RemoveQueueItemOutcome, ContractError> {
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(sql_err)?;
        let (queue_revision, _) = load_queue_face_in_tx(&tx, session_id)?;
        ensure_queue_revision(expected_revision, &queue_revision)?;

        let items = load_queue_items_in_tx(&tx, session_id)?;
        let Some(src) = items.iter().position(|(rid, _, _)| rid == run_id) else {
            return Err(missing_queue_item_error(&tx, session_id, run_id));
        };
        // acceptance「running项拒绝而非取消」。
        if items[src].1 != "queued" {
            return Err(run_already_started(run_id));
        }

        // 终态化（镜像 `run.cancel` 的 queued 分支形状；排队 run 零模型请求）。
        let now = now_rfc3339();
        let rev = bump_rev_in_tx(&tx)?;
        let cursor = append_run_event_in_tx(
            &tx,
            run_id,
            session_id,
            "run.cancelled",
            &json!({"runId": run_id, "reason": "removed_from_queue"}),
            &now,
        )?;
        tx.execute(
            "UPDATE runs SET state = 'cancelled', cancel_requested = 1, revision = ?1,
             terminal = 1, outcome_code = 'CANCELLED', final_cursor = ?2, updated_at = ?3
             WHERE run_id = ?4",
            params![rev, cursor, now, run_id],
        )
        .map_err(sql_err)?;
        tx.execute("DELETE FROM run_queue WHERE run_id = ?1", params![run_id])
            .map_err(sql_err)?;
        // queue.resume 会把 queued 头 pin 成 active_run_id；移除该 pin
        // （wp4 review 的 run.cancel 同款 scoped 清理），否则后续提交永远排队。
        tx.execute(
            "UPDATE sessions SET active_run_id = NULL WHERE session_id = ?1 AND active_run_id = ?2",
            params![session_id, run_id],
        )
        .map_err(sql_err)?;
        let new_qrev = bump_rev_in_tx(&tx)?;
        tx.execute(
            "UPDATE sessions SET queue_revision = ?1 WHERE session_id = ?2",
            params![new_qrev, session_id],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        drop(conn);
        self.event_notifier().notify_waiters();
        self.state_notifier().notify_waiters();
        let run = self.get_run(run_id)?;
        Ok(RemoveQueueItemOutcome {
            run: run.to_contract_json(),
            queue_revision: new_qrev,
        })
    }

    /// `queue.promote_to_steer`：一个事务内原子完成校验、steer 建立与排队
    /// 资格移除（postcondition「原子建立steer与移除排队资格」）。
    pub(crate) fn promote_queued_run_to_steer(
        &self,
        session_id: &str,
        queued_run_id: &str,
        active_run_id: &str,
        steer_id: &str,
        expected_revision: &str,
    ) -> Result<PromoteToSteerOutcome, ContractError> {
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(sql_err)?;
        let (queue_revision, _) = load_queue_face_in_tx(&tx, session_id)?;
        ensure_queue_revision(expected_revision, &queue_revision)?;

        // 1. queuedRunId：必须在队列且仍处 queued。
        let items = load_queue_items_in_tx(&tx, session_id)?;
        let Some(src) = items.iter().position(|(rid, _, _)| rid == queued_run_id) else {
            return Err(missing_queue_item_error(&tx, session_id, queued_run_id));
        };
        if items[src].1 != "queued" {
            return Err(run_already_started(queued_run_id));
        }

        // 2. activeRunId：目标必须存在、非终态、且是本会话当前 active run。
        let active_run: Option<(String, String, i64)> = tx
            .query_row(
                "SELECT session_id, state, terminal FROM runs WHERE run_id = ?1",
                params![active_run_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(sql_err)?;
        let Some((ar_session, ar_state, ar_terminal)) = active_run else {
            return Err(domain_not_found("run", active_run_id));
        };
        // acceptance「目标结束…时队列不变」。
        if ar_terminal != 0 {
            return Err(ContractError::new(
                CODE_RUN_TERMINAL,
                format!(
                    "target run `{active_run_id}` is terminal ({ar_state}); \
                     a queued run can only be steered into a live run"
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        let pinned_active: Option<String> = tx
            .query_row(
                "SELECT active_run_id FROM sessions WHERE session_id = ?1",
                params![session_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(sql_err)?
            .flatten();
        if ar_session != session_id || pinned_active.as_deref() != Some(active_run_id) {
            return Err(ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!(
                    "activeRunId `{active_run_id}` is not the active run of session `{session_id}`"
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }

        // 3. steerId 防重（同 (activeRunId, steerId)）。
        let exists: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM steers WHERE run_id = ?1 AND steer_id = ?2",
                params![active_run_id, steer_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if exists.is_some() {
            return Err(ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!(
                    "steerId `{steer_id}` is already used for run `{active_run_id}`; \
                     pick a different steerId"
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }

        // 4. 原子转换。steer 行复用 `steer.submit` 的落库形状（`steer.rs`
        //    先例：submit_steer 自持锁不能在本事务内调用，形状由测试对拍）；
        //    digest 按 steer.submit 的输入形态计算，使后续同内容 steer.submit
        //    幂等重放而不是冲突。
        let queued_input_json: Option<String> = tx
            .query_row(
                "SELECT input_json FROM runs WHERE run_id = ?1",
                params![queued_run_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        let queued_input: Value = queued_input_json
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Null);
        let parts = queued_input.get("input").cloned().unwrap_or(json!([]));
        let steer_input = json!({
            "instanceId": self.meta().instance_id,
            "runId": active_run_id,
            "steerId": steer_id,
            "input": parts,
        });
        let digest = compute_input_digest("steer.submit", &steer_input);
        let now = now_rfc3339();
        let steer_rev = bump_rev_in_tx(&tx)?;
        tx.execute(
            "INSERT INTO steers (run_id, steer_id, session_id, instance_id, input_json, input_digest, phase, state, revision, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', 'queued', ?7, ?8, ?9)",
            params![
                active_run_id,
                steer_id,
                session_id,
                self.meta().instance_id,
                serde_json::to_string(&parts).unwrap_or_else(|_| "[]".into()),
                digest,
                steer_rev,
                now,
                now
            ],
        )
        .map_err(sql_err)?;

        // 源 run 终态化：cancelled + CONVERTED_TO_STEER（转换来源保留）。
        let run_rev = bump_rev_in_tx(&tx)?;
        let cursor = append_run_event_in_tx(
            &tx,
            queued_run_id,
            session_id,
            "run.cancelled",
            &json!({
                "runId": queued_run_id,
                "reason": "converted_to_steer",
                "steerId": steer_id,
                "activeRunId": active_run_id,
            }),
            &now,
        )?;
        tx.execute(
            "UPDATE runs SET state = 'cancelled', revision = ?1, terminal = 1,
             outcome_code = 'CONVERTED_TO_STEER', final_cursor = ?2, updated_at = ?3
             WHERE run_id = ?4",
            params![run_rev, cursor, now, queued_run_id],
        )
        .map_err(sql_err)?;
        tx.execute(
            "DELETE FROM run_queue WHERE run_id = ?1",
            params![queued_run_id],
        )
        .map_err(sql_err)?;
        // 防御：被 resume pin 成 active 的 queued 头被转换时清 pin（同 remove）。
        tx.execute(
            "UPDATE sessions SET active_run_id = NULL WHERE session_id = ?1 AND active_run_id = ?2",
            params![session_id, queued_run_id],
        )
        .map_err(sql_err)?;
        let new_qrev = bump_rev_in_tx(&tx)?;
        tx.execute(
            "UPDATE sessions SET queue_revision = ?1 WHERE session_id = ?2",
            params![new_qrev, session_id],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        drop(conn);
        // 唤醒 await_steer 驱动与队列观察者。
        self.event_notifier().notify_waiters();
        self.state_notifier().notify_waiters();
        let run = self.get_run(queued_run_id)?;
        Ok(PromoteToSteerOutcome {
            steer_id: steer_id.to_owned(),
            queued_run: run.to_contract_json(),
            queue_revision: new_qrev,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests（store 直连 + dispatch 断言，`steer.rs` / `replacement.rs` 先例）
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

    async fn wait_running(rt: &std::sync::Arc<PublicV1Runtime>, iid: &str, run_id: &str) {
        for _ in 0..200 {
            if run_state(rt, iid, run_id).await == "running" {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("run `{run_id}` never reached running");
    }

    async fn queue_state(rt: &std::sync::Arc<PublicV1Runtime>, iid: &str, sid: &str) -> Value {
        let (_, got) = rt
            .dispatch(
                "queue.get",
                json!({"instanceId": iid, "sessionId": sid}),
                None,
                None,
            )
            .await
            .unwrap();
        got["queue"].clone()
    }

    fn queue_run_ids(queue: &Value) -> Vec<String> {
        queue["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["runId"].as_str().unwrap().to_owned())
            .collect()
    }

    fn fresh_home(tag: &str) -> (TempDir, std::path::PathBuf) {
        let tmp = TempDir::new(tag);
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        (tmp, root_dir)
    }

    /// 合同声明的错误码在 registry 错误清单内（防本地常量拼错；NOT_FOUND 的
    /// 未声明使用沿用 queue.resume 先例，不在此断言）。
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
            ("queue.move", CODE_RUN_ALREADY_STARTED),
            ("queue.remove", CODE_RUN_ALREADY_STARTED),
            ("queue.promote_to_steer", CODE_RUN_ALREADY_STARTED),
            ("queue.promote_to_steer", CODE_RUN_TERMINAL),
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
            ("queue.pause", "sync", "conditionalWrite"),
            ("queue.move", "sync", "conditionalWrite"),
            ("queue.remove", "sync", "conditionalWrite"),
            ("queue.promote_to_steer", "sync", "conditionalWrite"),
            ("input.follow", "local", "nonReplayable"),
        ] {
            let spec = reg.get(op).unwrap();
            assert_eq!(spec.execution.as_str(), execution, "{op} execution");
            assert_eq!(spec.retry.as_str(), retry, "{op} retry");
        }
        // input.follow 在 gateway 侧只有可用性行（execution='local' 不挂
        // /v1/ws handler——domains/mod.rs 的 S2 基线测试按此断言）。
        let domains = DomainRegistry::global();
        assert!(domains.is_registered("input.follow"));
        assert!(domains.handler_for("input.follow").is_none());
    }

    /// pause 主线：当前 run 继续、后续入队被 QUEUE_PAUSED 拒、resume 按
    /// 当前 reason 确认；gateway_restart 暂停被用户 pause 覆写。
    #[tokio::test]
    async fn queue_pause_boundaries_and_reason_ownership() {
        let (_tmp, root_dir) = fresh_home("qa-pause");
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "qa1").await;

        let head = submit_run(&rt, &iid, &sid, "[pd-driver:sleep=60000]", "qa-pause-h").await;
        wait_running(&rt, &iid, &head).await;
        let queued = submit_run(&rt, &iid, &sid, "queued while head runs", "qa-pause-q").await;

        let queue = queue_state(&rt, &iid, &sid).await;
        assert_eq!(queue["paused"], false);
        let qrev = queue["revision"].as_str().unwrap().to_owned();

        // 修订过期先拒（零副作用）。
        let err = rt
            .dispatch(
                "queue.pause",
                json!({"instanceId": iid, "sessionId": sid, "expectedRevision": "rev-bogus",
                       "reason": "user_hold"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "REVISION_CONFLICT");

        let (_, paused) = rt
            .dispatch(
                "queue.pause",
                json!({"instanceId": iid, "sessionId": sid, "expectedRevision": qrev,
                       "reason": "user_hold"}),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("queue.pause", &paused)
            .expect("queue.pause result matches the contract schema");
        assert_eq!(paused["queue"]["paused"], true);
        assert_eq!(paused["queue"]["pauseReason"], "user_hold");
        let paused_rev = paused["queue"]["revision"].as_str().unwrap().to_owned();
        assert_ne!(paused_rev, qrev);

        // acceptance「当前run继续」。
        assert_eq!(run_state(&rt, &iid, &head).await, "running");

        // acceptance「后续并发入队不越过暂停边界」。
        let err = rt
            .dispatch(
                "run.submit",
                json!({"instanceId": iid, "sessionId": sid,
                       "input": [{"kind": "text", "text": "nope"}]}),
                Some("qa-pause-x"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "QUEUE_PAUSED");

        // resume（头 run 仍在运行——queue.resume 不为非空 pin 另起 worker，
        // 驱动头 run 的既有 worker 在解除暂停后自然接续队列）。已知的
        // 交互缝（头 run 在暂停中终态化后再 resume 会搁浅队列）见本文件
        // 模块文档与交付报告——属 S2 worker/resume 机器，本域不改。
        rt.dispatch(
            "queue.resume",
            json!({"instanceId": iid, "sessionId": sid, "expectedRevision": paused_rev,
                   "acknowledgedPauseReason": "user_hold"}),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(run_state(&rt, &iid, &head).await, "running");

        // 收尾：取消头 run，排队项接续执行。
        rt.dispatch(
            "run.cancel",
            json!({"instanceId": iid, "runId": head}),
            Some("qa-pause-cancel"),
            None,
        )
        .await
        .unwrap();
        let cancelled = wait_terminal(&rt, &iid, &head).await;
        assert_eq!(cancelled["state"], "cancelled");
        let done = wait_terminal(&rt, &iid, &queued).await;
        assert_eq!(done["state"], "succeeded");

        // 缺失会话：NOT_FOUND（queue.resume 同款未声明先例）。
        let err = rt
            .dispatch(
                "queue.pause",
                json!({"instanceId": iid, "sessionId": "s-nope", "expectedRevision": "rev-1",
                       "reason": "x"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "NOT_FOUND");
    }

    /// gateway_restart 暂停上的用户 pause：覆写 reason，resume 只认新 reason。
    #[tokio::test]
    async fn queue_pause_overwrites_the_recovery_pause_reason() {
        let (_tmp, root_dir) = fresh_home("qa-pause2");
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "qa2").await;

        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "UPDATE sessions SET queue_paused = 1, pause_reason = 'gateway_restart' WHERE session_id = ?1",
                params![sid],
            )
            .unwrap();
        }
        let queue = queue_state(&rt, &iid, &sid).await;
        let qrev = queue["revision"].as_str().unwrap().to_owned();

        let (_, paused) = rt
            .dispatch(
                "queue.pause",
                json!({"instanceId": iid, "sessionId": sid, "expectedRevision": qrev,
                       "reason": "user_hold"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(paused["queue"]["pauseReason"], "user_hold");

        // resume 确认的是当前（覆写后）原因；旧原因不再被接受。
        let new_rev = paused["queue"]["revision"].as_str().unwrap().to_owned();
        let err = rt
            .dispatch(
                "queue.resume",
                json!({"instanceId": iid, "sessionId": sid, "expectedRevision": new_rev,
                       "acknowledgedPauseReason": "gateway_restart"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "INVALID_ARGUMENT");
        rt.dispatch(
            "queue.resume",
            json!({"instanceId": iid, "sessionId": sid, "expectedRevision": new_rev,
                   "acknowledgedPauseReason": "user_hold"}),
            None,
            None,
        )
        .await
        .unwrap();
    }

    /// move 主线：锚定重排、无锚前移、输入不被复制/改动、顺序被 worker 遵守。
    #[tokio::test]
    async fn queue_move_reorders_without_touching_inputs() {
        let (_tmp, root_dir) = fresh_home("qa-move");
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "qa3").await;

        let head = submit_run(&rt, &iid, &sid, "[pd-driver:sleep=60000]", "qa-move-h").await;
        wait_running(&rt, &iid, &head).await;
        let a = submit_run(
            &rt,
            &iid,
            &sid,
            "item-a [pd-driver:sleep=2000]",
            "qa-move-a",
        )
        .await;
        let b = submit_run(
            &rt,
            &iid,
            &sid,
            "item-b [pd-driver:sleep=60000]",
            "qa-move-b",
        )
        .await;

        // b 移到 a 之前：[head, b, a]。
        let qrev = queue_state(&rt, &iid, &sid).await["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        let (_, moved) = rt
            .dispatch(
                "queue.move",
                json!({"instanceId": iid, "sessionId": sid, "runId": b,
                       "expectedRevision": qrev, "beforeRunId": a}),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("queue.move", &moved)
            .expect("queue.move result matches the contract schema");
        assert_eq!(
            queue_run_ids(&moved["queue"]),
            vec![head.clone(), b.clone(), a.clone()]
        );
        let positions: Vec<i64> = moved["queue"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["position"].as_i64().unwrap())
            .collect();
        assert_eq!(positions, vec![1, 2, 3]);

        // 无锚 = 前移到队首：[a, head, b]。
        let qrev2 = moved["queue"]["revision"].as_str().unwrap().to_owned();
        let (_, front) = rt
            .dispatch(
                "queue.move",
                json!({"instanceId": iid, "sessionId": sid, "runId": a,
                       "expectedRevision": qrev2, "beforeRunId": null}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            queue_run_ids(&front["queue"]),
            vec![a.clone(), head.clone(), b.clone()]
        );

        // acceptance「移动不复制输入」：a 的输入原样未动。
        let (_, got) = rt
            .dispatch(
                "run.get",
                json!({"instanceId": iid, "runId": a}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            got["run"]["state"], "queued",
            "移动不得启动或改动 run 的生命周期"
        );

        // 顺序被串行 worker 遵守：取消头后 a（队首）先于 b 执行。
        rt.dispatch(
            "run.cancel",
            json!({"instanceId": iid, "runId": head}),
            Some("qa-move-cancel"),
            None,
        )
        .await
        .unwrap();
        let a_done = wait_terminal(&rt, &iid, &a).await;
        assert_eq!(a_done["state"], "succeeded");
        // b 只能在 a 终态后才可能启动（串行队列）：此刻 b 尚未成功。
        assert_ne!(run_state(&rt, &iid, &b).await, "succeeded");
        rt.dispatch(
            "run.cancel",
            json!({"instanceId": iid, "runId": b}),
            Some("qa-move-cancel-b"),
            None,
        )
        .await
        .unwrap();
        let b_done = wait_terminal(&rt, &iid, &b).await;
        assert_eq!(b_done["state"], "cancelled");
    }

    /// move 拒绝面：running 拒、缺失/终态 NOT_FOUND、跨会话锚 NOT_FOUND、
    /// 自锚 INVALID_ARGUMENT、修订过期 REVISION_CONFLICT。
    #[tokio::test]
    async fn queue_move_refusals() {
        let (_tmp, root_dir) = fresh_home("qa-move2");
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "qa4").await;
        let other = seeded(&rt, &iid, "qa4b").await;

        let head = submit_run(&rt, &iid, &sid, "[pd-driver:sleep=60000]", "qa-mv2-h").await;
        wait_running(&rt, &iid, &head).await;
        let a = submit_run(&rt, &iid, &sid, "a", "qa-mv2-a").await;
        let gone = submit_run(&rt, &iid, &sid, "gone", "qa-mv2-g").await;
        let foreign = submit_run(&rt, &iid, &other, "foreign", "qa-mv2-f").await;

        let qrev = queue_state(&rt, &iid, &sid).await["revision"]
            .as_str()
            .unwrap()
            .to_owned();

        // running 项拒绝。
        let err = rt
            .dispatch(
                "queue.move",
                json!({"instanceId": iid, "sessionId": sid, "runId": head,
                       "expectedRevision": qrev, "beforeRunId": a}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "RUN_ALREADY_STARTED");

        // 自锚。
        let err = rt
            .dispatch(
                "queue.move",
                json!({"instanceId": iid, "sessionId": sid, "runId": a,
                       "expectedRevision": qrev, "beforeRunId": a}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "INVALID_ARGUMENT");

        // 跨会话锚：NOT_FOUND（acceptance「beforeRunId必须同队列」）。
        let err = rt
            .dispatch(
                "queue.move",
                json!({"instanceId": iid, "sessionId": sid, "runId": a,
                       "expectedRevision": qrev, "beforeRunId": foreign}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "NOT_FOUND");

        // 缺失 run。
        let err = rt
            .dispatch(
                "queue.move",
                json!({"instanceId": iid, "sessionId": sid, "runId": "no-run",
                       "expectedRevision": qrev}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "NOT_FOUND");

        // 已终态（已出队）的 run：NOT_FOUND。
        rt.dispatch(
            "run.cancel",
            json!({"instanceId": iid, "runId": gone}),
            Some("qa-mv2-cancel"),
            None,
        )
        .await
        .unwrap();
        wait_terminal(&rt, &iid, &gone).await;
        let err = rt
            .dispatch(
                "queue.move",
                json!({"instanceId": iid, "sessionId": sid, "runId": gone,
                       "expectedRevision": qrev}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "NOT_FOUND");

        // 修订过期。
        let err = rt
            .dispatch(
                "queue.move",
                json!({"instanceId": iid, "sessionId": sid, "runId": a,
                       "expectedRevision": "rev-bogus", "beforeRunId": null}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "REVISION_CONFLICT");

        // 缺失会话。
        let err = rt
            .dispatch(
                "queue.move",
                json!({"instanceId": iid, "sessionId": "s-nope", "runId": a,
                       "expectedRevision": qrev}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "NOT_FOUND");
    }

    /// remove 主线：queued → cancelled（零模型请求：事件里没有 run.started）、
    /// 出队、头 run 与其他排队项不受影响。
    #[tokio::test]
    async fn queue_remove_cancels_queued_and_refuses_running() {
        let (_tmp, root_dir) = fresh_home("qa-rm");
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "qa5").await;

        let head = submit_run(&rt, &iid, &sid, "[pd-driver:sleep=60000]", "qa-rm-h").await;
        wait_running(&rt, &iid, &head).await;
        let a = submit_run(&rt, &iid, &sid, "a", "qa-rm-a").await;
        let b = submit_run(&rt, &iid, &sid, "b", "qa-rm-b").await;

        let qrev = queue_state(&rt, &iid, &sid).await["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        let (_, removed) = rt
            .dispatch(
                "queue.remove",
                json!({"instanceId": iid, "sessionId": sid, "runId": a,
                       "expectedRevision": qrev}),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("queue.remove", &removed)
            .expect("queue.remove result matches the contract schema");
        assert_eq!(removed["run"]["state"], "cancelled");
        assert_eq!(removed["run"]["terminal"], true);
        assert_eq!(removed["run"]["outcomeCode"], "CANCELLED");
        let new_qrev = removed["queueRevision"].as_str().unwrap().to_owned();
        assert_ne!(new_qrev, qrev);

        // 出队；头与其他排队项不受影响。
        let queue = queue_state(&rt, &iid, &sid).await;
        assert_eq!(queue_run_ids(&queue), vec![head.clone(), b.clone()]);
        assert_eq!(queue["revision"].as_str().unwrap(), new_qrev);
        assert_eq!(run_state(&rt, &iid, &head).await, "running");

        // acceptance「queued变cancelled且零模型请求」：a 的事件只有
        // run.queued + run.cancelled，绝无 run.started。
        let (_, events) = rt
            .dispatch(
                "run.events",
                json!({"instanceId": iid, "runId": a, "page": {"limit": 100}}),
                None,
                None,
            )
            .await
            .unwrap();
        let types: Vec<&str> = events["frame"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["type"].as_str())
            .collect();
        assert_eq!(types, vec!["run.queued", "run.cancelled"], "{types:?}");

        // acceptance「running项拒绝而非取消」。
        let err = rt
            .dispatch(
                "queue.remove",
                json!({"instanceId": iid, "sessionId": sid, "runId": head,
                       "expectedRevision": new_qrev}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "RUN_ALREADY_STARTED");
        assert_eq!(run_state(&rt, &iid, &head).await, "running");

        // 修订过期与缺失。
        let err = rt
            .dispatch(
                "queue.remove",
                json!({"instanceId": iid, "sessionId": sid, "runId": b,
                       "expectedRevision": "rev-bogus"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "REVISION_CONFLICT");
        let err = rt
            .dispatch(
                "queue.remove",
                json!({"instanceId": iid, "sessionId": sid, "runId": "no-run",
                       "expectedRevision": new_qrev}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "NOT_FOUND");
    }

    /// promote 主线：排队项转 active run 的引导——源 run 终态化
    /// （CONVERTED_TO_STEER、不再独立执行、无 run.started），steer 被目标
    /// run 消费（输入到达目标 run），队列与修订正确推进。
    #[tokio::test]
    async fn queue_promote_converts_queued_run_into_live_steer() {
        let (_tmp, root_dir) = fresh_home("qa-promo");
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "qa6").await;

        let head = submit_run(&rt, &iid, &sid, "[pd-driver:await_steer]", "qa-po-h").await;
        wait_running(&rt, &iid, &head).await;
        let a = submit_run(&rt, &iid, &sid, "promote me", "qa-po-a").await;

        let qrev = queue_state(&rt, &iid, &sid).await["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        let (_, promoted) = rt
            .dispatch(
                "queue.promote_to_steer",
                json!({"instanceId": iid, "sessionId": sid, "queuedRunId": a,
                       "activeRunId": head, "expectedRevision": qrev, "steerId": "st-promo"}),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("queue.promote_to_steer", &promoted)
            .expect("queue.promote_to_steer result matches the contract schema");
        assert_eq!(promoted["steerId"], "st-promo");
        assert_eq!(promoted["queuedRun"]["state"], "cancelled");
        assert_eq!(promoted["queuedRun"]["terminal"], true);
        assert_eq!(promoted["queuedRun"]["outcomeCode"], "CONVERTED_TO_STEER");
        let new_qrev = promoted["queueRevision"].as_str().unwrap().to_owned();
        assert_ne!(new_qrev, qrev);

        // acceptance「成功后原项不再独立执行」：出队且从未启动（头 run 消费
        // 与否与该断言无关，只断 a）。
        let queue = queue_state(&rt, &iid, &sid).await;
        let items_after = queue_run_ids(&queue);
        assert!(
            !items_after.contains(&a),
            "转换后的排队项必须出队: {items_after:?}"
        );
        let (_, events) = rt
            .dispatch(
                "run.events",
                json!({"instanceId": iid, "runId": a, "page": {"limit": 100}}),
                None,
                None,
            )
            .await
            .unwrap();
        let types: Vec<&str> = events["frame"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["type"].as_str())
            .collect();
        assert_eq!(types, vec!["run.queued", "run.cancelled"], "{types:?}");

        // 目标 run 消费转换来的引导：await_steer 唤醒并携带原文完成。
        let head_done = wait_terminal(&rt, &iid, &head).await;
        assert_eq!(head_done["state"], "succeeded");
        let (_, steer_status) = rt
            .dispatch(
                "steer.get",
                json!({"instanceId": iid, "runId": head, "steerId": "st-promo"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(steer_status["phase"], "consumed", "{steer_status}");
        assert_eq!(steer_status["state"], "succeeded", "{steer_status}");
        let (_, head_events) = rt
            .dispatch(
                "run.events",
                json!({"instanceId": iid, "runId": head, "page": {"limit": 100}}),
                None,
                None,
            )
            .await
            .unwrap();
        let texts: Vec<String> = head_events["frame"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == "run.output.delta")
            .filter_map(|e| e["data"]["text"].as_str().map(str::to_owned))
            .collect();
        assert!(
            texts.iter().any(|t| t.contains("promote me")),
            "转换输入必须到达目标 run: {texts:?}"
        );
    }

    /// promote 拒绝面：目标终态（队列不变）、已启动排队项、activeRunId 非当前
    /// active、steerId 重复、缺失、修订过期；paused 会话上的 promote 按合同
    /// 允许（未声明 QUEUE_PAUSED）。
    #[tokio::test]
    async fn queue_promote_refusals_keep_queue_unchanged() {
        let (_tmp, root_dir) = fresh_home("qa-promo2");
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "qa7").await;
        let other = seeded(&rt, &iid, "qa7b").await;

        let head = submit_run(&rt, &iid, &sid, "[pd-driver:await_steer]", "qa-po2-h").await;
        wait_running(&rt, &iid, &head).await;
        let a = submit_run(&rt, &iid, &sid, "a", "qa-po2-a").await;
        let b = submit_run(&rt, &iid, &sid, "b", "qa-po2-b").await;
        // 另一会话里一个已终态的 run（目标结束场景）。
        let finished = submit_run(&rt, &iid, &other, "quick", "qa-po2-f").await;
        wait_terminal(&rt, &iid, &finished).await;

        let qrev = queue_state(&rt, &iid, &sid).await["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        let before_ids = {
            let q = queue_state(&rt, &iid, &sid).await;
            queue_run_ids(&q)
        };

        // acceptance「目标结束…队列不变」。
        let err = rt
            .dispatch(
                "queue.promote_to_steer",
                json!({"instanceId": iid, "sessionId": sid, "queuedRunId": a,
                       "activeRunId": finished, "expectedRevision": qrev, "steerId": "st-x"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "RUN_TERMINAL");

        // 已启动的排队项（头 run 本身）。
        let err = rt
            .dispatch(
                "queue.promote_to_steer",
                json!({"instanceId": iid, "sessionId": sid, "queuedRunId": head,
                       "activeRunId": head, "expectedRevision": qrev, "steerId": "st-y"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "RUN_ALREADY_STARTED");

        // activeRunId 不是本会话当前 active run（给了另一个排队项）。
        let err = rt
            .dispatch(
                "queue.promote_to_steer",
                json!({"instanceId": iid, "sessionId": sid, "queuedRunId": a,
                       "activeRunId": b, "expectedRevision": qrev, "steerId": "st-z"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "INVALID_ARGUMENT");

        // steerId 重复与缺失。
        let err = rt
            .dispatch(
                "queue.promote_to_steer",
                json!({"instanceId": iid, "sessionId": sid, "queuedRunId": a,
                       "activeRunId": "no-run", "expectedRevision": qrev, "steerId": "st-w"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "NOT_FOUND");
        let err = rt
            .dispatch(
                "queue.promote_to_steer",
                json!({"instanceId": iid, "sessionId": sid, "queuedRunId": "no-run",
                       "activeRunId": head, "expectedRevision": qrev, "steerId": "st-v"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "NOT_FOUND");

        // 修订过期。
        let err = rt
            .dispatch(
                "queue.promote_to_steer",
                json!({"instanceId": iid, "sessionId": sid, "queuedRunId": a,
                       "activeRunId": head, "expectedRevision": "rev-bogus", "steerId": "st-u"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "REVISION_CONFLICT");

        // 上述全部失败路径：队列与修订零变化。
        let after = queue_state(&rt, &iid, &sid).await;
        assert_eq!(queue_run_ids(&after), before_ids);
        assert_eq!(after["revision"].as_str().unwrap(), qrev);
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            let steer_rows: i64 = conn
                .query_row("SELECT COUNT(*) FROM steers", [], |r| r.get(0))
                .unwrap();
            assert_eq!(steer_rows, 0, "失败路径不得留下 steer 行");
        }

        // paused 会话上的 promote：合同未声明 QUEUE_PAUSED，按合同允许。
        rt.dispatch(
            "queue.pause",
            json!({"instanceId": iid, "sessionId": sid, "expectedRevision": qrev,
                   "reason": "user_hold"}),
            None,
            None,
        )
        .await
        .unwrap();
        let paused_rev = queue_state(&rt, &iid, &sid).await["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        let (_, promoted) = rt
            .dispatch(
                "queue.promote_to_steer",
                json!({"instanceId": iid, "sessionId": sid, "queuedRunId": a,
                       "activeRunId": head, "expectedRevision": paused_rev, "steerId": "st-1"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(promoted["steerId"], "st-1");
        // 出队断言只看 a 与 b（头 run 消费引导后随时可能终态化出队）。
        let items_after = queue_run_ids(&queue_state(&rt, &iid, &sid).await);
        assert!(
            !items_after.contains(&a) && items_after.contains(&b),
            "paused promote 出队 a、保留 b: {items_after:?}"
        );

        // 同内容 steer.submit 幂等重放（digest 对拍）：promote 写入的 steer 输入
        // 就是排队项 a 的输入（text "a"），同 (runId, steerId, 输入) 的
        // steer.submit 返回当前引导状态而不是冲突（终态 run 上重提交先过
        // steerId 防重，不误报 RUN_TERMINAL——t009 同款序）。
        let (_, replay) = rt
            .dispatch(
                "steer.submit",
                json!({"instanceId": iid, "runId": head, "steerId": "st-1",
                       "input": [{"kind": "text", "text": "a"}]}),
                Some("qa-po2-replay"),
                None,
            )
            .await;
        match replay {
            Ok((_, status)) => {
                assert_eq!(status["steerId"], "st-1", "{status}");
            }
            Err(err) => panic!("同内容 steer.submit 必须与 promote 的 steer 行幂等: {err:?}"),
        }

        // steerId 重复（同 (activeRunId, steerId)）：用 sleep 头做稳定目标
        // （不消费引导、60s 内存活）。先恢复调度并等 b 走完。
        let resume_rev = queue_state(&rt, &iid, &sid).await["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        rt.dispatch(
            "queue.resume",
            json!({"instanceId": iid, "sessionId": sid, "expectedRevision": resume_rev,
                   "acknowledgedPauseReason": "user_hold"}),
            None,
            None,
        )
        .await
        .unwrap();
        let b_done = wait_terminal(&rt, &iid, &b).await;
        assert_eq!(b_done["state"], "succeeded");
        let sleep_head = submit_run(&rt, &iid, &sid, "[pd-driver:sleep=60000]", "qa-po2-sh").await;
        wait_running(&rt, &iid, &sleep_head).await;
        let q3 = submit_run(&rt, &iid, &sid, "q3", "qa-po2-q3").await;
        let q4 = submit_run(&rt, &iid, &sid, "q4", "qa-po2-q4").await;
        let dup_rev = queue_state(&rt, &iid, &sid).await["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        rt.dispatch(
            "queue.promote_to_steer",
            json!({"instanceId": iid, "sessionId": sid, "queuedRunId": q3,
                   "activeRunId": sleep_head, "expectedRevision": dup_rev, "steerId": "st-dup"}),
            None,
            None,
        )
        .await
        .unwrap();
        // 第一个 promote 已推进队列修订：重读后再试同 steerId。
        let dup_rev2 = queue_state(&rt, &iid, &sid).await["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        let err = rt
            .dispatch(
                "queue.promote_to_steer",
                json!({"instanceId": iid, "sessionId": sid, "queuedRunId": q4,
                       "activeRunId": sleep_head, "expectedRevision": dup_rev2,
                       "steerId": "st-dup"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(
            contract_code(&err),
            "INVALID_ARGUMENT",
            "steerId 重复必须拒绝"
        );
    }
}
