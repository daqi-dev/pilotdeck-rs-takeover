//! `subagent.types` / `subagent.list` / `subagent.history` — 子代理类型目录
//! 与历史（`cli.task.history`）。
//!
//! 合同正本 `state/cli-v1-operations.json#subagent.*`。TS 基线语义参考
//! （只读）：`gateway.method:supported_agents`（InProcessGateway.ts:2444，
//! 返回 `SUBAGENT_DEFINITIONS` 的固定四预置）与
//! `gateway.method:read_subagent_messages`（ui/server/routes/messages.js:137
//! 按 sessionKey+subagentId 读子代理 transcript）。旧 Rust 线
//! `dispatch.rs::supported_agents` 投影同一四预置目录。
//!
//! ## 数据源设计（设计决定，报告 §4）
//!
//! - **类型目录是服务端固定预置集**（与基线 `builtinSubagentTypes.ts`
//!   的四个 id 一致：general-purpose / explore / plan / verify），
//!   `available=true`、`unavailableReason=null`——目录编译在本网关内，
//!   与旧线 `supported_agents` 无条件列出一致。「模型文本名称不能注册
//!   真实身份」由此成立：目录是常量，不从消息正文派生。
//! - **v1 store 无子代理概念**：本域引入 `subagents`（子代理记录，按
//!   父 session/run 归属）与 `subagent_entries`（子代理历史，与
//!   `history_entries` 同形）两张表，写入口是
//!   [`V1Store::register_subagent`] / [`V1Store::append_subagent_entries`]
//!   / [`V1Store::update_subagent_state`]——供未来的子代理 spawn 操作
//!   接线（合同 285 操作中没有 spawn，故本交付内表为空、读操作返回完整
//!   空集合「无历史返回完整空集合」，不是错误）。
//! - **父 scope 授权**（「父scope授权必要」「父scope越权拒绝」）：
//!   `subagent.list` / `subagent.history` 都要求 parentSessionId+parentRunId
//!   是真实归属对（run 属于该会话、会话未删除）否则 `PARENT_MISMATCH`
//!   （两操作错误集均无 `NOT_FOUND` 之外的缺席码；`subagent.history` 的
//!   `NOT_FOUND` 保留给不存在的 subagentId）。「同名子代理在不同父
//!   run 不串」由 (parent_session_id, parent_run_id) 过滤 + 全局唯一
//!   subagent_id 共同保证。
//! - 分页游标钉住视图摘要（`SNAPSHOT_CHANGED` 不静默混页）；空数组
//!   `roles` 语义等同不过滤（与 search 一致，报告登记）。

use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_SNAPSHOT_CHANGED, ContractError, EffectStatus,
    RetrySpec,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::workspace_dirs::{bump_rev_in_tx, sql_err};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, now_rfc3339};

/// 合同声明而协议 crate 尚无常量的错误码。
pub(crate) const CODE_PARENT_MISMATCH: &str = "PARENT_MISMATCH";

/// 子代理历史条目的合法角色（与合同 `History.role` 枚举一致）。
const HISTORY_ROLES: [&str; 4] = ["system", "user", "assistant", "tool"];
/// Subagent.state 的合法值（与合同 `Subagent.state` 枚举一致）。
const SUBAGENT_STATES: [&str; 9] = [
    "queued",
    "running",
    "waiting",
    "cancelling",
    "succeeded",
    "failed",
    "cancelled",
    "interrupted",
    "result_unknown",
];

