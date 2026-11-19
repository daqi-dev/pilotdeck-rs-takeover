//! `search.start` / `search.results` — 跨会话历史搜索（`cli.task.history`）。
//!
//! 合同正本 `state/cli-v1-operations.json#search.start / #search.results`。
//! 语义参考（只读）：TS 基线 `src/cli/commands/chatSearch.ts` +
//! `session/search/NodeSessionSearchPort.ts`，旧 Rust 线
//! `pilotdeck-cli/src/chat_search.rs` + `channel/commands/search/`（query/
//! scope/role/regex/大小写/limit 组合语义）。本文件不复制旧线代码，按 v1
//! 合同实现，与旧线的差异登记在任务报告（最重要的三条：v1 的 regex 是
//! fancy-regex 直译而非旧线的 V8 方言翻译层；v1 的非法模式是合同码
//! `INVALID_PATTERN` 而非 CLI usage error；v1 的 `pageLimit` 是命中总数上
//! 限而非仅显示条数——旧 `--limit` 截断语义的承接）。
//!
//! ## 数据源与选型（设计决定，报告 §4）
//!
//! - **不建全文索引**：合同的查询形态是 `literal`（子串）与 `regex`，
//!   FTS5 的 MATCH 语法两者都不覆盖（token 化的子串匹配不可靠、regex
//!   完全不支持）。搜索直接扫权威存储 `history_entries`（t012 的会话
//!   历史表），分块有界扫描（`MAX_SCAN_ENTRIES` 条上限、每次一块、块间
//!   响应取消），无需维护索引一致性——因此「索引落后水位」仅在扫描触
//!   顶截断时出现（`indexingComplete=false` + `searchedThrough` 水位），
//!   正常完成即 `indexingComplete=true`。（rusqlite bundled 的 FTS5 可用
//!   性结论见报告：libsqlite3-sys 0.37 build.rs 带
//!   `-DSQLITE_ENABLE_FTS5`，但本选型不需要它。）
//! - **异步受理**：`search.start`（`retry=idempotentSubmit`）受理事务原
//!   子完成「幂等闸复验 + collections 缝的 `operations` 行（queued）+
//!   `search_snapshots` 行 + 幂等回执」，随后后台任务扫描并把命中集
//!   **物化**进 `search_hits`（不可变快照），completionResult
//!   `{searchId, indexingComplete, searchedThrough, resultCount}` 落在
//!   缝行的 `result_json`（`operations.get` 直接可查，`searchId ==
//!   operationId`）。扫描的存储故障落 `INDEX_UNAVAILABLE`、regex 回溯
//!   上限落 `RESOURCE_LIMIT`（操作失败可经 `operations.get` 的 error 察
//!   觉，`search.results` 对非 succeeded 终态报 `OPERATION_NOT_READY`，
//!   不以空成功伪装失败）。
//! - **search.results 分页**：物化命中集是不可变快照；读时联查
//!   `history_entries`/`sessions` 做「已删除不泄漏」过滤（会话被删或
//!   条目被 replacement 截断后，命中从可见集中消失）。页游标钉住可见
//!   命中数（`srch-<n>-<offset>`），中途可见集变化 → `SNAPSHOT_CHANGED`
//!   而非静默混页。快照按 7 天保留期惰性清理（新搜索受理时顺带清
//!   `updated_at` 超期的终态行）；命中集已被清理 → `CURSOR_EXPIRED`。
//! - **scope 越界**：不存在的 project/session scope 不报错（错误集无
//!   `NOT_FOUND`），按零命中处理——与「范围外不泄漏」及 TS 基线「缺
//!   session 文件 → 无命中」一致。只有 `session_state='deleted'` 的会话
//!   及其条目被排除；`project.remove` 只注销登记不删会话，其余话仍可
//!   搜（报告登记）。
//!
//! ## 并发与秘密
//!
//! 扫描每块短暂持有 store 连接锁（读），块间让出（「搜索只读可并行」：
//! 并发搜索按块交错，不长期独占写入口）；取消意图通过缝行的
//! `cancel_requested` 轮询（`operations.cancel` 受理）。excerpt 取自命中
//! 正文（正文本身是会话内容，不涉密钥；查询词不落日志）。

use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::envelope::compute_input_digest;
use agentcpm_pd_protocol::errors::{
    CODE_CURSOR_EXPIRED, CODE_IDEMPOTENCY_CONFLICT, CODE_INVALID_ARGUMENT, CODE_NOT_FOUND,
    CODE_RESOURCE_LIMIT, CODE_SNAPSHOT_CHANGED, ContractError, EffectStatus, RetrySpec,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::domains::operations_ops::LongOperationOutcome;
use crate::public_v1::domains::workspace_dirs::{bump_rev_in_tx, sql_err};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, now_rfc3339};

/// 合同声明而协议 crate 尚无常量的错误码。
pub(crate) const CODE_INVALID_PATTERN: &str = "INVALID_PATTERN";
pub(crate) const CODE_INDEX_UNAVAILABLE: &str = "INDEX_UNAVAILABLE";
pub(crate) const CODE_OPERATION_NOT_READY: &str = "OPERATION_NOT_READY";

/// 扫描条目上限（有界扫描的「界」）。
const MAX_SCAN_ENTRIES: usize = 100_000;
/// 命中物化上限（命中集大小的实例硬顶，`pageLimit` 再大也不越过它）。
const MAX_HITS: usize = 10_000;
/// 每块扫描的行数。
const SCAN_CHUNK: i64 = 1_000;
/// 物化快照保留期（与幂等记录 retained_until 同宽）。
const SNAPSHOT_RETENTION_DAYS: i64 = 7;
/// excerpt 视窗：命中点前后各取的字符数（旧线 snippet ±60 units 同宽）。
const EXCERPT_CONTEXT_CHARS: usize = 60;

