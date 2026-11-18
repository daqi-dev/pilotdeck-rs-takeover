//! `replacement.commit` — 末轮替换事务的提交侧（`t007`）。
//!
//! 合同正本 `state/cli-v1-operations.json#replacement.commit`：
//! - 两段式第二跳。`replacement.prepare`（history 任务）先「冻结候选边界并
//!   保存恢复原件」，**不截断可见历史**（prepare 合同 acceptance）；本操作在
//!   **一个 SQLite 事务**里完成「历史替换 + Gateway 分配的新 run 持久受理」
//!   （postcondition：「历史替换与Gateway分配的新run持久受理同一事务」），
//!   并保留旧历史恢复原件（backup 留存在 plan 行，供恢复路径使用）。
//! - `expectedRevision` 复验（REVISION_CONFLICT）；plan 不在 prepared 态或
//!   已过 TTL → REPLACEMENT_EXPIRED；prepare 冻结的边界不再是会话末轮 →
//!   TURN_CONFLICT（「过期expectedTurnId拒绝替换」）。
//! - 幂等：`retry=idempotentSubmit`。key 绑定 methodId + 规范化语义输入；
//!   同 key 同输入去重回放原回执（「受理后丢响应同幂等key唯一」「重试不重复
//!   截断」），同 key 不同输入 IDEMPOTENCY_CONFLICT。缺失/过期两类确定性
//!   拒绝按 run.submit 先例持久登记，其余（REVISION/TURN/SESSION/QUEUE 类）
//!   不登记——它们是 after_reconcile 语义，重试按当时事实重新判定。
//! - schema（additionalProperties:false）已拒绝客户端额外传入新 runId/
//!   operationId；instanceId 一致性由 dispatch 的 bind_instance_id 在副作用前
//!   完成（INSTANCE_MISMATCH，目标两端零业务副作用）。
//! - 提交后取消新 run 不回滚已确认替换（使用 run.cancel）。
//!
//! ## 与 run 机器的缝合（集成者接线点）
//!
//! - 新 run 的受理完全复刻 `store.rs::submit_run` 的落库形状（runs/run_queue/
//!   events/幂等记录/active_run_id 指针），因为 `submit_run` 自持锁无法在本
//!   事务内复用；两处形状由本文件的测试对拍 `run.submit` 回执保证一致。
//! - `start_worker` 为真时 handler 调 `rt.start_session_worker(run_id)`——与
//!   `domains/run.rs::run_submit` 相同的唤醒点（空闲会话立即驱动；占用中会话
//!   由既有 worker 循环在结束后按 run_queue 顺序领取）。
//! - 会话准入（closed→SESSION_CLOSED、deleted/missing→NOT_FOUND、queue
//!   paused→QUEUE_PAUSED）在事务内内联镜像 `session_admin::check_session_admission`
//!   与 `submit_run` 的队列检查（该 pub 方法自持锁，不能在本事务内调用）。
//!
//! ## replacement_plans 表（history 任务复用约定）
//!
//! prepare 侧（history 任务的 `replacement.prepare` handler）调用本文件的
//! [`V1Store::freeze_replacement_plan`] 存储原语完成冻结；`replacement.get` /
//! `replacement.rollback` 直接读写同一张表。列约定见 DDL 注释。回滚
//! （prepared 撤销）= 删除/标记 plan 行——历史从未被截断，无需恢复动作。

use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::envelope::compute_input_digest;
use agentcpm_pd_protocol::errors::{
    CODE_ALREADY_EXISTS, CODE_IDEMPOTENCY_CONFLICT, CODE_INVALID_ARGUMENT, CODE_NOT_FOUND,
    CODE_QUEUE_PAUSED, CODE_REVISION_CONFLICT, ContractError, EffectStatus, RetrySpec,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::session_admin::CODE_SESSION_CLOSED;
use crate::public_v1::domains::workspace_dirs::{
    CODE_ACTIVE_RESOURCES, append_domain_event_in_tx, bump_rev_in_tx, domain_not_found, sql_err,
};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, now_rfc3339};

/// 合同声明而协议 crate 尚无常量的错误码。`TURN_CONFLICT` 同时是 prepare
/// 侧（history 任务）可复用的拒绝码。
pub(crate) const CODE_REPLACEMENT_EXPIRED: &str = "REPLACEMENT_EXPIRED";
pub(crate) const CODE_TURN_CONFLICT: &str = "TURN_CONFLICT";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema(
        "replacement_plans",
        "CREATE TABLE IF NOT EXISTS replacement_plans (
            replacement_id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            instance_id TEXT NOT NULL,
            replaced_turn_id TEXT NOT NULL,
            boundary_seq INTEGER NOT NULL,
            backup_json TEXT NOT NULL,
            state TEXT NOT NULL,
            revision TEXT NOT NULL,
            expires_at TEXT,
            committed_run_id TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_replacement_plans_session
            ON replacement_plans(session_id);",
    );
    reg.handler("replacement.commit", |rt, input, key, rev| {
        Box::pin(replacement_commit(rt, input, key, rev))
    });
}