/// 固定预置目录（基线 `builtinSubagentTypes.ts` 的四个 id；描述性名称
/// 仅用于文档，合同 AgentType 只有 typeId/name/available）。
const SUBAGENT_TYPE_CATALOG: [&str; 4] = ["general-purpose", "explore", "plan", "verify"];
/// 目录内容版本（目录变化时递增；分页 snapshotRevision 用）。
const SUBAGENT_CATALOG_REVISION: &str = "subagent-types-1";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema(
        "subagents",
        "CREATE TABLE IF NOT EXISTS subagents (
            subagent_id TEXT PRIMARY KEY,
            parent_session_id TEXT NOT NULL,
            parent_run_id TEXT NOT NULL,
            type_id TEXT NOT NULL,
            state TEXT NOT NULL,
            revision TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_subagents_parent
            ON subagents(parent_session_id, parent_run_id);",
    );
    reg.schema(
        "subagent_entries",
        "CREATE TABLE IF NOT EXISTS subagent_entries (
            subagent_id TEXT NOT NULL,
            entry_id TEXT NOT NULL,
            seq INTEGER NOT NULL,
            turn_id TEXT NOT NULL,
            role TEXT NOT NULL,
            content_json TEXT NOT NULL,
            run_id TEXT,
            tool_call_id TEXT,
            created_at TEXT NOT NULL,
            PRIMARY KEY (subagent_id, entry_id)
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_subagent_entries_seq
            ON subagent_entries(subagent_id, seq);",
    );
    reg.handler("subagent.types", |rt, input, _key, _rev| {
        Box::pin(subagent_types(rt, input))
    });
    reg.handler("subagent.list", |rt, input, _key, _rev| {
        Box::pin(subagent_list(rt, input))
    });
    reg.handler("subagent.history", |rt, input, _key, _rev| {
        Box::pin(subagent_history(rt, input))
    });
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn invalid_argument(message: String) -> ContractError {
    ContractError::new(
        CODE_INVALID_ARGUMENT,
        message,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn not_found(kind: &str, id: &str) -> ContractError {
    ContractError::new(
        CODE_NOT_FOUND,
        format!("{kind} `{id}` not found"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn parent_mismatch(parent_session_id: &str, parent_run_id: &str, why: &str) -> ContractError {
    ContractError::new(
        CODE_PARENT_MISMATCH,
        format!(
            "parent scope (session `{parent_session_id}`, run `{parent_run_id}`) is not a valid parent pair: {why}"
        ),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn snapshot_changed(what: &str) -> ContractError {
    ContractError::new(
        CODE_SNAPSHOT_CHANGED,
        format!("the {what} changed while paginating; restart pagination"),
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

/// 确定性短摘要（视图钉扎用；operations_ops 的 digest_revision 同形）。
fn digest_revision(prefix: &str, parts: &[String]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    format!("{prefix}-{}", &format!("{:x}", hasher.finalize())[..24])
}

/// 合同 `page` 对象的 limit（1..500，默认 100）。
fn page_limit(page: Option<&Value>) -> usize {
    page.and_then(Value::as_object)
        .and_then(|obj| obj.get("limit"))
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 500) as usize
}

/// 校验一条历史条目的线形（history_archive::validate_history_entry 的
/// 领域内镜像：t012 的函数是模块私有，不可跨文件复用）。
fn validate_entry(entry: &Value) -> Result<(String, String, String), ContractError> {
    let turn_id = entry["turnId"].as_str().unwrap_or_default().to_owned();
    if turn_id.is_empty() {
        return Err(invalid_argument(
            "subagent history entry requires turnId".to_owned(),
        ));
    }
    let role = entry["role"].as_str().unwrap_or_default().to_owned();
    if !HISTORY_ROLES.contains(&role.as_str()) {
        return Err(invalid_argument(format!(
            "invalid subagent history role `{role}`"
        )));
    }
    let Some(parts) = entry["content"].as_array() else {
        return Err(invalid_argument(
            "subagent history entry content must be an array".to_owned(),
        ));
    };
    for part in parts {
        match part["kind"].as_str() {
            Some("text") => {
                if part.get("text").and_then(Value::as_str).is_none() {
                    return Err(invalid_argument(
                        "subagent history text part requires text".to_owned(),
                    ));
                }
            }
            Some("attachment") => {
                if part["artifactId"].as_str().unwrap_or_default().is_empty() {
                    return Err(invalid_argument(
                        "subagent history attachment part requires artifactId".to_owned(),
                    ));
                }
            }
            _ => {
                return Err(invalid_argument(
                    "subagent history part must be kind text or attachment".to_owned(),
                ));
            }
        }
    }
    let content_json = serde_json::to_string(parts).unwrap_or_else(|_| "[]".into());
    Ok((turn_id, role, content_json))
}

/// 行 → 合同 `History` 项（history_archive::history_item 的领域内镜像）。
fn history_item(
    entry_id: &str,
    turn_id: &str,
    role: &str,
    content_json: &str,
    run_id: Option<&str>,
    tool_call_id: Option<&str>,
    created_at: &str,
) -> Value {
    let content: Value = serde_json::from_str(content_json).unwrap_or(json!([]));
    json!({
        "entryId": entry_id,
        "turnId": turn_id,
        "role": role,
        "content": content,
        "runId": run_id,
        "toolCallId": tool_call_id,
        "createdAt": created_at,
    })
}

/// Subagent 行 → 合同 `Subagent` 项。
fn subagent_item(
    subagent_id: &str,
    parent_session_id: &str,
    parent_run_id: &str,
    type_id: &str,
    state: &str,
    revision: &str,
) -> Value {
    json!({
        "subagentId": subagent_id,
        "parentSessionId": parent_session_id,
        "parentRunId": parent_run_id,
        "typeId": type_id,
        "state": state,
        "revision": revision,
    })
}

/// 追加领域事件（workspace_dirs::append_domain_event_in_tx 的调用）。
fn append_subagent_event(
    conn: &Connection,
    resource_id: &str,
    event_type: &str,
    payload: &Value,
    occurred_at: &str,
) -> Result<(), ContractError> {
    let placeholder = format!(
        "tmp-subagent-{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    );
    let payload_str = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
    conn.execute(
        "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at)
         VALUES (?1, ?2, ?3, ?4, 'agentcpm-pd.event/1', ?5, ?5, ?6)",
        params![placeholder, resource_id, resource_id, event_type, payload_str, occurred_at],
    )
    .map_err(sql_err)?;
    let seq = conn.last_insert_rowid();
    conn.execute(
        "UPDATE events SET cursor = ?1 WHERE seq = ?2",
        params![format!("cur-{seq:012}"), seq],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// 校验父 scope 归属对（run 属于会话、会话存在且未删除）。失败一律
/// `PARENT_MISMATCH`（「错误父会话拒绝」）。
fn check_parent_scope(
    conn: &Connection,
    parent_session_id: &str,
    parent_run_id: &str,
) -> Result<(), ContractError> {
    let session: Option<String> = conn
        .query_row(
            "SELECT session_state FROM sessions WHERE session_id = ?1",
            params![parent_session_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    match session.as_deref() {
        None => {
            return Err(parent_mismatch(
                parent_session_id,
                parent_run_id,
                "parent session does not exist",
            ));
        }
        Some("deleted") => {
            return Err(parent_mismatch(
                parent_session_id,
                parent_run_id,
                "parent session is deleted",
            ));
        }
        Some(_) => {}
    }
    let run_session: Option<String> = conn
        .query_row(
            "SELECT session_id FROM runs WHERE run_id = ?1",
            params![parent_run_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    if run_session.as_deref() != Some(parent_session_id) {
        return Err(parent_mismatch(
            parent_session_id,
            parent_run_id,
            "parent run does not belong to the parent session",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// subagent.types
// ---------------------------------------------------------------------------

async fn subagent_types(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let project_id = input["projectId"].as_str().unwrap_or_default();
    let limit = page_limit(input.get("page"));

    let conn = rt.store().conn.lock().expect("sqlite mutex");
    // 未注册的项目：该 scope 名下无类型（空目录 + complete，不报错——
    // 错误集无 NOT_FOUND；「范围外不泄漏」）。
    let registered: Option<String> = conn
        .query_row(
            "SELECT project_id FROM projects WHERE project_id = ?1",
            params![project_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    let catalog: Vec<&str> = if registered.is_some() {
        SUBAGENT_TYPE_CATALOG.to_vec()
    } else {
        Vec::new()
    };

    let offset = match input
        .get("page")
        .and_then(Value::as_object)
        .and_then(|obj| obj.get("cursor"))
        .and_then(Value::as_str)
    {
        Some(cursor) => cursor
            .strip_prefix("sty-")
            .and_then(|rest| rest.parse::<usize>().ok())
            .ok_or_else(|| invalid_argument(format!("invalid subagent types cursor `{cursor}`")))?,
        None => 0,
    };
    if offset > catalog.len() {
        return Err(DispatchError::Application(invalid_argument(
            "pagination cursor beyond end of the type catalog".to_owned(),
        )));
    }
    let end = (offset + limit).min(catalog.len());
    let items: Vec<Value> = catalog[offset..end]
        .iter()
        .map(|type_id| {
            json!({
                "typeId": type_id,
                "name": type_id,
                "available": true,
                "unavailableReason": Value::Null,
            })
        })
        .collect();
    let complete = end >= catalog.len();
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "items": items,
            "pageInfo": {
                "snapshotRevision": SUBAGENT_CATALOG_REVISION,
                "nextCursor": if complete { Value::Null } else { json!(format!("sty-{end}")) },
                "complete": complete,
            },
        }),
    ))
}

// ---------------------------------------------------------------------------
// subagent.list
// ---------------------------------------------------------------------------

async fn subagent_list(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let parent_session_id = input["parentSessionId"].as_str().unwrap_or_default();
    let parent_run_id = input["parentRunId"].as_str().unwrap_or_default();
    let limit = page_limit(input.get("page"));

    let conn = rt.store().conn.lock().expect("sqlite mutex");
    check_parent_scope(&conn, parent_session_id, parent_run_id)?;

    // 视图摘要：所有 (id, state, revision)——任何变化使在途分页失效。
    let mut stmt = conn
        .prepare(
            "SELECT subagent_id, state, revision FROM subagents
             WHERE parent_session_id = ?1 AND parent_run_id = ?2
             ORDER BY created_at, subagent_id",
        )
        .map_err(sql_err)?;
    let view: Vec<(String, String, String)> = stmt
        .query_map(params![parent_session_id, parent_run_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .map_err(sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err)?;
    drop(stmt);
    let digest_parts: Vec<String> = view
        .iter()
        .flat_map(|(id, state, revision)| vec![id.clone(), state.clone(), revision.clone()])
        .collect();
    let digest = digest_revision("subview", &digest_parts);

    let offset = match input
        .get("page")
        .and_then(Value::as_object)
        .and_then(|obj| obj.get("cursor"))
        .and_then(Value::as_str)
    {
        Some(cursor) => {
            // 摘要本身带连字符（`subview-<hex>`），按最后一段取 offset。
            let (pinned, off) = cursor
                .strip_prefix("sav-")
                .and_then(|rest| rest.rsplit_once('-'))
                .and_then(|(d, off)| off.parse::<usize>().ok().map(|off| (d.to_owned(), off)))
                .ok_or_else(|| {
                    invalid_argument(format!("invalid subagent list cursor `{cursor}`"))
                })?;
            if pinned != digest {
                return Err(DispatchError::Application(snapshot_changed(
                    "parent run's subagent list",
                )));
            }
            off
        }
        None => 0,
    };
    if offset > view.len() {
        return Err(DispatchError::Application(invalid_argument(
            "pagination cursor beyond end of the subagent list".to_owned(),
        )));
    }

    let end = (offset + limit).min(view.len());
    let mut stmt = conn
        .prepare(
            "SELECT subagent_id, parent_session_id, parent_run_id, type_id, state, revision
             FROM subagents
             WHERE parent_session_id = ?1 AND parent_run_id = ?2
             ORDER BY created_at, subagent_id
             LIMIT ?3 OFFSET ?4",
        )
        .map_err(sql_err)?;
    let rows: Vec<(String, String, String, String, String, String)> = stmt
        .query_map(
            params![
                parent_session_id,
                parent_run_id,
                (end - offset) as i64,
                offset as i64
            ],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .map_err(sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err)?;
    let items: Vec<Value> = rows
        .iter()
        .map(|(id, ps, pr, type_id, state, revision)| {
            subagent_item(id, ps, pr, type_id, state, revision)
        })
        .collect();
    let complete = end >= view.len();
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "items": items,
            "pageInfo": {
                "snapshotRevision": digest,
                "nextCursor": if complete { Value::Null } else { json!(format!("sav-{digest}-{end}")) },
                "complete": complete,
            },
        }),
    ))
}

// ---------------------------------------------------------------------------
// subagent.history
// ---------------------------------------------------------------------------

async fn subagent_history(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let parent_session_id = input["parentSessionId"].as_str().unwrap_or_default();
    let parent_run_id = input["parentRunId"].as_str().unwrap_or_default();
    let subagent_id = input["subagentId"].as_str().unwrap_or_default();
    let limit = page_limit(input.get("page"));

    let conn = rt.store().conn.lock().expect("sqlite mutex");
    check_parent_scope(&conn, parent_session_id, parent_run_id)?;

    let row: Option<(String, String, String)> = conn
        .query_row(
            "SELECT parent_session_id, parent_run_id, revision
             FROM subagents WHERE subagent_id = ?1",
            params![subagent_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(sql_err)?;
    let Some((row_session, row_run, revision)) = row else {
        return Err(DispatchError::Application(not_found(
            "subagent",
            subagent_id,
        )));
    };
    // 父 scope 授权：给出的归属对必须与子代理实际归属一致（越权拒绝）。
    if row_session != parent_session_id || row_run != parent_run_id {
        return Err(DispatchError::Application(parent_mismatch(
            parent_session_id,
            parent_run_id,
            "the subagent belongs to a different parent scope",
        )));
    }

    // 视图摘要：条目数 + 子代理 revision（追加/状态变化都会改变摘要）。
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM subagent_entries WHERE subagent_id = ?1",
            params![subagent_id],
            |r| r.get(0),
        )
        .map_err(sql_err)?;
    let digest = digest_revision(
        "subhist",
        &[count.to_string(), revision, subagent_id.to_owned()],
    );

    // 游标携带 after_seq（追加型表、seq 从 1 连续，无删除路径）。
    let after_seq = match input
        .get("page")
        .and_then(Value::as_object)
        .and_then(|obj| obj.get("cursor"))
        .and_then(Value::as_str)
    {
        Some(cursor) => {
            // 摘要本身带连字符（`subhist-<hex>`），按最后一段取 after_seq。
            let (pinned, off) = cursor
                .strip_prefix("sah-")
                .and_then(|rest| rest.rsplit_once('-'))
                .and_then(|(d, off)| off.parse::<usize>().ok().map(|off| (d.to_owned(), off)))
                .ok_or_else(|| {
                    invalid_argument(format!("invalid subagent history cursor `{cursor}`"))
                })?;
            if pinned != digest {
                return Err(DispatchError::Application(snapshot_changed(
                    "subagent history view",
                )));
            }
            off
        }
        None => 0,
    };
    if after_seq > count as usize {
        return Err(DispatchError::Application(invalid_argument(
            "pagination cursor beyond end of the subagent history".to_owned(),
        )));
    }

    let mut stmt = conn
        .prepare(
            "SELECT entry_id, turn_id, role, content_json, run_id, tool_call_id, created_at
             FROM subagent_entries WHERE subagent_id = ?1 AND seq > ?2
             ORDER BY seq ASC LIMIT ?3",
        )
        .map_err(sql_err)?;
    let rows: Vec<(
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        String,
    )> = stmt
        .query_map(
            params![subagent_id, after_seq as i64, (limit + 1) as i64],
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
        .map_err(sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err)?;
    drop(stmt);
    let complete = rows.len() <= limit;
    let items: Vec<Value> = rows
        .into_iter()
        .take(limit)
        .map(
            |(entry_id, turn_id, role, content_json, run_id, tool_call_id, created_at)| {
                history_item(
                    &entry_id,
                    &turn_id,
                    &role,
                    &content_json,
                    run_id.as_deref(),
                    tool_call_id.as_deref(),
                    &created_at,
                )
            },
        )
        .collect();
    let end = (after_seq + items.len()) as i64;
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "items": items,
            "pageInfo": {
                "snapshotRevision": digest,
                "nextCursor": if complete { Value::Null } else { json!(format!("sah-{digest}-{end}")) },
                "complete": complete,
            },
        }),
    ))
}

// ---------------------------------------------------------------------------
// Store write API（未来子代理 spawn 操作的接线点；测试用于种夹具）
// ---------------------------------------------------------------------------

impl V1Store {
    /// 登记一个子代理（父 session/run 归属 + 类型 + 初始状态）。父归属
    /// 对不真实、类型不在目录中、状态不在枚举内或 id 已存在时拒绝。
    /// 返回合同 `Subagent` 形状。
    pub fn register_subagent(
        &self,
        subagent_id: &str,
        parent_session_id: &str,
        parent_run_id: &str,
        type_id: &str,
        state: &str,
    ) -> Result<Value, ContractError> {
        if !SUBAGENT_TYPE_CATALOG.contains(&type_id) {
            return Err(invalid_argument(format!(
                "unknown subagent type `{type_id}`; the catalog is server-owned"
            )));
        }
        if !SUBAGENT_STATES.contains(&state) {
            return Err(invalid_argument(format!(
                "invalid subagent state `{state}`"
            )));
        }
        let now = now_rfc3339();
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(sql_err)?;
        check_parent_scope(&tx, parent_session_id, parent_run_id)?;
        let dup: Option<String> = tx
            .query_row(
                "SELECT subagent_id FROM subagents WHERE subagent_id = ?1",
                params![subagent_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if dup.is_some() {
            return Err(ContractError::new(
                agentcpm_pd_protocol::errors::CODE_ALREADY_EXISTS,
                format!("subagent `{subagent_id}` is already registered"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        let rev = bump_rev_in_tx(&tx)?;
        tx.execute(
            "INSERT INTO subagents
             (subagent_id, parent_session_id, parent_run_id, type_id, state, revision, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            params![
                subagent_id,
                parent_session_id,
                parent_run_id,
                type_id,
                state,
                rev,
                now
            ],
        )
        .map_err(sql_err)?;
        append_subagent_event(
            &tx,
            subagent_id,
            "subagent.registered",
            &json!({
                "subagentId": subagent_id,
                "parentSessionId": parent_session_id,
                "parentRunId": parent_run_id,
                "typeId": type_id,
                "state": state,
                "revision": rev,
            }),
            &now,
        )?;
        tx.commit().map_err(sql_err)?;
        self.event_notifier().notify_waiters();
        Ok(subagent_item(
            subagent_id,
            parent_session_id,
            parent_run_id,
            type_id,
            state,
            &rev,
        ))
    }

    /// 追加子代理历史条目（entryId 与 per-subagent seq 由存储分配）。
    /// 追加会推进子代理 revision（分页摘要随之变化）。
    pub fn append_subagent_entries(
        &self,
        subagent_id: &str,
        entries: &[Value],
    ) -> Result<Value, ContractError> {
        let now = now_rfc3339();
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(sql_err)?;
        let exists: Option<String> = tx
            .query_row(
                "SELECT subagent_id FROM subagents WHERE subagent_id = ?1",
                params![subagent_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if exists.is_none() {
            return Err(not_found("subagent", subagent_id));
        }
        let mut max_seq: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM subagent_entries WHERE subagent_id = ?1",
                params![subagent_id],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        let mut items = Vec::with_capacity(entries.len());
        for entry in entries {
            let (turn_id, role, content_json) = validate_entry(entry)?;
            let entry_id = self.next_id();
            max_seq += 1;
            let created_at = entry
                .get("createdAt")
                .and_then(Value::as_str)
                .unwrap_or(&now)
                .to_owned();
            let run_id = entry.get("runId").and_then(Value::as_str);
            let tool_call_id = entry.get("toolCallId").and_then(Value::as_str);
            tx.execute(
                "INSERT INTO subagent_entries
                 (subagent_id, entry_id, seq, turn_id, role, content_json, run_id, tool_call_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    subagent_id,
                    entry_id,
                    max_seq,
                    turn_id,
                    role,
                    content_json,
                    run_id,
                    tool_call_id,
                    created_at
                ],
            )
            .map_err(sql_err)?;
            items.push(history_item(
                &entry_id,
                &turn_id,
                &role,
                &content_json,
                run_id,
                tool_call_id,
                &created_at,
            ));
        }
        let rev = bump_rev_in_tx(&tx)?;
        tx.execute(
            "UPDATE subagents SET revision = ?1, updated_at = ?2 WHERE subagent_id = ?3",
            params![rev, now, subagent_id],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(json!({"items": items}))
    }

    /// 更新子代理状态（未来生命周期接线的落点；幂等于同值）。
    pub fn update_subagent_state(
        &self,
        subagent_id: &str,
        state: &str,
    ) -> Result<Value, ContractError> {
        if !SUBAGENT_STATES.contains(&state) {
            return Err(invalid_argument(format!(
                "invalid subagent state `{state}`"
            )));
        }
        let now = now_rfc3339();
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(sql_err)?;
        let current: Option<(String, String)> = tx
            .query_row(
                "SELECT state, revision FROM subagents WHERE subagent_id = ?1",
                params![subagent_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        let Some((old_state, old_rev)) = current else {
            return Err(not_found("subagent", subagent_id));
        };
        if old_state == state {
            return Ok(json!({"subagentId": subagent_id, "state": state, "revision": old_rev}));
        }
        let rev = bump_rev_in_tx(&tx)?;
        tx.execute(
            "UPDATE subagents SET state = ?1, revision = ?2, updated_at = ?3 WHERE subagent_id = ?4",
            params![state, rev, now, subagent_id],
        )
        .map_err(sql_err)?;
        append_subagent_event(
            &tx,
            subagent_id,
            "subagent.state_changed",
            &json!({"subagentId": subagent_id, "state": state, "revision": rev}),
            &now,
        )?;
        tx.commit().map_err(sql_err)?;
        self.event_notifier().notify_waiters();
        Ok(json!({"subagentId": subagent_id, "state": state, "revision": rev}))
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

    /// 项目 + 会话 + 一个终态 run（父归属对的种子）。
    async fn seeded(
        rt: &std::sync::Arc<PublicV1Runtime>,
        iid: &str,
        tag: &str,
    ) -> (String, String) {
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
        let (_, receipt) = rt
            .dispatch(
                "run.submit",
                json!({
                    "instanceId": iid,
                    "sessionId": &session_id,
                    "input": [{"kind": "text", "text": "parent run"}],
                }),
                Some(&format!("key-parent-{tag}")),
                None,
            )
            .await
            .unwrap();
        let run_id = receipt["runId"].as_str().unwrap().to_owned();
        let _ = rt
            .dispatch(
                "run.wait",
                json!({"instanceId": iid, "runId": &run_id, "timeoutMs": 5000}),
                None,
                None,
            )
            .await
            .unwrap();
        (session_id, run_id)
    }

    #[tokio::test]
    async fn subagent_types_lists_the_server_catalog_and_distinguishes_no_history() {
        let tmp = TempDir::new("suba-types");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let _ = seeded(&rt, &iid, "st1").await;

        let (_, types) = rt
            .dispatch(
                "subagent.types",
                json!({"instanceId": iid, "projectId": "p-st1"}),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("subagent.types", &types)
            .expect("types page matches the contract result schema");
        let ids: Vec<&str> = types["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["typeId"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            vec!["general-purpose", "explore", "plan", "verify"],
            "the server-owned preset catalog"
        );
        assert!(types["items"][0]["available"].as_bool().unwrap());
        assert_eq!(types["pageInfo"]["complete"], true);

        // 未注册项目：空目录 + complete（未装配与无历史区分——类型目录
        // 存在与否不靠报错表达）。
        let (_, empty) = rt
            .dispatch(
                "subagent.types",
                json!({"instanceId": iid, "projectId": "p-nope"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(empty["items"].as_array().unwrap().len(), 0);
        assert_eq!(empty["pageInfo"]["complete"], true);
    }

    #[tokio::test]
    async fn subagent_list_and_history_enforce_parent_scope_and_isolation() {
        let tmp = TempDir::new("suba-scope");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let (sid_a, run_a) = seeded(&rt, &iid, "sa1").await;
        let (sid_b, run_b) = seeded(&rt, &iid, "sa2").await;

        // 同名（同 typeId）子代理在不同父 run：两条独立记录，互不串。
        rt.store()
            .register_subagent("sub-a-1", &sid_a, &run_a, "explore", "succeeded")
            .unwrap();
        rt.store()
            .register_subagent("sub-b-1", &sid_b, &run_b, "explore", "running")
            .unwrap();
        rt.store()
            .append_subagent_entries(
                "sub-a-1",
                &[
                    json!({"turnId": "t-1", "role": "user", "content": [{"kind": "text", "text": "scan the workspace"}]}),
                    json!({"turnId": "t-1", "role": "assistant", "content": [{"kind": "text", "text": "found 3 files"}]}),
                    // tool 角色与 runId/toolCallId 关联保留。
                    json!({"turnId": "t-2", "role": "tool", "content": [{"kind": "text", "text": "grep ok"}], "runId": run_a, "toolCallId": "call-1"}),
                ],
            )
            .unwrap();
        rt.store()
            .append_subagent_entries(
                "sub-b-1",
                &[json!({"turnId": "t-1", "role": "assistant", "content": [{"kind": "text", "text": "b side only"}]})],
            )
            .unwrap();

        // list 按 (parentSessionId, parentRunId) 隔离。
        let (_, list_a) = rt
            .dispatch(
                "subagent.list",
                json!({"instanceId": iid, "parentSessionId": sid_a, "parentRunId": run_a}),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("subagent.list", &list_a)
            .expect("list page matches the contract result schema");
        let ids_a: Vec<&str> = list_a["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["subagentId"].as_str().unwrap())
            .collect();
        assert_eq!(ids_a, vec!["sub-a-1"]);
        assert_eq!(list_a["items"][0]["typeId"], "explore");
        assert_eq!(list_a["items"][0]["state"], "succeeded");
        let (_, list_b) = rt
            .dispatch(
                "subagent.list",
                json!({"instanceId": iid, "parentSessionId": sid_b, "parentRunId": run_b}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(list_b["items"].as_array().unwrap().len(), 1);
        assert_eq!(list_b["items"][0]["subagentId"], "sub-b-1");

        // 错误父会话 / 不属于会话的 run / 不存在的会话：PARENT_MISMATCH。
        let code = expect_code(
            &rt,
            "subagent.list",
            json!({"instanceId": iid, "parentSessionId": sid_b, "parentRunId": run_a}),
        )
        .await;
        assert_eq!(code, "PARENT_MISMATCH");
        let code = expect_code(
            &rt,
            "subagent.list",
            json!({"instanceId": iid, "parentSessionId": "s-missing", "parentRunId": run_a}),
        )
        .await;
        assert_eq!(code, "PARENT_MISMATCH");

        // history：A 的子代理历史完整（角色与 toolCallId 关联保留）。
        let (_, history) = rt
            .dispatch(
                "subagent.history",
                json!({
                    "instanceId": iid,
                    "parentSessionId": sid_a,
                    "parentRunId": run_a,
                    "subagentId": "sub-a-1",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("subagent.history", &history)
            .expect("history page matches the contract result schema");
        let entries = history["items"].as_array().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0]["role"], "user");
        assert_eq!(entries[2]["role"], "tool");
        assert_eq!(entries[2]["toolCallId"], "call-1");
        assert_eq!(entries[2]["runId"], run_a.as_str());

        // 越权：用 B 的父 scope 读 A 的子代理 → PARENT_MISMATCH。
        let code = expect_code(
            &rt,
            "subagent.history",
            json!({
                "instanceId": iid,
                "parentSessionId": sid_b,
                "parentRunId": run_b,
                "subagentId": "sub-a-1",
            }),
        )
        .await;
        assert_eq!(code, "PARENT_MISMATCH");
        // 不存在的子代理：NOT_FOUND（父 scope 本身合法）。
        let code = expect_code(
            &rt,
            "subagent.history",
            json!({
                "instanceId": iid,
                "parentSessionId": sid_a,
                "parentRunId": run_a,
                "subagentId": "sub-missing",
            }),
        )
        .await;
        assert_eq!(code, "NOT_FOUND");

        // B 侧子代理只有 B 的历史（不串）。
        let (_, history_b) = rt
            .dispatch(
                "subagent.history",
                json!({
                    "instanceId": iid,
                    "parentSessionId": sid_b,
                    "parentRunId": run_b,
                    "subagentId": "sub-b-1",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        let entries_b = history_b["items"].as_array().unwrap();
        assert_eq!(entries_b.len(), 1);
        assert_eq!(entries_b[0]["turnId"], "t-1");
        assert_eq!(entries_b[0]["content"][0]["text"], "b side only");
    }

    #[tokio::test]
    async fn subagent_empty_history_is_complete_and_pages_pin_the_view() {
        let tmp = TempDir::new("suba-page");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let (sid, run_id) = seeded(&rt, &iid, "sp1").await;

        // 无子代理的父 run：list 是完整空集合（不是错误）。
        let (_, list) = rt
            .dispatch(
                "subagent.list",
                json!({"instanceId": iid, "parentSessionId": sid, "parentRunId": run_id}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(list["items"].as_array().unwrap().len(), 0);
        assert_eq!(list["pageInfo"]["complete"], true);

        rt.store()
            .register_subagent("sub-p-1", &sid, &run_id, "plan", "running")
            .unwrap();
        let mut entries = Vec::new();
        for n in 0..3 {
            entries.push(json!({
                "turnId": format!("t-{n}"),
                "role": "assistant",
                "content": [{"kind": "text", "text": format!("step {n}")}],
            }));
        }
        rt.store()
            .append_subagent_entries("sub-p-1", &entries)
            .unwrap();

        // 无历史返回完整空集合：另一个刚登记的子代理。
        rt.store()
            .register_subagent("sub-p-2", &sid, &run_id, "verify", "queued")
            .unwrap();
        let (_, empty) = rt
            .dispatch(
                "subagent.history",
                json!({
                    "instanceId": iid,
                    "parentSessionId": sid,
                    "parentRunId": run_id,
                    "subagentId": "sub-p-2",
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(empty["items"].as_array().unwrap().len(), 0);
        assert_eq!(empty["pageInfo"]["complete"], true);

        // 分页 + 视图钉扎：页间追加使摘要变化 → SNAPSHOT_CHANGED。
        let (_, page1) = rt
            .dispatch(
                "subagent.history",
                json!({
                    "instanceId": iid,
                    "parentSessionId": sid,
                    "parentRunId": run_id,
                    "subagentId": "sub-p-1",
                    "page": {"limit": 2},
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(page1["items"].as_array().unwrap().len(), 2);
        assert_eq!(page1["pageInfo"]["complete"], false);
        let cursor = page1["pageInfo"]["nextCursor"].as_str().unwrap().to_owned();
        rt.store()
            .append_subagent_entries(
                "sub-p-1",
                &[json!({"turnId": "t-9", "role": "assistant", "content": [{"kind": "text", "text": "late entry"}]})],
            )
            .unwrap();
        let code = expect_code(
            &rt,
            "subagent.history",
            json!({
                "instanceId": iid,
                "parentSessionId": sid,
                "parentRunId": run_id,
                "subagentId": "sub-p-1",
                "page": {"limit": 2, "cursor": cursor},
            }),
        )
        .await;
        assert_eq!(code, "SNAPSHOT_CHANGED");

        // 坏游标：INVALID_ARGUMENT。
        let code = expect_code(
            &rt,
            "subagent.history",
            json!({
                "instanceId": iid,
                "parentSessionId": sid,
                "parentRunId": run_id,
                "subagentId": "sub-p-1",
                "page": {"cursor": "bogus"},
            }),
        )
        .await;
        assert_eq!(code, "INVALID_ARGUMENT");

        // 写入口的防御：未知类型 / 未知状态 / 重复登记。
        let error = rt
            .store()
            .register_subagent("sub-x", &sid, &run_id, "made-up-type", "queued")
            .unwrap_err();
        assert_eq!(error.code, "INVALID_ARGUMENT");
        let error = rt
            .store()
            .register_subagent("sub-x", &sid, &run_id, "explore", "zombie")
            .unwrap_err();
        assert_eq!(error.code, "INVALID_ARGUMENT");
        let error = rt
            .store()
            .register_subagent("sub-p-1", &sid, &run_id, "explore", "queued")
            .unwrap_err();
        assert_eq!(error.code, "ALREADY_EXISTS");

        // 状态更新推进 revision（list 视图摘要变化 → SNAPSHOT_CHANGED）。
        let (_, list1) = rt
            .dispatch(
                "subagent.list",
                json!({
                    "instanceId": iid,
                    "parentSessionId": sid,
                    "parentRunId": run_id,
                    "page": {"limit": 1},
                }),
                None,
                None,
            )
            .await
            .unwrap();
        let list_cursor = list1["pageInfo"]["nextCursor"].as_str().unwrap().to_owned();
        rt.store()
            .update_subagent_state("sub-p-1", "succeeded")
            .unwrap();
        let code = expect_code(
            &rt,
            "subagent.list",
            json!({
                "instanceId": iid,
                "parentSessionId": sid,
                "parentRunId": run_id,
                "page": {"limit": 1, "cursor": list_cursor},
            }),
        )
        .await;
        assert_eq!(code, "SNAPSHOT_CHANGED");
    }
}