const OPERATION_EVENT_SCHEMA: &str = "agentcpm-pd.operation.event/1";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema(
        "search_hits",
        "CREATE TABLE IF NOT EXISTS search_hits (
            operation_id TEXT NOT NULL,
            position INTEGER NOT NULL,
            project_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            entry_id TEXT NOT NULL,
            role TEXT NOT NULL,
            excerpt TEXT NOT NULL,
            history_revision TEXT NOT NULL,
            PRIMARY KEY (operation_id, position)
        );
        CREATE INDEX IF NOT EXISTS idx_search_hits_entry
            ON search_hits(session_id, entry_id);",
    );
    reg.schema(
        "search_snapshots",
        "CREATE TABLE IF NOT EXISTS search_snapshots (
            operation_id TEXT PRIMARY KEY,
            created_at TEXT NOT NULL
        );",
    );
    reg.handler("search.start", |rt, input, key, _rev| {
        Box::pin(search_start(rt, input, key))
    });
    reg.handler("search.results", |rt, input, _key, _rev| {
        Box::pin(search_results(rt, input))
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

fn operation_not_ready(operation_id: &str, why: &str) -> ContractError {
    ContractError::new(
        CODE_OPERATION_NOT_READY,
        format!("search operation `{operation_id}` has no result snapshot yet: {why}"),
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::NotApplied,
    )
}

/// 解析合同 `Scope`（schema 已保证形状）。
#[derive(Clone, Debug)]
enum SearchScope {
    Instance,
    Project {
        project_id: String,
    },
    Session {
        project_id: String,
        session_id: String,
    },
}

impl SearchScope {
    fn parse(input: &Value) -> Self {
        let scope = input.get("scope").cloned().unwrap_or(Value::Null);
        match scope.get("kind").and_then(Value::as_str) {
            Some("project") => Self::Project {
                project_id: scope["projectId"].as_str().unwrap_or_default().to_owned(),
            },
            Some("session") => Self::Session {
                project_id: scope["projectId"].as_str().unwrap_or_default().to_owned(),
                session_id: scope["sessionId"].as_str().unwrap_or_default().to_owned(),
            },
            _ => Self::Instance,
        }
    }

    /// collections 缝行的 scope 三元组（scope_kind, project_id, session_id）。
    fn seam_columns(&self) -> (&'static str, Option<String>, Option<String>) {
        match self {
            Self::Instance => ("instance", None, None),
            Self::Project { project_id } => ("project", Some(project_id.clone()), None),
            Self::Session {
                project_id,
                session_id,
            } => (
                "session",
                Some(project_id.clone()),
                Some(session_id.clone()),
            ),
        }
    }
}

/// 搜索执行参数（受理时快照，后台扫描消费；语法与大小写已烘焙进
/// [`SearchMatcher`]，无需重复携带）。
struct SearchParams {
    query: String,
    roles: Vec<String>,
    scope: SearchScope,
    /// 命中总数上限（pageLimit，受 MAX_HITS 硬顶）。
    hit_cap: usize,
}

/// 一次扫描物化的命中行。
struct ScannedHit {
    project_id: String,
    session_id: String,
    entry_id: String,
    role: String,
    excerpt: String,
    history_revision: String,
}

/// 匹配器：literal 以逐字符类包裹转义（对任何正则方言都无歧义），regex
/// 原样编译；case-insensitive 由 fancy-regex 的 Unicode 折叠完成。
#[derive(Debug)]
struct SearchMatcher {
    pattern: fancy_regex::Regex,
}

impl SearchMatcher {
    /// 编译失败返回错误细节（调用方映射为 `INVALID_PATTERN`）。
    fn compile(query: &str, regex: bool, case_sensitive: bool) -> Result<Self, String> {
        let source = if regex {
            query.to_owned()
        } else {
            Self::escape_literal(query)
        };
        let mut builder = fancy_regex::RegexBuilder::new(&source);
        builder.case_insensitive(!case_sensitive);
        builder
            .build()
            .map(|pattern| Self { pattern })
            .map_err(|error| error.to_string())
    }

    /// 字面查询的安全转义：每个字符包进单字符类 `[c]`。类内只需转义
    /// `\` `]` `^` `-` `[`，其余（含 `.*+?(){}|$&~#`）在类内都是字面量。
    /// `[` 必须转义：fancy-regex 的类解析器把类内裸 `[` 计为嵌套类开括号
    /// （parse_class 的 nest 计数），`[[]` 会留下未闭合的外层类、在串尾报
    /// InvalidClass——字面查询含 `[` 时曾被误判为非法 pattern。
    fn escape_literal(query: &str) -> String {
        let mut out = String::with_capacity(query.len() * 3);
        for ch in query.chars() {
            match ch {
                '\\' => out.push_str("[\\\\]"),
                ']' => out.push_str("[\\]]"),
                '^' => out.push_str("[\\^]"),
                '-' => out.push_str("[\\-]"),
                '[' => out.push_str("[\\[]"),
                _ => {
                    out.push('[');
                    out.push(ch);
                    out.push(']');
                }
            }
        }
        out
    }

    /// 首个命中的字符区间（excerpt 视窗用）。运行期失败（回溯上限）以
    /// `Err` 上抛，不当「无命中」处理（「失败不返回空成功」）。
    fn find(&self, text: &str) -> Result<Option<(usize, usize)>, String> {
        self.pattern
            .find(text)
            .map(|found| found.map(|m| (m.start(), m.end())))
            .map_err(|error| error.to_string())
    }
}

/// 从 content_json 提取可搜索正文：全部 text part 以 `\n` 连接。
/// 解析失败的条目按无正文处理（跳过匹配，不崩溃）。
fn searchable_text(content_json: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(content_json) else {
        return String::new();
    };
    let Some(parts) = value.as_array() else {
        return String::new();
    };
    let mut texts = Vec::new();
    for part in parts {
        if part["kind"].as_str() == Some("text")
            && let Some(text) = part["text"].as_str()
            && !text.is_empty()
        {
            texts.push(text);
        }
    }
    texts.join("\n")
}

/// 命中点前后的固定宽视窗，越界侧补省略号（旧线 ±60 units 同宽语义）。
fn excerpt_around(text: &str, start: usize, end: usize) -> String {
    let total = text.chars().count();
    let start_char = text[..start].chars().count();
    let end_char = start_char + text[start..end].chars().count();
    let from = start_char.saturating_sub(EXCERPT_CONTEXT_CHARS);
    let to = (end_char + EXCERPT_CONTEXT_CHARS).min(total);
    let mut out = String::new();
    if from > 0 {
        out.push_str("...");
    }
    out.extend(text.chars().skip(from).take(to - from));
    if to < total {
        out.push_str("...");
    }
    out
}

/// 缝行操作事件（`operations_ops::append_operation_event_in_tx` 的领域内
/// 镜像：占位游标 + 回填，schema id 与操作事件一致）。
fn append_search_operation_event(
    conn: &Connection,
    operation_id: &str,
    session_id: &str,
    event_type: &str,
    payload: &Value,
    occurred_at: &str,
) -> Result<String, ContractError> {
    let placeholder = format!(
        "tmp-search-{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    );
    let payload_str = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
    conn.execute(
        "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)",
        params![
            placeholder,
            operation_id,
            session_id,
            event_type,
            OPERATION_EVENT_SCHEMA,
            payload_str,
            occurred_at
        ],
    )
    .map_err(sql_err)?;
    let seq = conn.last_insert_rowid();
    conn.execute(
        "UPDATE events SET cursor = ?1 WHERE seq = ?2",
        params![format!("cur-{seq:012}"), seq],
    )
    .map_err(sql_err)?;
    Ok(format!("cur-{seq:012}"))
}

// ---------------------------------------------------------------------------
// search.start (async, idempotentSubmit)
// ---------------------------------------------------------------------------

#[allow(clippy::large_enum_variant)]
enum SearchSubmission {
    Deduplicated {
        receipt: Value,
    },
    New {
        receipt: Value,
        operation_id: String,
        matcher: SearchMatcher,
        params: SearchParams,
    },
}

async fn search_start(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let submission = store.submit_search(&input, idempotency_key.as_deref())?;
    match submission {
        SearchSubmission::Deduplicated { receipt } => Ok((ResponseOutcome::Accepted, receipt)),
        SearchSubmission::New {
            receipt,
            operation_id,
            matcher,
            params,
        } => {
            // 后台扫描：物化命中集 + 终态落库。受理事务已持久化，客户端
            // 退出不取消（合同 cancellation 语义）。
            tokio::task::spawn_blocking(move || {
                let _ = execute_search_scan(&store, &operation_id, &params, &matcher);
            });
            Ok((ResponseOutcome::Accepted, receipt))
        }
    }
}

impl V1Store {
    /// `search.start` 的受理事务：幂等闸（重放/冲突）→ 确定性拒绝登记
    /// （空 query / 非法 regex——「接纳前拒绝」）→ 惰性清理超期快照 →
    /// collections 缝 `operations` 行（queued，可取消）+ `search_snapshots`
    /// 行 + 幂等回执，单事务原子提交。
    fn submit_search(
        &self,
        input: &Value,
        idempotency_key: Option<&str>,
    ) -> Result<SearchSubmission, ContractError> {
        let digest = compute_input_digest("search.start", input);
        let now = now_rfc3339();
        let retained_until =
            (chrono::Utc::now() + chrono::Duration::days(SNAPSHOT_RETENTION_DAYS)).to_rfc3339();

        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(sql_err)?;

        // 1. 幂等闸（run.submit / replacement.commit 同序：先于一切校验）。
        if let Some(key) = idempotency_key {
            let existing: Option<(String, String, Option<String>, Option<String>)> = tx
                .query_row(
                    "SELECT input_digest, request_state, receipt_json, rejection_code
                     FROM idempotency_records
                     WHERE idempotency_key = ?1 AND method_id = 'search.start'",
                    params![key],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()
                .map_err(sql_err)?;
            if let Some((stored_digest, req_state, receipt_json, rejection_code)) = existing {
                if stored_digest != digest {
                    return Err(ContractError::new(
                        CODE_IDEMPOTENCY_CONFLICT,
                        format!(
                            "idempotencyKey `{key}` already used for search.start with different input"
                        ),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
                if req_state == "accepted"
                    && let Some(raw) = receipt_json
                    && let Ok(mut receipt) = serde_json::from_str::<Value>(&raw)
                {
                    if let Some(obj) = receipt.as_object_mut() {
                        obj.insert("deduplicated".into(), json!(true));
                    }
                    return Ok(SearchSubmission::Deduplicated { receipt });
                }
                return Err(ContractError::new(
                    rejection_code.unwrap_or_else(|| CODE_INVALID_ARGUMENT.to_owned()),
                    "request previously rejected definitively; fix the input and use a new key"
                        .to_owned(),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
        }

        // 2. 接纳前校验（确定性拒绝登记，同 key 重放同一拒绝）。
        let reject = |tx: &Connection, code: &str, message: String| -> ContractError {
            if let Some(key) = idempotency_key {
                let _ = tx.execute(
                    "INSERT OR IGNORE INTO idempotency_records
                     (idempotency_key, method_id, input_digest, request_state, reason, resource_json,
                      receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
                     VALUES (?1, 'search.start', ?2, 'notAccepted', 'definitivelyRejected', NULL, NULL,
                             ?3, ?4, ?5, 'correctRejectedInputWithNewKey')",
                    params![key, digest, code, now, retained_until],
                );
            }
            ContractError::new(
                code.to_owned(),
                message,
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        };
        let query = input["query"].as_str().unwrap_or_default().to_owned();
        if query.is_empty() {
            let error = reject(
                &tx,
                CODE_INVALID_ARGUMENT,
                "search query is required".to_owned(),
            );
            let _ = tx.commit();
            return Err(error);
        }
        let syntax = input
            .get("syntax")
            .and_then(Value::as_str)
            .unwrap_or("literal");
        let regex_syntax = syntax == "regex";
        let case_sensitive = input
            .get("caseSensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let matcher = match SearchMatcher::compile(&query, regex_syntax, case_sensitive) {
            Ok(matcher) => matcher,
            Err(detail) => {
                let error = reject(
                    &tx,
                    CODE_INVALID_PATTERN,
                    format!("invalid search pattern `{query}`: {detail}"),
                );
                let _ = tx.commit();
                return Err(error);
            }
        };
        let roles: Vec<String> = input
            .get("roles")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|role| role.as_str().map(ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let hit_cap = input
            .get("pageLimit")
            .and_then(Value::as_u64)
            .unwrap_or(100)
            .clamp(1, MAX_HITS as u64) as usize;
        let params = SearchParams {
            query,
            roles,
            scope: SearchScope::parse(input),
            hit_cap,
        };

        // 3. 惰性清理超期快照（终态且 updated_at 超过保留期）。
        let cutoff =
            (chrono::Utc::now() - chrono::Duration::days(SNAPSHOT_RETENTION_DAYS)).to_rfc3339();
        let stale: Vec<String> = {
            let mut stmt = tx
                .prepare(
                    "SELECT operation_id FROM operations
                     WHERE operation_type = 'search.start' AND terminal = 1 AND updated_at < ?1",
                )
                .map_err(sql_err)?;
            let ids = stmt
                .query_map(params![cutoff], |r| r.get::<_, String>(0))
                .map_err(sql_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_err)?;
            drop(stmt);
            ids
        };
        for operation_id in &stale {
            tx.execute(
                "DELETE FROM search_hits WHERE operation_id = ?1",
                params![operation_id],
            )
            .map_err(sql_err)?;
            tx.execute(
                "DELETE FROM search_snapshots WHERE operation_id = ?1",
                params![operation_id],
            )
            .map_err(sql_err)?;
        }

        // 4. 缝行（collections seam 的 `operations` 表，queued + 可取消）。
        let operation_id = self.next_id();
        let rev = bump_rev_in_tx(&tx)?;
        let (scope_kind, scope_project, scope_session) = params.scope.seam_columns();
        tx.execute(
            "INSERT INTO operations
             (operation_id, operation_type, scope_kind, project_id, session_id, state, revision,
              cancel_requested, cancellable, effect_status, progress_json, created_at, updated_at)
             VALUES (?1, 'search.start', ?2, ?3, ?4, 'queued', ?5, 0, 1, 'known', '[]', ?6, ?6)",
            params![
                operation_id,
                scope_kind,
                scope_project,
                scope_session,
                rev,
                now
            ],
        )
        .map_err(sql_err)?;
        append_search_operation_event(
            &tx,
            &operation_id,
            scope_session.as_deref().unwrap_or(""),
            "operation.queued",
            &json!({
                "operationId": operation_id,
                "operationType": "search.start",
                "query": params.query,
            }),
            &now,
        )?;
        tx.execute(
            "INSERT INTO search_snapshots (operation_id, created_at) VALUES (?1, ?2)",
            params![operation_id, now],
        )
        .map_err(sql_err)?;

        // 5. 幂等回执（operation receipt）。
        let receipt = json!({
            "kind": "operation",
            "operationId": operation_id,
            "durable": true,
            "acceptedRevision": rev,
            "deduplicated": false,
            "state": "queued",
        });
        if let Some(key) = idempotency_key {
            tx.execute(
                "INSERT INTO idempotency_records
                 (idempotency_key, method_id, input_digest, request_state, reason, resource_json,
                  receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
                 VALUES (?1, 'search.start', ?2, 'accepted', 'receiptFound', ?3, ?4, NULL, ?5, ?6,
                         'doNotReplay')",
                params![
                    key,
                    digest,
                    serde_json::to_string(
                        &json!({"kind": "operation", "operationId": operation_id})
                    )
                    .unwrap_or_default(),
                    serde_json::to_string(&receipt).unwrap_or_default(),
                    now,
                    retained_until,
                ],
            )
            .map_err(sql_err)?;
        }
        tx.commit().map_err(sql_err)?;
        self.event_notifier().notify_waiters();

        Ok(SearchSubmission::New {
            receipt,
            operation_id,
            matcher,
            params,
        })
    }
}

// ---------------------------------------------------------------------------
// The background scan
// ---------------------------------------------------------------------------

/// 一次块扫描的原始行。
type ScanRow = (
    String, // session_id
    String, // entry_id
    String, // role
    String, // content_json
    String, // created_at
    i64,    // seq
    String, // project_id
    String, // session revision (historyRevision)
);

/// 执行受理后的搜索扫描：分块读取（每块短暂持锁）、块间响应取消、
/// 命中物化 + 终态与 completionResult 原子落库。扫描期故障把操作落为
/// failed（`INDEX_UNAVAILABLE` / `RESOURCE_LIMIT`），不以静默或空成功
/// 伪装（「失败不返回空成功」）。
fn execute_search_scan(
    store: &V1Store,
    operation_id: &str,
    params: &SearchParams,
    matcher: &SearchMatcher,
) -> Result<(), ContractError> {
    store.mark_operation_running(operation_id)?;
    // 覆盖声明锚点：T0 前已存在且落在扫描范围内的行全部覆盖；扫描期间
    // 新提交且排序靠前的行不保证覆盖（报告登记的边界）。
    let scan_started_at = now_rfc3339();
    let fail = |code: &str, message: String| -> Result<(), ContractError> {
        store.finish_long_operation(
            operation_id,
            LongOperationOutcome::Failed {
                code: code.to_owned(),
                message,
            },
        )
    };

    let role_sql = if params.roles.is_empty() {
        String::new()
    } else {
        let placeholders: Vec<&str> = params.roles.iter().map(|_| "?").collect();
        format!(" AND he.role IN ({})", placeholders.join(","))
    };
    let (scope_sql, scope_params): (String, Vec<String>) = match &params.scope {
        SearchScope::Instance => (String::new(), Vec::new()),
        SearchScope::Project { project_id } => {
            (" AND s.project_id = ?".to_owned(), vec![project_id.clone()])
        }
        SearchScope::Session {
            project_id,
            session_id,
        } => (
            " AND s.project_id = ? AND he.session_id = ?".to_owned(),
            vec![project_id.clone(), session_id.clone()],
        ),
    };
    let base_sql = format!(
        "SELECT he.session_id, he.entry_id, he.role, he.content_json, he.created_at, he.seq,
                s.project_id, s.revision
         FROM history_entries he
         JOIN sessions s ON s.session_id = he.session_id
         WHERE s.session_state != 'deleted'{role_sql}{scope_sql}
           AND (he.created_at, he.session_id, he.seq) > (?, ?, ?)
         ORDER BY he.created_at, he.session_id, he.seq
         LIMIT {SCAN_CHUNK}"
    );

    let mut hits: Vec<ScannedHit> = Vec::new();
    let mut scanned: usize = 0;
    let mut watermark: Option<String> = None;
    let (mut last_created_at, mut last_session, mut last_seq) =
        (String::new(), String::new(), 0i64);
    let mut truncated = false;

    'scan: loop {
        if store.operation_cancel_requested(operation_id)? {
            return store.finish_long_operation(operation_id, LongOperationOutcome::Cancelled);
        }
        // 一块读取：guard 在块内短暂持有；错误以值传出块外再落库（在
        // 持锁路径里调 finish_long_operation 会再次抢同一把 store 锁）。
        let chunk: Result<Vec<ScanRow>, String> = {
            let conn = store.conn.lock().expect("sqlite mutex");
            (|| -> Result<Vec<ScanRow>, String> {
                let mut stmt = conn
                    .prepare(&base_sql)
                    .map_err(|e| format!("sqlite prepare failed: {e}"))?;
                let mut query_params: Vec<&dyn rusqlite::ToSql> = Vec::new();
                for role in &params.roles {
                    query_params.push(role);
                }
                for value in &scope_params {
                    query_params.push(value);
                }
                query_params.push(&last_created_at);
                query_params.push(&last_session);
                query_params.push(&last_seq);
                let mapped = stmt
                    .query_map(query_params.as_slice(), |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                            r.get(7)?,
                        ))
                    })
                    .map_err(|e| format!("sqlite scan query failed: {e}"))?;
                mapped
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("sqlite scan read failed: {e}"))
            })()
        };
        let rows = match chunk {
            Ok(rows) => rows,
            Err(detail) => {
                return fail(CODE_INDEX_UNAVAILABLE, detail);
            }
        };
        if rows.is_empty() {
            break;
        }
        let chunk_len = rows.len();
        for (session_id, entry_id, role, content_json, created_at, seq, project_id, revision) in
            rows
        {
            scanned += 1;
            watermark = Some(created_at.clone());
            last_created_at = created_at;
            last_session = session_id.clone();
            last_seq = seq;
            let text = searchable_text(&content_json);
            let found = match matcher.find(&text) {
                Ok(found) => found,
                Err(detail) => {
                    return fail(
                        CODE_RESOURCE_LIMIT,
                        format!("search regex exceeded its execution budget: {detail}"),
                    );
                }
            };
            if let Some((start, end)) = found {
                hits.push(ScannedHit {
                    project_id,
                    session_id,
                    entry_id,
                    role,
                    excerpt: excerpt_around(&text, start, end),
                    history_revision: revision,
                });
                if hits.len() >= params.hit_cap {
                    truncated = true;
                    break 'scan;
                }
            }
            if scanned >= MAX_SCAN_ENTRIES {
                truncated = true;
                break 'scan;
            }
        }
        if chunk_len < SCAN_CHUNK as usize {
            break;
        }
    }

    let (indexing_complete, searched_through) = if truncated {
        (false, watermark)
    } else {
        (true, Some(scan_started_at))
    };
    store.publish_search_results(
        operation_id,
        &hits,
        indexing_complete,
        searched_through.as_deref(),
    )
}

impl V1Store {
    /// 扫描终态落库：命中物化 + 缝行 terminal/succeeded + completionResult
    /// + `operation.completed` 事件，单事务原子提交。与取消竞态时（缝行
    ///   已终态）不改任何状态（幂等护栏）。
    fn publish_search_results(
        &self,
        operation_id: &str,
        hits: &[ScannedHit],
        indexing_complete: bool,
        searched_through: Option<&str>,
    ) -> Result<(), ContractError> {
        let now = now_rfc3339();
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn.transaction().map_err(sql_err)?;
        let terminal: Option<i64> = tx
            .query_row(
                "SELECT terminal FROM operations WHERE operation_id = ?1",
                params![operation_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        // 未知行（受理事务不可能缺行，防御）或已终态（取消竞态）：不动。
        if terminal != Some(0) {
            tx.commit().map_err(sql_err)?;
            return Ok(());
        }
        let mut position: i64 = 0;
        for hit in hits {
            position += 1;
            tx.execute(
                "INSERT INTO search_hits
                 (operation_id, position, project_id, session_id, entry_id, role, excerpt, history_revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    operation_id,
                    position,
                    hit.project_id,
                    hit.session_id,
                    hit.entry_id,
                    hit.role,
                    hit.excerpt,
                    hit.history_revision,
                ],
            )
            .map_err(sql_err)?;
        }
        let completion = json!({
            "searchId": operation_id,
            "indexingComplete": indexing_complete,
            "searchedThrough": searched_through,
            "resultCount": hits.len(),
        });
        let rev = bump_rev_in_tx(&tx)?;
        let cursor = append_search_operation_event(
            &tx,
            operation_id,
            "",
            "operation.completed",
            &json!({
                "operationId": operation_id,
                "state": "succeeded",
                "resultCount": hits.len(),
                "indexingComplete": indexing_complete,
            }),
            &now,
        )?;
        tx.execute(
            "UPDATE operations
             SET state = 'succeeded', revision = ?1, result_json = ?2, final_cursor = ?3,
                 terminal = 1, updated_at = ?4
             WHERE operation_id = ?5 AND terminal = 0",
            params![
                rev,
                serde_json::to_string(&completion).unwrap_or_default(),
                cursor,
                now,
                operation_id
            ],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        self.event_notifier().notify_waiters();
        self.state_notifier().notify_waiters();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// search.results (sync, readOnly)
// ---------------------------------------------------------------------------

async fn search_results(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    Ok((
        ResponseOutcome::Succeeded,
        rt.store().read_search_results(&input)?,
    ))
}

impl V1Store {
    /// `search.results`：终态搜索的分页读取（可见性过滤 + 快照钉扎）。
    fn read_search_results(&self, input: &Value) -> Result<Value, ContractError> {
        let operation_id = input["operationId"].as_str().unwrap_or_default();
        let obj = input.get("page").and_then(Value::as_object);
        let limit = obj
            .and_then(|p| p.get("limit"))
            .and_then(Value::as_u64)
            .unwrap_or(100)
            .clamp(1, 500) as usize;

        let conn = self.conn.lock().expect("sqlite mutex");
        #[allow(clippy::type_complexity)]
        let row: Option<(String, bool, String, Option<String>, Option<String>)> = conn
            .query_row(
                "SELECT operation_type, terminal, state, result_json, error_json
                 FROM operations WHERE operation_id = ?1",
                params![operation_id],
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
        let Some((operation_type, terminal, state, result_json, error_json)) = row else {
            return Err(not_found("search", operation_id));
        };
        if operation_type != "search.start" {
            return Err(not_found("search", operation_id));
        }
        if !terminal {
            return Err(operation_not_ready(
                operation_id,
                &format!("state is `{state}`"),
            ));
        }
        if state != "succeeded" {
            let recorded = error_json
                .as_deref()
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                .and_then(|issue| issue["code"].as_str().map(ToOwned::to_owned))
                .unwrap_or(state.clone());
            return Err(operation_not_ready(
                operation_id,
                &format!(
                    "search terminated `{state}` (recorded failure: {recorded}); see operations.get"
                ),
            ));
        }
        // 快照保留期已过（受理时的惰性清理会同时删掉两行）。
        let snapshot: Option<String> = conn
            .query_row(
                "SELECT operation_id FROM search_snapshots WHERE operation_id = ?1",
                params![operation_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if snapshot.is_none() {
            return Err(ContractError::new(
                CODE_CURSOR_EXPIRED,
                format!(
                    "search snapshot for operation `{operation_id}` is no longer retained; start a new search"
                ),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        let completion: Value = result_json
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok())
            .unwrap_or(Value::Null);
        let revision: String = conn
            .query_row(
                "SELECT revision FROM operations WHERE operation_id = ?1",
                params![operation_id],
                |r| r.get(0),
            )
            .map_err(sql_err)?;

        // 可见命中集：命中条目仍存在且其会话未删除（已删除不泄漏）。
        let visible_join = "FROM search_hits sh
             JOIN history_entries he ON he.session_id = sh.session_id AND he.entry_id = sh.entry_id
             JOIN sessions s ON s.session_id = he.session_id AND s.session_state != 'deleted'
             WHERE sh.operation_id = ?1";
        let visible: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) {visible_join}"),
                params![operation_id],
                |r| r.get(0),
            )
            .map_err(sql_err)?;

        // 游标：钉住可见命中数（快照变化不静默混页）。
        let offset = match obj.and_then(|p| p.get("cursor")).and_then(Value::as_str) {
            Some(cursor) => {
                let (pinned, off) = cursor
                    .strip_prefix("srch-")
                    .and_then(|rest| rest.rsplit_once('-'))
                    .and_then(|(n, off)| off.parse::<usize>().ok().map(|off| (n.to_owned(), off)))
                    .ok_or_else(|| {
                        invalid_argument(format!("invalid search results cursor `{cursor}`"))
                    })?;
                if pinned.parse::<i64>().ok() != Some(visible) {
                    return Err(ContractError::new(
                        CODE_SNAPSHOT_CHANGED,
                        format!(
                            "the visible result set changed (was {pinned} hits, now {visible}); restart pagination"
                        ),
                        RetrySpec::after_reconcile(),
                        EffectStatus::NotApplied,
                    ));
                }
                off
            }
            None => 0,
        };

        let fetch = (limit + 1) as i64;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT sh.project_id, sh.session_id, sh.entry_id, sh.role, sh.excerpt, sh.history_revision
                 {visible_join} ORDER BY sh.position LIMIT ?2 OFFSET ?3"
            ))
            .map_err(sql_err)?;
        let rows: Vec<(String, String, String, String, String, String)> = stmt
            .query_map(params![operation_id, fetch, offset as i64], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .map_err(sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err)?;
        drop(stmt);
        let complete = rows.len() <= limit;
        let items: Vec<Value> = rows
            .into_iter()
            .take(limit)
            .map(
                |(project_id, session_id, entry_id, role, excerpt, history_revision)| {
                    json!({
                        "projectId": project_id,
                        "sessionId": session_id,
                        "entryId": entry_id,
                        "role": role,
                        "excerpt": excerpt,
                        "historyRevision": history_revision,
                    })
                },
            )
            .collect();
        let next_cursor = if complete {
            Value::Null
        } else {
            json!(format!("srch-{visible}-{}", offset + limit))
        };

        Ok(json!({
            "items": items,
            "pageInfo": {
                "snapshotRevision": revision,
                "nextCursor": next_cursor,
                "complete": complete,
            },
            "indexingComplete": completion["indexingComplete"].as_bool().unwrap_or(true),
            "searchedThrough": completion.get("searchedThrough").cloned().unwrap_or(Value::Null),
        }))
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

    async fn start_search(
        rt: &std::sync::Arc<PublicV1Runtime>,
        iid: &str,
        scope: Value,
        query: &str,
        key: &str,
    ) -> Result<(ResponseOutcome, Value), crate::public_v1::runtime::DispatchError> {
        rt.dispatch(
            "search.start",
            json!({
                "instanceId": iid,
                "scope": scope,
                "query": query,
            }),
            Some(key),
            None,
        )
        .await
    }

    /// 等待后台扫描终态（有界轮询）。
    async fn await_terminal(
        rt: &std::sync::Arc<PublicV1Runtime>,
        iid: &str,
        operation_id: &str,
    ) -> Value {
        for _ in 0..200 {
            let (_, op) = rt
                .dispatch(
                    "operations.get",
                    json!({"instanceId": iid, "operationId": operation_id}),
                    None,
                    None,
                )
                .await
                .unwrap();
            let state = op["operation"]["state"].as_str().unwrap_or_default();
            if matches!(
                state,
                "succeeded" | "failed" | "cancelled" | "interrupted" | "result_unknown"
            ) {
                return op["operation"].clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("search operation did not terminate within the test window");
    }

    async fn results_page(
        rt: &std::sync::Arc<PublicV1Runtime>,
        iid: &str,
        operation_id: &str,
        page: Value,
    ) -> Result<Value, String> {
        match rt
            .dispatch(
                "search.results",
                json!({
                    "instanceId": iid,
                    "operationId": operation_id,
                    "page": page,
                }),
                None,
                None,
            )
            .await
        {
            Ok((_, value)) => Ok(value),
            Err(error) => Err(contract_code(&error)),
        }
    }

    #[tokio::test]
    async fn literal_and_regex_search_hit_pagination_and_scope() {
        let tmp = TempDir::new("srch-hits");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "s1").await;
        rt.store()
            .append_history_entries(
                &sid,
                &[
                    json!({"turnId": "t-1", "role": "user", "content": [{"kind": "text", "text": "张三的 NEEdle 问题"}]}),
                    json!({"turnId": "t-1", "role": "assistant", "content": [{"kind": "text", "text": "first answer"}]}),
                    json!({"turnId": "t-2", "role": "user", "content": [{"kind": "text", "text": "second question about needle"}]}),
                ],
            )
            .unwrap();

        // 大小写不敏感 literal 命中两条（中文+大小写混合）。
        let (outcome, receipt) = start_search(
            &rt,
            &iid,
            json!({"kind": "session", "projectId": "p-s1", "sessionId": &sid}),
            "needle",
            "key-s1",
        )
        .await
        .unwrap();
        assert_eq!(outcome, ResponseOutcome::Accepted);
        OperationRegistry::global()
            .validate_result("search.start", &receipt)
            .expect("receipt matches the contract result schema");
        assert_eq!(receipt["kind"], "operation");
        let op_id = receipt["operationId"].as_str().unwrap().to_owned();
        let op = await_terminal(&rt, &iid, &op_id).await;
        assert_eq!(op["state"], "succeeded");
        assert_eq!(op["result"]["resultCount"], 2);
        assert_eq!(op["result"]["indexingComplete"], true);
        assert_eq!(op["result"]["searchId"], op_id.as_str());

        // 分页：limit 1 两页 + complete。
        let page1 = results_page(&rt, &iid, &op_id, json!({"limit": 1}))
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("search.results", &page1)
            .expect("results page matches the contract result schema");
        assert_eq!(page1["indexingComplete"], true);
        assert_eq!(page1["items"].as_array().unwrap().len(), 1);
        assert_eq!(page1["pageInfo"]["complete"], false);
        let cursor = page1["pageInfo"]["nextCursor"].as_str().unwrap().to_owned();
        let page2 = results_page(&rt, &iid, &op_id, json!({"limit": 5, "cursor": cursor}))
            .await
            .unwrap();
        assert_eq!(page2["items"].as_array().unwrap().len(), 1);
        assert_eq!(page2["pageInfo"]["complete"], true);
        assert_eq!(page2["pageInfo"]["nextCursor"], Value::Null);
        // entryId 可回查（history.read 锚点）。
        let hit = page2["items"][0].clone();
        assert!(!hit["entryId"].as_str().unwrap().is_empty());
        let (_, history) = rt
            .dispatch(
                "history.read",
                json!({"instanceId": iid, "sessionId": sid, "page": {"limit": 500}}),
                None,
                None,
            )
            .await
            .unwrap();
        let ids: Vec<&str> = history["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["entryId"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&hit["entryId"].as_str().unwrap()));

        // 大小写敏感 literal：仅 "NEEdle" 一条。
        let (outcome, receipt) = rt
            .dispatch(
                "search.start",
                json!({
                    "instanceId": iid,
                    "scope": {"kind": "session", "projectId": "p-s1", "sessionId": &sid},
                    "query": "NEEdle",
                    "caseSensitive": true,
                }),
                Some("key-s2"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome, ResponseOutcome::Accepted);
        let op2 = await_terminal(&rt, &iid, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(op2["result"]["resultCount"], 1);

        // regex + 角色过滤。
        let (_, receipt) = rt
            .dispatch(
                "search.start",
                json!({
                    "instanceId": iid,
                    "scope": {"kind": "instance"},
                    "query": "n[eE]+dle",
                    "syntax": "regex",
                    "roles": ["user"],
                }),
                Some("key-s3"),
                None,
            )
            .await
            .unwrap();
        let op3 = await_terminal(&rt, &iid, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(op3["result"]["resultCount"], 2);
        let page = results_page(
            &rt,
            &iid,
            receipt["operationId"].as_str().unwrap(),
            json!({}),
        )
        .await
        .unwrap();
        for item in page["items"].as_array().unwrap() {
            assert_eq!(item["role"], "user");
        }

        // 范围外：别的项目 scope 零命中（受理即成功，resultCount=0）。
        let (_, receipt) = rt
            .dispatch(
                "search.start",
                json!({
                    "instanceId": iid,
                    "scope": {"kind": "project", "projectId": "p-other"},
                    "query": "needle",
                }),
                Some("key-s4"),
                None,
            )
            .await
            .unwrap();
        let op4 = await_terminal(&rt, &iid, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(op4["result"]["resultCount"], 0);
        let page = results_page(
            &rt,
            &iid,
            receipt["operationId"].as_str().unwrap(),
            json!({}),
        )
        .await
        .unwrap();
        assert_eq!(page["items"].as_array().unwrap().len(), 0);
        assert_eq!(page["pageInfo"]["complete"], true);
    }

    #[tokio::test]
    async fn search_idempotency_and_preacceptance_rejections() {
        let tmp = TempDir::new("srch-idem");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "s2").await;
        rt.store()
            .append_history_entries(
                &sid,
                &[json!({"turnId": "t-1", "role": "user", "content": [{"kind": "text", "text": "alpha needle"}]})],
            )
            .unwrap();

        // 非法 regex：接纳前拒绝 INVALID_PATTERN，且同 key 重放同一拒绝。
        let err = rt
            .dispatch(
                "search.start",
                json!({"instanceId": iid, "scope": {"kind": "instance"}, "query": "[unclosed", "syntax": "regex"}),
                Some("key-bad"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "INVALID_PATTERN");
        let err = rt
            .dispatch(
                "search.start",
                json!({"instanceId": iid, "scope": {"kind": "instance"}, "query": "[unclosed", "syntax": "regex"}),
                Some("key-bad"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "INVALID_PATTERN");

        // 受理 + 同 key 同输入去重（回放原回执）。
        let input = json!({
            "instanceId": iid,
            "scope": {"kind": "instance"},
            "query": "needle",
        });
        let (_, first) = rt
            .dispatch("search.start", input.clone(), Some("key-ok"), None)
            .await
            .unwrap();
        let op_id = first["operationId"].as_str().unwrap().to_owned();
        let op = await_terminal(&rt, &iid, &op_id).await;
        assert_eq!(op["result"]["resultCount"], 1);
        let (_, replay) = rt
            .dispatch("search.start", input.clone(), Some("key-ok"), None)
            .await
            .unwrap();
        assert_eq!(replay["deduplicated"], true);
        assert_eq!(replay["operationId"].as_str().unwrap(), op_id);

        // requests.lookup 同 methodId/key 返回原资源。
        let (_, lookup) = rt
            .dispatch(
                "requests.lookup",
                json!({"instanceId": iid, "methodId": "search.start", "idempotencyKey": "key-ok"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(lookup["requestState"], "accepted");
        assert_eq!(lookup["resource"]["operationId"].as_str().unwrap(), op_id);

        // 同 key 不同语义输入：IDEMPOTENCY_CONFLICT。
        let err = rt
            .dispatch(
                "search.start",
                json!({"instanceId": iid, "scope": {"kind": "instance"}, "query": "other"}),
                Some("key-ok"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "IDEMPOTENCY_CONFLICT");

        // 空 query：INVALID_ARGUMENT（同 key 重放同一拒绝）。
        let err = rt
            .dispatch(
                "search.start",
                json!({"instanceId": iid, "scope": {"kind": "instance"}, "query": ""}),
                Some("key-empty"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "INVALID_ARGUMENT");

        // schema 拒绝客户端传入新资源 ID（additionalProperties:false）。
        let err = rt
            .dispatch(
                "search.start",
                json!({"instanceId": iid, "scope": {"kind": "instance"}, "query": "x", "runId": "r-1"}),
                Some("key-extra"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&err), "INVALID_ARGUMENT");
    }

    #[tokio::test]
    async fn search_results_gates_and_snapshot_changes() {
        let tmp = TempDir::new("srch-gates");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "s3").await;
        rt.store()
            .append_history_entries(
                &sid,
                &[
                    json!({"turnId": "t-1", "role": "user", "content": [{"kind": "text", "text": "needle one"}]}),
                    json!({"turnId": "t-2", "role": "user", "content": [{"kind": "text", "text": "needle two"}]}),
                ],
            )
            .unwrap();

        // 未知 operationId：NOT_FOUND；非搜索 operationId：NOT_FOUND。
        let err = results_page(&rt, &iid, "op-missing", json!({}))
            .await
            .err()
            .unwrap();
        assert_eq!(err, "NOT_FOUND");
        let (foreign_id, _) = rt
            .store()
            .begin_long_operation("instance.maintenance", &json!({"kind": "instance"}), true)
            .unwrap();
        let err = results_page(&rt, &iid, &foreign_id, json!({}))
            .await
            .err()
            .unwrap();
        assert_eq!(err, "NOT_FOUND");

        let (_, receipt) = start_search(&rt, &iid, json!({"kind": "instance"}), "needle", "key-g1")
            .await
            .unwrap();
        let op_id = receipt["operationId"].as_str().unwrap().to_owned();
        let op = await_terminal(&rt, &iid, &op_id).await;
        assert_eq!(op["result"]["resultCount"], 2);

        // 坏游标：INVALID_ARGUMENT。
        let err = results_page(&rt, &iid, &op_id, json!({"cursor": "bogus"}))
            .await
            .err()
            .unwrap();
        assert_eq!(err, "INVALID_ARGUMENT");

        // 页间删除命中条目：可见集变化 → SNAPSHOT_CHANGED。
        let page1 = results_page(&rt, &iid, &op_id, json!({"limit": 1}))
            .await
            .unwrap();
        let cursor = page1["pageInfo"]["nextCursor"].as_str().unwrap().to_owned();
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "DELETE FROM history_entries WHERE session_id = ?1 AND turn_id = 't-2'",
                params![sid],
            )
            .unwrap();
        }
        let err = results_page(&rt, &iid, &op_id, json!({"cursor": cursor}))
            .await
            .err()
            .unwrap();
        assert_eq!(err, "SNAPSHOT_CHANGED");
        // 重启分页可见新视图（只剩一条命中，不泄漏已删除条目）。
        let fresh = results_page(&rt, &iid, &op_id, json!({})).await.unwrap();
        assert_eq!(fresh["items"].as_array().unwrap().len(), 1);

        // 已删除会话的命中不泄漏：删除会话 → 可见集归零。
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "UPDATE sessions SET session_state = 'deleted' WHERE session_id = ?1",
                params![sid],
            )
            .unwrap();
        }
        let empty = results_page(&rt, &iid, &op_id, json!({})).await.unwrap();
        assert_eq!(empty["items"].as_array().unwrap().len(), 0);
        assert_eq!(empty["pageInfo"]["complete"], true);

        // 快照清理后：CURSOR_EXPIRED。
        {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            conn.execute(
                "UPDATE operations SET updated_at = ?1 WHERE operation_id = ?2",
                params!["2020-01-01T00:00:00.000000+00:00", op_id],
            )
            .unwrap();
        }
        let (_, receipt) = start_search(&rt, &iid, json!({"kind": "instance"}), "x", "key-g2")
            .await
            .unwrap();
        let _ = await_terminal(&rt, &iid, receipt["operationId"].as_str().unwrap()).await;
        let err = results_page(&rt, &iid, &op_id, json!({}))
            .await
            .err()
            .unwrap();
        assert_eq!(err, "CURSOR_EXPIRED");
    }

    #[tokio::test]
    async fn search_page_limit_truncates_with_watermark() {
        let tmp = TempDir::new("srch-cap");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        let sid = seeded(&rt, &iid, "s4").await;
        let mut entries = Vec::new();
        for n in 0..5 {
            entries.push(json!({
                "turnId": format!("t-{n}"),
                "role": "user",
                "content": [{"kind": "text", "text": format!("needle entry {n}")}],
                "createdAt": format!("2026-11-18T00:00:{n:02}.000000+00:00"),
            }));
        }
        rt.store().append_history_entries(&sid, &entries).unwrap();

        // pageLimit=2：截断，indexingComplete=false + 水位。
        let (_, receipt) = rt
            .dispatch(
                "search.start",
                json!({
                    "instanceId": iid,
                    "scope": {"kind": "instance"},
                    "query": "needle",
                    "pageLimit": 2,
                }),
                Some("key-cap"),
                None,
            )
            .await
            .unwrap();
        let op_id = receipt["operationId"].as_str().unwrap().to_owned();
        let op = await_terminal(&rt, &iid, &op_id).await;
        assert_eq!(op["result"]["resultCount"], 2);
        assert_eq!(op["result"]["indexingComplete"], false);
        assert_eq!(
            op["result"]["searchedThrough"].as_str().unwrap(),
            "2026-11-18T00:00:01.000000+00:00",
            "watermark is the created_at of the last scanned entry"
        );
        let page = results_page(&rt, &iid, &op_id, json!({})).await.unwrap();
        assert_eq!(page["indexingComplete"], false);
        assert_eq!(page["items"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn literal_matcher_escapes_regex_metacharacters() {
        // literal 语法下正则元字符按字面匹配。
        let matcher = SearchMatcher::compile("a.b*c", false, false).unwrap();
        assert!(matcher.find("xa.b*cy").unwrap().is_some());
        assert!(matcher.find("aXb*c").unwrap().is_none());
        let matcher = SearchMatcher::compile("[all]", false, false).unwrap();
        assert!(matcher.find("contains [all] literally").unwrap().is_some());
        // 非法 regex：编译失败（受理前 INVALID_PATTERN 的来源）。
        let error = SearchMatcher::compile("[unclosed", true, false).unwrap_err();
        assert!(!error.is_empty());
    }
}