async fn replacement_commit(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
    expected_revision: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    // envelope 级 expectedRevision 优先，缺省回落到 params 内（interaction.rs
    // 先例）；schema 已保证 params.expectedRevision 必填。
    let effective = expected_revision
        .as_deref()
        .or_else(|| input.get("expectedRevision").and_then(Value::as_str));
    let outcome = rt
        .store()
        .commit_replacement(&input, effective, idempotency_key.as_deref())?;
    if outcome.start_worker {
        rt.start_session_worker(outcome.run_id.clone());
    }
    Ok((ResponseOutcome::Accepted, outcome.receipt))
}

/// [`V1Store::commit_replacement`] 的返回。
pub(crate) struct ReplacementCommitOutcome {
    /// run 形态 receipt（与 `run.submit` 同构）。
    pub receipt: Value,
    pub run_id: String,
    /// 会话空闲（无 active、无 queued）时立即驱动新 run。
    pub start_worker: bool,
}

impl V1Store {
    /// `replacement.commit`：一个事务内完成历史截断、plan 置为 committed、
    /// 新 run 受理（runs/run_queue/events/幂等记录）。
    pub(crate) fn commit_replacement(
        &self,
        input: &Value,
        expected_revision: Option<&str>,
        idempotency_key: Option<&str>,
    ) -> Result<ReplacementCommitOutcome, ContractError> {
        let replacement_id = input["replacementId"].as_str().unwrap_or_default();
        let parts = input.get("input").cloned().unwrap_or(Value::Null);
        let effective_rev = expected_revision.unwrap_or_default();
        let digest = compute_input_digest("replacement.commit", input);
        let now = now_rfc3339();
        let retained_until = (chrono::Utc::now() + chrono::Duration::days(7)).to_rfc3339();

        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(sql_err)?;

        // 1. 幂等闸（先于一切资源查找，run.submit 同序）。
        if let Some(key) = idempotency_key {
            let existing: Option<(String, String, Option<String>, Option<String>)> = tx
                .query_row(
                    "SELECT input_digest, request_state, receipt_json, rejection_code
                     FROM idempotency_records WHERE idempotency_key = ?1 AND method_id = 'replacement.commit'",
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
                            "idempotencyKey `{key}` already used for replacement.commit with different input"
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
                    return Ok(ReplacementCommitOutcome {
                        receipt,
                        run_id: String::new(),
                        start_worker: false,
                    });
                }
                return Err(ContractError::new(
                    rej_code.unwrap_or_else(|| CODE_INVALID_ARGUMENT.to_owned()),
                    "request previously rejected definitively",
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
        }

        // 2. plan 查找与状态复验。
        #[allow(clippy::type_complexity)]
        let plan: Option<(String, String, i64, String, String, Option<String>, Option<String>)> = tx
            .query_row(
                "SELECT session_id, replaced_turn_id, boundary_seq, state, revision, expires_at, committed_run_id
                 FROM replacement_plans WHERE replacement_id = ?1",
                params![replacement_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .optional()
            .map_err(sql_err)?;
        let Some((
            session_id,
            replaced_turn_id,
            boundary_seq,
            state,
            plan_rev,
            expires_at,
            _committed_run,
        )) = plan
        else {
            if let Some(key) = idempotency_key {
                record_commit_rejection(&tx, key, &digest, CODE_NOT_FOUND, &now, &retained_until)?;
                tx.commit().map_err(sql_err)?;
            }
            return Err(domain_not_found("replacement", replacement_id));
        };

        let expired = |message: String| {
            ContractError::new(
                CODE_REPLACEMENT_EXPIRED,
                message,
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        };
        if state != "prepared" {
            if let Some(key) = idempotency_key {
                record_commit_rejection(
                    &tx,
                    key,
                    &digest,
                    CODE_REPLACEMENT_EXPIRED,
                    &now,
                    &retained_until,
                )?;
                tx.commit().map_err(sql_err)?;
            }
            return Err(expired(format!(
                "replacement `{replacement_id}` is `{state}`; only a prepared replacement can be committed"
            )));
        }
        if let Some(raw) = &expires_at {
            // 解析失败的 expires_at 视为过期（宁拒不错）。
            let still_valid =
                chrono::DateTime::parse_from_rfc3339(raw)
                    .ok()
                    .is_some_and(|expires| {
                        chrono::Utc::now()
                            .signed_duration_since(expires)
                            .num_milliseconds()
                            < 0
                    });
            if !still_valid {
                if let Some(key) = idempotency_key {
                    record_commit_rejection(
                        &tx,
                        key,
                        &digest,
                        CODE_REPLACEMENT_EXPIRED,
                        &now,
                        &retained_until,
                    )?;
                    tx.commit().map_err(sql_err)?;
                }
                return Err(expired(format!(
                    "replacement `{replacement_id}` expired at {raw}"
                )));
            }
        }
        if plan_rev != effective_rev {
            return Err(ContractError::new(
                CODE_REVISION_CONFLICT,
                format!(
                    "replacement expectedRevision `{effective_rev}` does not match `{plan_rev}`"
                ),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }

        // 3. 会话准入（事务内镜像 check_session_admission + submit_run 队列检查）。
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
                record_commit_rejection(&tx, key, &digest, CODE_NOT_FOUND, &now, &retained_until)?;
                tx.commit().map_err(sql_err)?;
            }
            return Err(domain_not_found("session", &session_id));
        };
        if session_state == "deleted" {
            if let Some(key) = idempotency_key {
                record_commit_rejection(&tx, key, &digest, CODE_NOT_FOUND, &now, &retained_until)?;
                tx.commit().map_err(sql_err)?;
            }
            return Err(domain_not_found("session", &session_id));
        }
        if session_state == "closed" {
            return Err(ContractError::new(
                CODE_SESSION_CLOSED,
                format!("session `{session_id}` is closed; reopen it before submitting"),
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

        // 4. 边界复验：prepare 冻结的被替换轮必须仍是会话末轮。
        let last_turn: Option<String> = tx
            .query_row(
                "SELECT turn_id FROM history_entries WHERE session_id = ?1 ORDER BY seq DESC LIMIT 1",
                params![session_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if last_turn.as_deref() != Some(replaced_turn_id.as_str()) {
            let message = match &last_turn {
                None => format!(
                    "session `{session_id}` history no longer ends with turn `{replaced_turn_id}`; the boundary to replace is gone"
                ),
                Some(current) => format!(
                    "session `{session_id}` history now ends with turn `{current}`, not the prepared `{replaced_turn_id}`; re-prepare against the current last turn"
                ),
            };
            return Err(ContractError::new(
                CODE_TURN_CONFLICT,
                message,
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }

        // 5. 历史替换（截断被替换轮）。
        let removed = tx
            .execute(
                "DELETE FROM history_entries WHERE session_id = ?1 AND seq >= ?2",
                params![session_id, boundary_seq],
            )
            .map_err(sql_err)?;

        // 6. 新 run 受理（复刻 submit_run 的落库形状；`submit_run` 自持锁无法
        //    在本事务内调用，形状由本文件测试与 run.submit 回执对拍保证）。
        let run_id = self.next_id();
        let rev = bump_rev_in_tx(&tx)?;
        let queued_exists: i64 = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM run_queue WHERE session_id = ?1)",
                params![session_id],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        let start_immediately = active_run_id.is_none() && queued_exists == 0;
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
                // 新 run 与被替换轮可追溯：continuedFrom 指向被替换轮，
                // committed_run_id 从 plan 侧反向可查。
                replaced_turn_id,
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
        if start_immediately {
            tx.execute(
                "UPDATE sessions SET active_run_id = ?1 WHERE session_id = ?2",
                params![run_id, session_id],
            )
            .map_err(sql_err)?;
        }
        // 追溯事件（run 事件流内可见：run.events）。
        append_run_event_in_tx(
            &tx,
            &run_id,
            &session_id,
            "replacement.committed",
            &json!({
                "runId": run_id,
                "replacementId": replacement_id,
                "sessionId": session_id,
                "replacedTurnId": replaced_turn_id,
                "removedEntryCount": removed,
            }),
            &now,
        )?;

        // 7. plan 置 committed（保留恢复原件 backup_json 不动）。
        let plan_rev_after = bump_rev_in_tx(&tx)?;
        tx.execute(
            "UPDATE replacement_plans
             SET state = 'committed', revision = ?1, committed_run_id = ?2, updated_at = ?3
             WHERE replacement_id = ?4",
            params![plan_rev_after, run_id, now, replacement_id],
        )
        .map_err(sql_err)?;

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
                 VALUES (?1, 'replacement.commit', ?2, 'accepted', 'receiptFound', ?3, ?4, NULL, ?5, ?6, 'doNotReplay')",
                params![
                    key,
                    digest,
                    serde_json::to_string(&resource_val).unwrap(),
                    serde_json::to_string(&receipt).unwrap(),
                    now,
                    retained_until,
                ],
            )
            .map_err(sql_err)?;
        }

        tx.commit().map_err(sql_err)?;
        self.event_notifier().notify_waiters();
        self.state_notifier().notify_waiters();

        Ok(ReplacementCommitOutcome {
            receipt,
            run_id,
            start_worker: start_immediately,
        })
    }

    /// `replacement.prepare` 的存储原语（history 任务复用）：冻结候选边界并
    /// 保存恢复原件。**不截断可见历史**（prepare 合同 acceptance），不动
    /// runs/run_queue——因此回滚 prepared 事务只是作废 plan 行。
    ///
    /// 边界与冲突判定在本原语内完成：
    /// - 会话必须存在且未删除（NOT_FOUND）；
    /// - 末轮必须存在且 `expected_turn_id` 与之相等（TURN_CONFLICT——
    ///   「陈旧turn拒绝」「过期expectedTurnId拒绝替换」）；
    /// - 会话不得有 active/queued run（ACTIVE_RESOURCES——防止冻结边界被在途
    ///   run 追加历史作废）；
    /// - replacement_id 不得重复（ALREADY_EXISTS）。
    ///   会话 revision CAS（prepare 的 expectedRevision）等 handler 级前置由
    ///   history 任务在其 handler 里先做，再调用本原语。
    pub fn freeze_replacement_plan(
        &self,
        session_id: &str,
        replacement_id: &str,
        expected_turn_id: &str,
        ttl_ms: Option<u64>,
    ) -> Result<Value, ContractError> {
        let now = now_rfc3339();
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(sql_err)?;

        let state: Option<String> = tx
            .query_row(
                "SELECT session_state FROM sessions WHERE session_id = ?1",
                params![session_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        match state.as_deref() {
            None | Some("deleted") => return Err(domain_not_found("session", session_id)),
            Some(_) => {}
        }

        let active: i64 = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM runs WHERE session_id = ?1 AND terminal = 0)
                 OR EXISTS(SELECT 1 FROM run_queue WHERE session_id = ?1)",
                params![session_id],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        if active != 0 {
            return Err(ContractError::new(
                CODE_ACTIVE_RESOURCES,
                format!(
                    "session `{session_id}` still has an active or queued run; \
                     cancel or wait for it before preparing a replacement"
                ),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }

        let last: Option<(String, i64)> = tx
            .query_row(
                "SELECT turn_id, seq FROM history_entries WHERE session_id = ?1 ORDER BY seq DESC LIMIT 1",
                params![session_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        let Some((last_turn, _last_seq)) = last else {
            return Err(ContractError::new(
                CODE_TURN_CONFLICT,
                format!(
                    "session `{session_id}` has no history entries; there is no last turn to replace"
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        };
        if last_turn != expected_turn_id {
            return Err(ContractError::new(
                CODE_TURN_CONFLICT,
                format!(
                    "session `{session_id}` history ends with turn `{last_turn}`, not `{expected_turn_id}`; \
                     the selected message is no longer the last turn"
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }

        let dup: Option<String> = tx
            .query_row(
                "SELECT replacement_id FROM replacement_plans WHERE replacement_id = ?1",
                params![replacement_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if dup.is_some() {
            return Err(ContractError::new(
                CODE_ALREADY_EXISTS,
                format!("replacement `{replacement_id}` is already registered"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }

        let boundary_seq: i64 = tx
            .query_row(
                "SELECT MIN(seq) FROM history_entries WHERE session_id = ?1 AND turn_id = ?2",
                params![session_id, last_turn],
                |r| r.get::<_, Option<i64>>(0),
            )
            .map_err(sql_err)?
            .unwrap_or(0);

        // 恢复原件：被替换轮的完整 entry 列表（合同 History 项形状）。
        let mut stmt = tx
            .prepare(
                "SELECT entry_id, turn_id, role, content_json, run_id, tool_call_id, created_at
                 FROM history_entries WHERE session_id = ?1 AND seq >= ?2 ORDER BY seq ASC",
            )
            .map_err(sql_err)?;
        #[allow(clippy::type_complexity)]
        let rows: Vec<(
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            String,
        )> = stmt
            .query_map(params![session_id, boundary_seq], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })
            .map_err(sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err)?;
        drop(stmt);
        let removed = rows.len();
        let backup: Vec<Value> = rows
            .into_iter()
            .map(
                |(entry_id, turn_id, role, content_json, run_id, tool_call_id, created_at)| {
                    json!({
                        "entryId": entry_id,
                        "turnId": turn_id,
                        "role": role,
                        "content": serde_json::from_str::<Value>(&content_json)
                            .unwrap_or(json!([])),
                        "runId": run_id,
                        "toolCallId": tool_call_id,
                        "createdAt": created_at,
                    })
                },
            )
            .collect();
        let backup_json = serde_json::to_string(&backup).unwrap_or_else(|_| "[]".into());

        let expires_at = ttl_ms.map(|ms| {
            (chrono::Utc::now() + chrono::Duration::milliseconds(ms as i64)).to_rfc3339()
        });
        let rev = bump_rev_in_tx(&tx)?;
        tx.execute(
            "INSERT INTO replacement_plans
             (replacement_id, session_id, instance_id, replaced_turn_id, boundary_seq, backup_json, state, revision, expires_at, committed_run_id, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'prepared', ?7, ?8, NULL, ?9, ?10)",
            params![
                replacement_id,
                session_id,
                self.meta().instance_id,
                last_turn,
                boundary_seq,
                backup_json,
                rev,
                expires_at,
                now,
                now
            ],
        )
        .map_err(sql_err)?;

        append_domain_event_in_tx(
            &tx,
            &self.meta().instance_id,
            session_id,
            session_id,
            "replacement.prepared",
            &json!({
                "replacementId": replacement_id,
                "sessionId": session_id,
                "replacedTurnId": last_turn,
                "boundarySeq": boundary_seq,
                "removedEntryCount": removed,
                "revision": rev,
            }),
            &now,
        )?;
        tx.commit().map_err(sql_err)?;

        Ok(json!({
            "replacementId": replacement_id,
            "sessionId": session_id,
            "replacedTurnId": last_turn,
            "boundarySeq": boundary_seq,
            "removedEntryCount": removed,
            "state": "prepared",
            "revision": rev,
            "expiresAt": expires_at,
            "createdAt": now,
        }))
    }
}

/// 幂等拒绝登记（run.submit 的 `record_rejection_in_tx` 同形）。
fn record_commit_rejection(
    conn: &Connection,
    key: &str,
    digest: &str,
    rejection_code: &str,
    observed_at: &str,
    retained_until: &str,
) -> Result<(), ContractError> {
    conn.execute(
        "INSERT OR IGNORE INTO idempotency_records (idempotency_key, method_id, input_digest, request_state, reason, resource_json, receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
         VALUES (?1, 'replacement.commit', ?2, 'notAccepted', 'definitivelyRejected', NULL, NULL, ?3, ?4, ?5, 'correctRejectedInputWithNewKey')",
        params![key, digest, rejection_code, observed_at, retained_until],
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

// ---------------------------------------------------------------------------
// Tests（store 直连模式，`configuration_tests.rs` / `history_archive.rs` 先例）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::runtime::PublicV1Runtime;
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

    fn contract_code(error: &crate::public_v1::runtime::DispatchError) -> String {
        match error {
            crate::public_v1::runtime::DispatchError::Application(e) => e.code.clone(),
            crate::public_v1::runtime::DispatchError::Rpc { data, .. } => data
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

    /// 两轮对话：t-1（user+assistant）、t-2（user+assistant）。
    fn conversation() -> Vec<Value> {
        vec![
            json!({"turnId": "t-1", "role": "user", "content": [{"kind": "text", "text": "first question"}]}),
            json!({"turnId": "t-1", "role": "assistant", "content": [{"kind": "text", "text": "first answer"}]}),
            json!({"turnId": "t-2", "role": "user", "content": [{"kind": "text", "text": "second question"}]}),
            json!({"turnId": "t-2", "role": "assistant", "content": [{"kind": "text", "text": "second answer"}]}),
        ]
    }

    async fn history_ids(
        rt: &std::sync::Arc<PublicV1Runtime>,
        iid: &str,
        sid: &str,
    ) -> Vec<String> {
        let (_, page) = rt
            .dispatch(
                "history.read",
                json!({"instanceId": iid, "sessionId": sid, "page": {"limit": 500}}),
                None,
                None,
            )
            .await
            .unwrap();
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["turnId"].as_str().unwrap().to_owned())
            .collect()
    }

    fn commit_input(iid: &str, replacement_id: &str, rev: &str, text: &str) -> Value {
        json!({
            "instanceId": iid,
            "replacementId": replacement_id,
            "input": [{"kind": "text", "text": text}],
            "expectedRevision": rev,
        })
    }

    #[tokio::test]
    async fn commit_replaces_last_turn_and_accepts_run_in_one_transaction() {
        let tmp = TempDir::new("rc-commit");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "rc1").await;
        rt.store()
            .append_history_entries(&sid, &conversation())
            .unwrap();

        let plan = rt
            .store()
            .freeze_replacement_plan(&sid, "rep-1", "t-2", None)
            .unwrap();
        assert_eq!(plan["state"], "prepared");
        assert_eq!(plan["replacedTurnId"], "t-2");
        assert_eq!(plan["removedEntryCount"], 2, "t-2 has two entries");
        // prepare 不截断可见历史。
        assert_eq!(history_ids(&rt, &iid, &sid).await.len(), 4);

        let rev = plan["revision"].as_str().unwrap().to_owned();
        let (outcome, receipt) = rt
            .dispatch(
                "replacement.commit",
                commit_input(&iid, "rep-1", &rev, "edited question"),
                Some("key-rc-1"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome, ResponseOutcome::Accepted);
        OperationRegistry::global()
            .validate_result("replacement.commit", &receipt)
            .expect("receipt matches the contract result schema");
        assert_eq!(receipt["kind"], "run");
        assert_eq!(receipt["durable"], true);
        assert_eq!(receipt["deduplicated"], false);
        assert_eq!(receipt["state"], "queued");
        let run_id = receipt["runId"].as_str().unwrap().to_owned();
        assert!(!run_id.is_empty());

        // 历史只剩 t-1（两轮截断为一轮）。
        assert_eq!(history_ids(&rt, &iid, &sid).await, vec!["t-1", "t-1"]);

        // 新 run 被驱动到终态，输入与可追溯性落地。
        let (_, waited) = rt
            .dispatch(
                "run.wait",
                json!({"instanceId": iid, "runId": run_id, "timeoutMs": 5000}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(waited["run"]["state"], "succeeded");
        let stored = rt.store().get_run(&run_id).unwrap();
        assert_eq!(stored.continued_from.as_deref(), Some("t-2"));
        assert_eq!(
            stored.input["input"][0]["text"].as_str().unwrap(),
            "edited question"
        );

        // plan 置 committed，run 可反查。
        let (state, committed_run, backup): (String, Option<String>, String) = {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            let (state, committed_run): (String, Option<String>) = conn
                .query_row(
                    "SELECT state, committed_run_id FROM replacement_plans WHERE replacement_id = 'rep-1'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            let backup: String = conn
                .query_row(
                    "SELECT backup_json FROM replacement_plans WHERE replacement_id = 'rep-1'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            (state, committed_run, backup)
        };
        assert_eq!(state, "committed");
        assert_eq!(committed_run.as_deref(), Some(run_id.as_str()));
        // 恢复原件仍在（postcondition：保留旧历史恢复原件）。
        let backup: Value = serde_json::from_str(&backup).unwrap();
        assert_eq!(backup.as_array().map(Vec::len), Some(2));
        assert_eq!(backup[0]["turnId"], "t-2");

        // 追溯事件进了 run 事件流。
        let (_, events) = rt
            .dispatch(
                "run.events",
                json!({"instanceId": iid, "runId": run_id, "page": {"limit": 100}}),
                None,
                None,
            )
            .await
            .unwrap();
        let types: Vec<&str> = events["frame"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["type"].as_str().unwrap())
            .collect();
        assert!(types.contains(&"run.queued"));
        assert!(types.contains(&"replacement.committed"));
    }

    #[tokio::test]
    async fn commit_deduplicates_replays_and_rejects_key_conflict() {
        let tmp = TempDir::new("rc-dedup");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "rc2").await;
        rt.store()
            .append_history_entries(&sid, &conversation())
            .unwrap();
        let plan = rt
            .store()
            .freeze_replacement_plan(&sid, "rep-2", "t-2", None)
            .unwrap();
        let rev = plan["revision"].as_str().unwrap().to_owned();

        let (_, first) = rt
            .dispatch(
                "replacement.commit",
                commit_input(&iid, "rep-2", &rev, "edited"),
                Some("key-rc-2"),
                None,
            )
            .await
            .unwrap();
        let run_id = first["runId"].as_str().unwrap().to_owned();

        // 同 key 同输入：去重回放原回执，不重复截断（历史长度不变）。
        let (_, replay) = rt
            .dispatch(
                "replacement.commit",
                commit_input(&iid, "rep-2", &rev, "edited"),
                Some("key-rc-2"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(replay["deduplicated"], true);
        assert_eq!(replay["runId"].as_str().unwrap(), run_id);
        assert_eq!(history_ids(&rt, &iid, &sid).await, vec!["t-1", "t-1"]);

        // requests.lookup 按同 methodId/key 返回原资源。
        let (_, lookup) = rt
            .dispatch(
                "requests.lookup",
                json!({"instanceId": iid, "methodId": "replacement.commit", "idempotencyKey": "key-rc-2"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(lookup["requestState"], "accepted");
        assert_eq!(lookup["resource"]["runId"].as_str().unwrap(), run_id);

        // 同 key 不同语义输入：IDEMPOTENCY_CONFLICT，历史仍不变。
        let err = rt
            .dispatch(
                "replacement.commit",
                commit_input(&iid, "rep-2", &rev, "different content"),
                Some("key-rc-2"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "IDEMPOTENCY_CONFLICT");
        assert_eq!(history_ids(&rt, &iid, &sid).await, vec!["t-1", "t-1"]);
    }

    #[tokio::test]
    async fn commit_refuses_stale_revision_state_expiry_and_turn_conflict() {
        let tmp = TempDir::new("rc-refuse");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "rc3").await;
        rt.store()
            .append_history_entries(&sid, &conversation())
            .unwrap();
        let plan = rt
            .store()
            .freeze_replacement_plan(&sid, "rep-3", "t-2", None)
            .unwrap();
        let rev = plan["revision"].as_str().unwrap().to_owned();

        // 过期 expectedRevision：REVISION_CONFLICT，历史完整。
        let err = rt
            .dispatch(
                "replacement.commit",
                commit_input(&iid, "rep-3", "rev-bogus", "x"),
                Some("key-rc-3a"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "REVISION_CONFLICT");
        assert_eq!(history_ids(&rt, &iid, &sid).await.len(), 4);

        // 边界被历史移动抢先：TURN_CONFLICT，历史完整。
        rt.store()
            .append_history_entries(
                &sid,
                &[json!({"turnId": "t-3", "role": "user", "content": [{"kind": "text", "text": "moved on"}]})],
            )
            .unwrap();
        let err = rt
            .dispatch(
                "replacement.commit",
                commit_input(&iid, "rep-3", &rev, "x"),
                Some("key-rc-3b"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "TURN_CONFLICT");
        assert_eq!(history_ids(&rt, &iid, &sid).await.len(), 5);
        // 撤掉 t-3 恢复边界，供后续步骤使用。
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "DELETE FROM history_entries WHERE session_id = ?1 AND turn_id = 't-3'",
                params![sid],
            )
            .unwrap();
        }

        // 已提交的 plan 再提交（新 key）：REPLACEMENT_EXPIRED。
        let _ = rt
            .dispatch(
                "replacement.commit",
                commit_input(&iid, "rep-3", &rev, "x"),
                Some("key-rc-3c"),
                None,
            )
            .await
            .unwrap();
        let err = rt
            .dispatch(
                "replacement.commit",
                commit_input(&iid, "rep-3", &rev, "x"),
                Some("key-rc-3d"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "REPLACEMENT_EXPIRED");

        // TTL 过期：REPLACEMENT_EXPIRED（rep-3 提交后末轮已是 t-1）。
        let plan5 = rt
            .store()
            .freeze_replacement_plan(&sid, "rep-5", "t-1", Some(1))
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let rev5 = plan5["revision"].as_str().unwrap().to_owned();
        let err = rt
            .dispatch(
                "replacement.commit",
                commit_input(&iid, "rep-5", &rev5, "x"),
                Some("key-rc-5"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "REPLACEMENT_EXPIRED");
        assert_eq!(history_ids(&rt, &iid, &sid).await, vec!["t-1", "t-1"]);

        // 未知 plan：NOT_FOUND（幂等拒绝登记后同 key 重放同一拒绝）。
        let err = rt
            .dispatch(
                "replacement.commit",
                commit_input(&iid, "rep-unknown", &rev, "x"),
                Some("key-rc-6"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "NOT_FOUND");
        let err = rt
            .dispatch(
                "replacement.commit",
                commit_input(&iid, "rep-unknown", &rev, "x"),
                Some("key-rc-6"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "NOT_FOUND");
    }

    #[tokio::test]
    async fn commit_gates_session_admission_and_freeze_validates_boundary() {
        let tmp = TempDir::new("rc-gate");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "rc4").await;
        rt.store()
            .append_history_entries(&sid, &conversation())
            .unwrap();

        // freeze：陈旧 expectedTurnId / 重复 id。
        let err = rt
            .store()
            .freeze_replacement_plan(&sid, "rep-x", "t-1", None)
            .unwrap_err();
        assert_eq!(err.code, "TURN_CONFLICT");
        let plan = rt
            .store()
            .freeze_replacement_plan(&sid, "rep-4", "t-2", None)
            .unwrap();
        let err = rt
            .store()
            .freeze_replacement_plan(&sid, "rep-4", "t-2", None)
            .unwrap_err();
        assert_eq!(err.code, "ALREADY_EXISTS");
        let rev = plan["revision"].as_str().unwrap().to_owned();

        // closed 会话：SESSION_CLOSED，历史完整。
        let close_rev = rev_of_session(&rt, &iid, &sid).await;
        rt.dispatch(
            "session.close",
            json!({"instanceId": iid, "sessionId": sid, "expectedRevision": close_rev}),
            None,
            None,
        )
        .await
        .unwrap();
        let err = rt
            .dispatch(
                "replacement.commit",
                commit_input(&iid, "rep-4", &rev, "x"),
                Some("key-rc-4a"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "SESSION_CLOSED");
        assert_eq!(history_ids(&rt, &iid, &sid).await.len(), 4);
        let reopen_rev = rev_of_session(&rt, &iid, &sid).await;
        rt.dispatch(
            "session.reopen",
            json!({"instanceId": iid, "sessionId": sid, "expectedRevision": reopen_rev}),
            None,
            None,
        )
        .await
        .unwrap();

        // queue 暂停（失败 run 的既有副作用）：QUEUE_PAUSED，历史完整。
        let (_, failed) = rt
            .dispatch(
                "run.submit",
                json!({
                    "instanceId": iid,
                    "sessionId": sid,
                    "input": [{"kind": "text", "text": "[pd-driver:fail]"}]
                }),
                Some("key-rc-4b"),
                None,
            )
            .await
            .unwrap();
        let failed_run = failed["runId"].as_str().unwrap().to_owned();
        let _ = rt
            .dispatch(
                "run.wait",
                json!({"instanceId": iid, "runId": failed_run, "timeoutMs": 5000}),
                None,
                None,
            )
            .await
            .unwrap();
        let err = rt
            .dispatch(
                "replacement.commit",
                commit_input(&iid, "rep-4", &rev, "x"),
                Some("key-rc-4c"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "QUEUE_PAUSED");
        assert_eq!(history_ids(&rt, &iid, &sid).await.len(), 4);

        // freeze 拒绝有在途 run 的会话：ACTIVE_RESOURCES。
        let resume_rev = queue_rev(&rt, &iid, &sid).await;
        rt.dispatch(
            "queue.resume",
            json!({
                "instanceId": iid,
                "sessionId": sid,
                "expectedRevision": resume_rev,
                "acknowledgedPauseReason": "run_failed",
            }),
            None,
            None,
        )
        .await
        .unwrap();
        let (_, running) = rt
            .dispatch(
                "run.submit",
                json!({
                    "instanceId": iid,
                    "sessionId": sid,
                    "input": [{"kind": "text", "text": "[pd-driver:sleep=8000]"}]
                }),
                Some("key-rc-4d"),
                None,
            )
            .await
            .unwrap();
        let busy_run = running["runId"].as_str().unwrap().to_owned();
        let err = rt
            .store()
            .freeze_replacement_plan(&sid, "rep-4b", "t-2", None)
            .unwrap_err();
        assert_eq!(err.code, "ACTIVE_RESOURCES");
        // 清理：取消在途 run，恢复 idle。
        rt.dispatch(
            "run.cancel",
            json!({"instanceId": iid, "runId": busy_run}),
            Some("key-rc-4e"),
            None,
        )
        .await
        .unwrap();
    }

    async fn rev_of_session(rt: &std::sync::Arc<PublicV1Runtime>, iid: &str, sid: &str) -> String {
        let (_, sess) = rt
            .dispatch(
                "session.get",
                json!({"instanceId": iid, "sessionId": sid}),
                None,
                None,
            )
            .await
            .unwrap();
        sess["session"]["revision"].as_str().unwrap().to_owned()
    }

    async fn queue_rev(rt: &std::sync::Arc<PublicV1Runtime>, iid: &str, sid: &str) -> String {
        let (_, q) = rt
            .dispatch(
                "queue.get",
                json!({"instanceId": iid, "sessionId": sid}),
                None,
                None,
            )
            .await
            .unwrap();
        q["queue"]["revision"].as_str().unwrap().to_owned()
    }
}
