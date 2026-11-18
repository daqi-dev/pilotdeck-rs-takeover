//! `replacement.prepare` / `replacement.get` / `replacement.rollback` — 末轮
//! 替换事务的管理侧（`cli.task.history`）。
//!
//! 合同正本 `state/cli-v1-operations.json#replacement.{prepare,get,rollback}`。
//! `replacement.commit` 已由 t007 交付（`domains/replacement.rs`）；本文件按
//! 其报告 §2 的接线约定实现其余三跳，复用
//! [`V1Store::freeze_replacement_plan`] 原语与 `replacement_plans` 表：
//!
//! - **prepare**：handler 侧先做会话 revision CAS（`expectedRevision` vs
//!   `sessions.revision`）与并发替换占线检查（同会话已有未过期的
//!   prepared plan → `REPLACEMENT_BUSY`，「并发替换冲突明确」），再调用
//!   freeze 原语冻结边界并保存恢复原件（**不截断可见历史**——prepare 合
//!   同 acceptance）。freeze 后复查占线（关闭 check-then-act 竞窗：若
//!   并发 prepare 同时冻结了两条 plan，败者删除自己刚冻结的行并返回
//!   `REPLACEMENT_BUSY`，replacementId 保持可重试）。
//! - **get**：直读 plan 行投影合同 `Replacement`。`baseRevision` 是
//!   prepare 签发、commit/rollback 需回传的 CAS 令牌（即 plan 行当前
//!   revision；prepared 阶段它与 prepare 返回值一致——本接线与 t007
//!   commit 的 plan_rev CAS 严格对齐）。`phase` 按存储态 + TTL 过期派
//!   生（prepared → 过期后 lazy 报 `expired`，不改历史——「到期不改
//!   历史」）；committed 态的 `state` 映射新 run 的状态（非终态归
//!   `waiting`，run 行缺失归 `result_unknown`）。「丢 commit 响应定位
//!   唯一新 run」由 `replacementRunId`（= committed_run_id）承载。
//! - **rollback**：committed → `ALREADY_COMMITTED`（「committed拒绝不
//!   回滚执行」）；prepared（含已过期）在 revision CAS 通过后置
//!   `rolledBack` 并追加事件——prepare 从未截断历史，撤销无需恢复动作
//!   （「prepared撤销保留原历史」）；已 rolledBack 的重复回滚按幂等成
//!   功返回当前投影。RESTORE_FAILED 与 phase `inconsistent` 在本接线下
//!   不可达（prepare=仅冻结、commit=单事务截断+受理，不存在需要恢复
//!   的中间态；合同声明保留给未来的恢复路径——报告登记）。

use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_REVISION_CONFLICT, ContractError, EffectStatus, RetrySpec,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::workspace_dirs::{
    append_domain_event_in_tx, bump_rev_in_tx, domain_not_found, sql_err,
};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, now_rfc3339};

/// 合同声明而协议 crate 尚无常量的错误码（RESTORE_FAILED 亦在合同错误
/// 集中声明，但本接线下不可达——见模块文档，不定义常量）。
pub(crate) const CODE_REPLACEMENT_BUSY: &str = "REPLACEMENT_BUSY";
pub(crate) const CODE_ALREADY_COMMITTED: &str = "ALREADY_COMMITTED";

/// prepare 的默认 TTL（合同 input 默认值）。
const DEFAULT_TTL_MS: u64 = 30_000;

pub(super) fn register(reg: &mut DomainRegistry) {
    // replacement_plans 表由 t007 的 replacement.rs 注册，此处不重复。
    reg.handler("replacement.prepare", |rt, input, _key, rev| {
        Box::pin(replacement_prepare(rt, input, rev))
    });
    reg.handler("replacement.get", |rt, input, _key, _rev| {
        Box::pin(replacement_get(rt, input))
    });
    reg.handler("replacement.rollback", |rt, input, _key, rev| {
        Box::pin(replacement_rollback(rt, input, rev))
    });
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn revision_conflict(expected: &str, actual: &str) -> ContractError {
    ContractError::new(
        CODE_REVISION_CONFLICT,
        format!("replacement expectedRevision `{expected}` does not match `{actual}`"),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

/// 一条 plan 行的完整读出。
struct PlanRow {
    session_id: String,
    replaced_turn_id: String,
    state: String,
    revision: String,
    expires_at: Option<String>,
    committed_run_id: Option<String>,
    created_at: String,
}

fn read_plan(conn: &Connection, replacement_id: &str) -> Result<Option<PlanRow>, ContractError> {
    Ok(conn
        .query_row(
            "SELECT session_id, replaced_turn_id, state, revision, expires_at, committed_run_id,
                    created_at
             FROM replacement_plans WHERE replacement_id = ?1",
            params![replacement_id],
            |r| {
                Ok(PlanRow {
                    session_id: r.get(0)?,
                    replaced_turn_id: r.get(1)?,
                    state: r.get(2)?,
                    revision: r.get(3)?,
                    expires_at: r.get(4)?,
                    committed_run_id: r.get(5)?,
                    created_at: r.get(6)?,
                })
            },
        )
        .optional()
        .map_err(sql_err)?)
}

/// TTL 是否已过（chrono 真解析；解析失败按未过期——宁拒不误伤）。
fn is_expired(expires_at: Option<&str>) -> bool {
    expires_at.is_some_and(|raw| {
        chrono::DateTime::parse_from_rfc3339(raw).is_ok_and(|expires| {
            chrono::Utc::now()
                .signed_duration_since(expires)
                .num_milliseconds()
                >= 0
        })
    })
}

/// committed plan 的 `state`：新 run 非终态归 `waiting`（Replacement.state
/// 枚举无 queued/running），终态同名；run 行缺失归 `result_unknown`。
fn committed_state(
    conn: &Connection,
    committed_run_id: Option<&str>,
) -> Result<String, ContractError> {
    let Some(run_id) = committed_run_id else {
        return Ok("result_unknown".to_owned());
    };
    let run_state: Option<String> = conn
        .query_row(
            "SELECT state FROM runs WHERE run_id = ?1",
            params![run_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    Ok(match run_state {
        Some(state)
            if matches!(
                state.as_str(),
                "succeeded" | "failed" | "cancelled" | "interrupted" | "result_unknown"
            ) =>
        {
            state
        }
        Some(_) => "waiting".to_owned(),
        None => "result_unknown".to_owned(),
    })
}

/// plan 行 → 合同 `Replacement` 投影。
fn replacement_json(
    conn: &Connection,
    replacement_id: &str,
    plan: &PlanRow,
) -> Result<Value, ContractError> {
    let (phase, state) = match plan.state.as_str() {
        "committed" => (
            "committed",
            committed_state(conn, plan.committed_run_id.as_deref())?,
        ),
        "rolledBack" => ("rolledBack", "cancelled".to_owned()),
        other => {
            // prepared（含 t007 直接种下的行）与未来的 inconsistent。
            if other == "inconsistent" {
                ("inconsistent", "failed".to_owned())
            } else if is_expired(plan.expires_at.as_deref()) {
                ("expired", "cancelled".to_owned())
            } else {
                ("prepared", "waiting".to_owned())
            }
        }
    };
    // expires_at 为 NULL 只会出现在绕过本 handler 的直接 freeze 种子行
    // （本 handler 恒传 Some(ttl)）；投影回落到 created_at 保证合同必填
    // 的 date-time 形状（报告登记）。
    let expires_at = plan
        .expires_at
        .clone()
        .unwrap_or_else(|| plan.created_at.clone());
    Ok(json!({
        "replacementId": replacement_id,
        "sessionId": plan.session_id,
        "expectedTurnId": plan.replaced_turn_id,
        "baseRevision": plan.revision,
        "expiresAt": expires_at,
        "state": state,
        "replacementRunId": plan.committed_run_id,
        "phase": phase,
    }))
}

// ---------------------------------------------------------------------------
// replacement.prepare
// ---------------------------------------------------------------------------

async fn replacement_prepare(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    expected_revision: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    Ok((
        ResponseOutcome::Succeeded,
        rt.store().prepare_replacement(&input, expected_revision)?,
    ))
}

impl V1Store {
    /// `replacement.prepare`：会话 CAS + 占线检查 → freeze 原语 → 占线
    /// 复查（竞窗关闭）。
    fn prepare_replacement(
        &self,
        input: &Value,
        expected_revision: Option<String>,
    ) -> Result<Value, ContractError> {
        let replacement_id = input["replacementId"].as_str().unwrap_or_default();
        let session_id = input["sessionId"].as_str().unwrap_or_default();
        let expected_turn_id = input["expectedTurnId"].as_str().unwrap_or_default();
        let ttl_ms = input
            .get("ttlMs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TTL_MS);
        // envelope 级 expectedRevision 优先，缺省回落到 params 内
        // （replacement.commit 同序）。
        let expected = expected_revision
            .or_else(|| {
                input
                    .get("expectedRevision")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_default();

        // 前置：会话存在且未删除 + revision CAS（freeze 内部复验边界与
        // 在途 run，但不复验会话 revision——本层是 CAS 的唯一落点）。
        {
            let conn = self.conn.lock().expect("sqlite mutex");
            let session: Option<(String, String)> = conn
                .query_row(
                    "SELECT session_state, revision FROM sessions WHERE session_id = ?1",
                    params![session_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(sql_err)?;
            match session {
                None => return Err(domain_not_found("session", session_id)),
                Some((state, revision)) => {
                    if state == "deleted" {
                        return Err(domain_not_found("session", session_id));
                    }
                    if revision != expected {
                        return Err(revision_conflict(&expected, &revision));
                    }
                }
            }
            if self.session_has_active_prepared_plan(&conn, session_id)? {
                return Err(ContractError::new(
                    CODE_REPLACEMENT_BUSY,
                    format!(
                        "session `{session_id}` already has a prepared replacement; commit or roll it back before preparing another"
                    ),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
        }

        // 冻结（不截断可见历史）。原语自身的错误集直接透出：
        // NOT_FOUND / ACTIVE_RESOURCES / TURN_CONFLICT / ALREADY_EXISTS。
        let frozen = self.freeze_replacement_plan(
            session_id,
            replacement_id,
            expected_turn_id,
            Some(ttl_ms),
        )?;

        // 占线复查：若并发 prepare 也在本会话冻结了 plan，删除自己刚冻
        // 结的行（replacementId 保持可重试）并拒绝。
        {
            let conn = self.conn.lock().expect("sqlite mutex");
            if count_prepared_plans(&conn, session_id)? > 1 {
                drop(conn);
                self.invalidate_prepared_plan(replacement_id, "REPLACEMENT_BUSY")?;
                return Err(ContractError::new(
                    CODE_REPLACEMENT_BUSY,
                    format!(
                        "a concurrent replacement prepared for session `{session_id}` won the race; retry with the same replacementId"
                    ),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
        }

        Ok(json!({
            "replacement": {
                "replacementId": replacement_id,
                "sessionId": session_id,
                "expectedTurnId": expected_turn_id,
                "baseRevision": frozen["revision"],
                "expiresAt": frozen["expiresAt"],
                "state": "waiting",
                "replacementRunId": Value::Null,
                "phase": "prepared",
            }
        }))
    }

    /// 会话是否存在未过期的 prepared plan（占线判定）。
    fn session_has_active_prepared_plan(
        &self,
        conn: &Connection,
        session_id: &str,
    ) -> Result<bool, ContractError> {
        Ok(count_prepared_plans(conn, session_id)? > 0)
    }

    /// 作废一条刚冻结的 plan（占线复查败者的清理路径）：删除行 + 审计
    /// 事件，replacementId 随后可重试。
    fn invalidate_prepared_plan(
        &self,
        replacement_id: &str,
        reason: &str,
    ) -> Result<(), ContractError> {
        let now = now_rfc3339();
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(sql_err)?;
        let owned: Option<(String, String)> = tx
            .query_row(
                "SELECT session_id, state FROM replacement_plans WHERE replacement_id = ?1",
                params![replacement_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        if let Some((session_id, state)) = owned
            && state == "prepared"
        {
            tx.execute(
                "DELETE FROM replacement_plans WHERE replacement_id = ?1",
                params![replacement_id],
            )
            .map_err(sql_err)?;
            append_domain_event_in_tx(
                &tx,
                &self.meta().instance_id,
                replacement_id,
                &session_id,
                "replacement.invalidated",
                &json!({
                    "replacementId": replacement_id,
                    "sessionId": session_id,
                    "reason": reason,
                }),
                &now,
            )?;
            tx.commit().map_err(sql_err)?;
            self.event_notifier().notify_waiters();
        }
        Ok(())
    }
}

fn count_prepared_plans(conn: &Connection, session_id: &str) -> Result<i64, ContractError> {
    let now = now_rfc3339();
    conn.query_row(
        "SELECT COUNT(*) FROM replacement_plans
         WHERE session_id = ?1 AND state = 'prepared'
           AND (expires_at IS NULL OR expires_at > ?2)",
        params![session_id, now],
        |r| r.get(0),
    )
    .map_err(sql_err)
}

// ---------------------------------------------------------------------------
// replacement.get
// ---------------------------------------------------------------------------

async fn replacement_get(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let replacement_id = input["replacementId"].as_str().unwrap_or_default();
    let conn = rt.store().conn.lock().expect("sqlite mutex");
    let Some(plan) = read_plan(&conn, replacement_id)? else {
        return Err(domain_not_found("replacement", replacement_id).into());
    };
    let replacement = replacement_json(&conn, replacement_id, &plan)?;
    Ok((
        ResponseOutcome::Succeeded,
        json!({"replacement": replacement}),
    ))
}

// ---------------------------------------------------------------------------
// replacement.rollback
// ---------------------------------------------------------------------------

async fn replacement_rollback(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    expected_revision: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    Ok((
        ResponseOutcome::Succeeded,
        rt.store().rollback_replacement(&input, expected_revision)?,
    ))
}

impl V1Store {
    /// `replacement.rollback`：committed 拒绝；prepared 在 CAS 通过后置
    /// rolledBack；已 rolledBack 幂等返回。
    fn rollback_replacement(
        &self,
        input: &Value,
        expected_revision: Option<String>,
    ) -> Result<Value, ContractError> {
        let replacement_id = input["replacementId"].as_str().unwrap_or_default();
        let expected = expected_revision
            .or_else(|| {
                input
                    .get("expectedRevision")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_default();
        let now = now_rfc3339();
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(sql_err)?;
        let Some(plan) = read_plan(&tx, replacement_id)? else {
            return Err(domain_not_found("replacement", replacement_id));
        };
        match plan.state.as_str() {
            "committed" => {
                // 「committed拒绝不回滚执行」。
                return Err(ContractError::new(
                    CODE_ALREADY_COMMITTED,
                    format!(
                        "replacement `{replacement_id}` is already committed (run `{}`); use run.cancel on the new run instead",
                        plan.committed_run_id.as_deref().unwrap_or("?")
                    ),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            "rolledBack" => {
                // 幂等：重复回滚返回当前投影（写结果未知先查询的路径）。
                let replacement = replacement_json(&tx, replacement_id, &plan)?;
                tx.commit().map_err(sql_err)?;
                return Ok(json!({"replacement": replacement}));
            }
            _ => {}
        }
        // prepared（含已过期）：CAS 通过后撤销。prepare 从未截断历史，
        // 撤销只是作废 plan 行（无需恢复动作）。
        if plan.revision != expected {
            return Err(revision_conflict(&expected, &plan.revision));
        }
        let rev = bump_rev_in_tx(&tx)?;
        tx.execute(
            "UPDATE replacement_plans
             SET state = 'rolledBack', revision = ?1, updated_at = ?2
             WHERE replacement_id = ?3 AND state = 'prepared'",
            params![rev, now, replacement_id],
        )
        .map_err(sql_err)?;
        append_domain_event_in_tx(
            &tx,
            &self.meta().instance_id,
            replacement_id,
            &plan.session_id,
            "replacement.rolled_back",
            &json!({
                "replacementId": replacement_id,
                "sessionId": plan.session_id,
                "replacedTurnId": plan.replaced_turn_id,
                "revision": rev,
            }),
            &now,
        )?;
        let rolled = PlanRow {
            session_id: plan.session_id,
            replaced_turn_id: plan.replaced_turn_id,
            state: "rolledBack".to_owned(),
            revision: rev,
            expires_at: plan.expires_at,
            committed_run_id: None,
            created_at: plan.created_at,
        };
        let replacement = replacement_json(&tx, replacement_id, &rolled)?;
        tx.commit().map_err(sql_err)?;
        self.event_notifier().notify_waiters();
        Ok(json!({"replacement": replacement}))
    }
}

// ---------------------------------------------------------------------------
// Tests（store 直连模式；本地未跑——2026-11-18 构建冻结令，待集成者 cb8）
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
        let session_id = format!("s-{tag}");
        rt.dispatch(
            "session.create",
            json!({
                "instanceId": iid,
                "sessionId": &session_id,
                "projectId": format!("p-{tag}"),
            }),
            None,
            None,
        )
        .await
        .unwrap();
        session_id
    }

    fn conversation() -> Vec<Value> {
        vec![
            json!({"turnId": "t-1", "role": "user", "content": [{"kind": "text", "text": "first question"}]}),
            json!({"turnId": "t-1", "role": "assistant", "content": [{"kind": "text", "text": "first answer"}]}),
            json!({"turnId": "t-2", "role": "user", "content": [{"kind": "text", "text": "second question"}]}),
            json!({"turnId": "t-2", "role": "assistant", "content": [{"kind": "text", "text": "second answer"}]}),
        ]
    }

    async fn history_len(rt: &std::sync::Arc<PublicV1Runtime>, iid: &str, sid: &str) -> usize {
        let (_, page) = rt
            .dispatch(
                "history.read",
                json!({"instanceId": iid, "sessionId": sid, "page": {"limit": 500}}),
                None,
                None,
            )
            .await
            .unwrap();
        page["items"].as_array().unwrap().len()
    }

    async fn session_rev(rt: &std::sync::Arc<PublicV1Runtime>, iid: &str, sid: &str) -> String {
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

    async fn expect_code(
        rt: &std::sync::Arc<PublicV1Runtime>,
        method: &str,
        input: Value,
    ) -> String {
        match rt.dispatch(method, input, None, None).await {
            Ok((_, value)) => panic!("expected a contract failure, got {value}"),
            Err(error) => contract_code(&error),
        }
    }

    #[tokio::test]
    async fn prepare_get_rollback_roundtrip_keeps_history_intact() {
        let tmp = TempDir::new("rpa-round");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "r1").await;
        rt.store()
            .append_history_entries(&sid, &conversation())
            .unwrap();
        let rev = session_rev(&rt, &iid, &sid).await;

        // prepare：结果过 schema，不截断可见历史。
        let (_, prepared) = rt
            .dispatch(
                "replacement.prepare",
                json!({
                    "instanceId": iid,
                    "replacementId": "rep-1",
                    "sessionId": sid,
                    "expectedRevision": rev,
                    "expectedTurnId": "t-2",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("replacement.prepare", &prepared)
            .expect("prepare result matches the contract result schema");
        let replacement = &prepared["replacement"];
        assert_eq!(replacement["phase"], "prepared");
        assert_eq!(replacement["state"], "waiting");
        assert_eq!(replacement["expectedTurnId"], "t-2");
        assert_eq!(replacement["replacementRunId"], Value::Null);
        assert!(replacement["expiresAt"].as_str().is_some());
        assert_eq!(
            history_len(&rt, &iid, &sid).await,
            4,
            "prepare must not truncate"
        );
        let base = replacement["baseRevision"].as_str().unwrap().to_owned();

        // get：与 prepare 一致（丢响应后的原 ID 查询路径）。
        let (_, got) = rt
            .dispatch(
                "replacement.get",
                json!({"instanceId": iid, "replacementId": "rep-1"}),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("replacement.get", &got)
            .expect("get result matches the contract result schema");
        assert_eq!(got["replacement"]["phase"], "prepared");
        assert_eq!(got["replacement"]["baseRevision"], base.as_str());

        // 回滚（CAS 用 prepare 的 baseRevision）：历史原样。
        let (_, rolled) = rt
            .dispatch(
                "replacement.rollback",
                json!({
                    "instanceId": iid,
                    "replacementId": "rep-1",
                    "expectedRevision": base,
                }),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("replacement.rollback", &rolled)
            .expect("rollback result matches the contract result schema");
        assert_eq!(rolled["replacement"]["phase"], "rolledBack");
        assert_eq!(rolled["replacement"]["state"], "cancelled");
        assert_eq!(
            history_len(&rt, &iid, &sid).await,
            4,
            "rollback keeps history"
        );

        // 幂等：重复回滚返回当前投影。
        let rev_after = rolled["replacement"]["baseRevision"]
            .as_str()
            .unwrap()
            .to_owned();
        let (_, again) = rt
            .dispatch(
                "replacement.rollback",
                json!({
                    "instanceId": iid,
                    "replacementId": "rep-1",
                    "expectedRevision": rev_after,
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(again["replacement"]["phase"], "rolledBack");

        // 已回滚的 plan 不再占线：同会话可再 prepare。
        let rev2 = session_rev(&rt, &iid, &sid).await;
        let (_, re_prepared) = rt
            .dispatch(
                "replacement.prepare",
                json!({
                    "instanceId": iid,
                    "replacementId": "rep-2",
                    "sessionId": sid,
                    "expectedRevision": rev2,
                    "expectedTurnId": "t-2",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(re_prepared["replacement"]["phase"], "prepared");
    }

    #[tokio::test]
    async fn prepare_rejections_cas_turn_conflict_busy_and_duplicates() {
        let tmp = TempDir::new("rpa-reject");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "r2").await;
        rt.store()
            .append_history_entries(&sid, &conversation())
            .unwrap();
        let rev = session_rev(&rt, &iid, &sid).await;

        // 陈旧 session revision：REVISION_CONFLICT，历史完整。
        let code = expect_code(
            &rt,
            "replacement.prepare",
            json!({
                "instanceId": iid,
                "replacementId": "rep-a",
                "sessionId": sid,
                "expectedRevision": "rev-bogus",
                "expectedTurnId": "t-2",
            }),
        )
        .await;
        assert_eq!(code, "REVISION_CONFLICT");
        assert_eq!(history_len(&rt, &iid, &sid).await, 4);

        // 陈旧 turn：TURN_CONFLICT。
        let code = expect_code(
            &rt,
            "replacement.prepare",
            json!({
                "instanceId": iid,
                "replacementId": "rep-a",
                "sessionId": sid,
                "expectedRevision": rev,
                "expectedTurnId": "t-1",
            }),
        )
        .await;
        assert_eq!(code, "TURN_CONFLICT");

        // 未知会话：NOT_FOUND。
        let code = expect_code(
            &rt,
            "replacement.prepare",
            json!({
                "instanceId": iid,
                "replacementId": "rep-a",
                "sessionId": "s-missing",
                "expectedRevision": rev,
                "expectedTurnId": "t-2",
            }),
        )
        .await;
        assert_eq!(code, "NOT_FOUND");

        // 成功后：重复 id → ALREADY_EXISTS；并发第二条 → REPLACEMENT_BUSY。
        let (_, prepared) = rt
            .dispatch(
                "replacement.prepare",
                json!({
                    "instanceId": iid,
                    "replacementId": "rep-a",
                    "sessionId": sid,
                    "expectedRevision": rev,
                    "expectedTurnId": "t-2",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        let code = expect_code(
            &rt,
            "replacement.prepare",
            json!({
                "instanceId": iid,
                "replacementId": "rep-a",
                "sessionId": sid,
                "expectedRevision": rev,
                "expectedTurnId": "t-2",
            }),
        )
        .await;
        assert_eq!(code, "ALREADY_EXISTS");
        let rev_after = prepared["replacement"]["baseRevision"]
            .as_str()
            .unwrap()
            .to_owned();
        // 占线：同会话已有未过期 prepared plan → REPLACEMENT_BUSY（freeze
        // 只推进全局计数器，sessions.revision 不动，CAS 仍通过）。
        let code = expect_code(
            &rt,
            "replacement.prepare",
            json!({
                "instanceId": iid,
                "replacementId": "rep-b",
                "sessionId": sid,
                "expectedRevision": session_rev(&rt, &iid, &sid).await,
                "expectedTurnId": "t-2",
            }),
        )
        .await;
        assert_eq!(code, "REPLACEMENT_BUSY");

        // 过期的 prepared plan 不占线：ttl=1ms 后可再 prepare。
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let (_, rolled) = rt
            .dispatch(
                "replacement.rollback",
                json!({
                    "instanceId": iid,
                    "replacementId": "rep-a",
                    "expectedRevision": rev_after,
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(rolled["replacement"]["phase"], "rolledBack");
        let rev_now = session_rev(&rt, &iid, &sid).await;
        let (_, expired_prep) = rt
            .dispatch(
                "replacement.prepare",
                json!({
                    "instanceId": iid,
                    "replacementId": "rep-c",
                    "sessionId": sid,
                    "expectedRevision": rev_now,
                    "expectedTurnId": "t-2",
                    "ttlMs": 1,
                }),
                None,
                None,
            )
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        // 过期后：get 报 expired（到期不改历史）。
        let (_, got) = rt
            .dispatch(
                "replacement.get",
                json!({"instanceId": iid, "replacementId": "rep-c"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(got["replacement"]["phase"], "expired");
        assert_eq!(got["replacement"]["state"], "cancelled");
        assert_eq!(history_len(&rt, &iid, &sid).await, 4);
        // 过期 plan 不占线（可再 prepare 新 id）。
        let rev_now = session_rev(&rt, &iid, &sid).await;
        let (_, again) = rt
            .dispatch(
                "replacement.prepare",
                json!({
                    "instanceId": iid,
                    "replacementId": "rep-d",
                    "sessionId": sid,
                    "expectedRevision": rev_now,
                    "expectedTurnId": "t-2",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(again["replacement"]["phase"], "prepared");

        // invalidate 机制（占线复查败者的清理路径）：直接种一条 + 调
        // invalidate → 行删除、id 可重试。
        rt.store()
            .freeze_replacement_plan(&sid, "rep-inv", "t-2", Some(60_000))
            .unwrap();
        rt.store()
            .invalidate_prepared_plan("rep-inv", "REPLACEMENT_BUSY")
            .unwrap();
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM replacement_plans WHERE replacement_id = 'rep-inv'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                count, 0,
                "invalidated plan row is removed; the id is retryable"
            );
        }
        // rollback 对已过CAS错位的 plan：REVISION_CONFLICT。
        let stale_base = again["replacement"]["baseRevision"]
            .as_str()
            .unwrap()
            .to_owned();
        let code = expect_code(
            &rt,
            "replacement.rollback",
            json!({
                "instanceId": iid,
                "replacementId": "rep-d",
                "expectedRevision": "rev-wrong",
            }),
        )
        .await;
        assert_eq!(code, "REVISION_CONFLICT");
        let (_, rolled_d) = rt
            .dispatch(
                "replacement.rollback",
                json!({
                    "instanceId": iid,
                    "replacementId": "rep-d",
                    "expectedRevision": stale_base,
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(rolled_d["replacement"]["phase"], "rolledBack");
    }

    #[tokio::test]
    async fn get_projects_committed_state_and_rollback_refuses_committed() {
        let tmp = TempDir::new("rpa-commit");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "r3").await;
        rt.store()
            .append_history_entries(&sid, &conversation())
            .unwrap();
        let rev = session_rev(&rt, &iid, &sid).await;

        let (_, prepared) = rt
            .dispatch(
                "replacement.prepare",
                json!({
                    "instanceId": iid,
                    "replacementId": "rep-1",
                    "sessionId": sid,
                    "expectedRevision": rev,
                    "expectedTurnId": "t-2",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        let base = prepared["replacement"]["baseRevision"]
            .as_str()
            .unwrap()
            .to_owned();

        // commit（t007 交付侧）→ get 投影 committed + 新 run 状态。新 run
        // 用 sleep 指令保持在途，非终态映射 deterministic 可断言。
        let (_, receipt) = rt
            .dispatch(
                "replacement.commit",
                json!({
                    "instanceId": iid,
                    "replacementId": "rep-1",
                    "input": [{"kind": "text", "text": "[pd-driver:sleep=8000]"}],
                    "expectedRevision": base,
                }),
                Some("key-rpa-1"),
                None,
            )
            .await
            .unwrap();
        let run_id = receipt["runId"].as_str().unwrap().to_owned();
        let (_, got) = rt
            .dispatch(
                "replacement.get",
                json!({"instanceId": iid, "replacementId": "rep-1"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(got["replacement"]["phase"], "committed");
        assert_eq!(got["replacement"]["replacementRunId"], run_id.as_str());
        assert_eq!(
            got["replacement"]["state"], "waiting",
            "the new run is not terminal yet (queued/running bucket)"
        );

        // 新 run 到终态后：get 的 state 跟随 run（取消路径 → cancelled）。
        rt.dispatch(
            "run.cancel",
            json!({"instanceId": iid, "runId": run_id}),
            Some("key-rpa-2"),
            None,
        )
        .await
        .unwrap();
        let _ = rt
            .dispatch(
                "run.wait",
                json!({"instanceId": iid, "runId": run_id, "timeoutMs": 5000}),
                None,
                None,
            )
            .await
            .unwrap();
        let (_, got) = rt
            .dispatch(
                "replacement.get",
                json!({"instanceId": iid, "replacementId": "rep-1"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(got["replacement"]["state"], "cancelled");

        // committed 拒绝回滚（不执行任何回滚动作）。
        let code = expect_code(
            &rt,
            "replacement.rollback",
            json!({
                "instanceId": iid,
                "replacementId": "rep-1",
                "expectedRevision": base,
            }),
        )
        .await;
        assert_eq!(code, "ALREADY_COMMITTED");
        // 历史保持 commit 后的形状（t-2 已被替换截断，仅剩 t-1 两轮）。
        assert_eq!(history_len(&rt, &iid, &sid).await, 2);

        // 未知 replacementId：NOT_FOUND。
        let code = expect_code(
            &rt,
            "replacement.get",
            json!({"instanceId": iid, "replacementId": "rep-missing"}),
        )
        .await;
        assert_eq!(code, "NOT_FOUND");
    }
}
