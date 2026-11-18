//! `usage.*` — scoped usage snapshots and aggregation (`cli.task.t016`, S3).
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `usage.snapshot` /
//! `usage.summary` 条目。TS 基线（cc7c4e6）是语义参考：`TokenStatsCollector`
//! （旧 Rust 线 `runtime/model/stats.rs` 的移植正本）把 cost 来源区分为
//! `provider_reported` / `configured_price` / `built_in_estimate` /
//! `fallback_estimate` / `legacy_unknown`，并显式保留未知值——v1 合同把这些
//! 归一为 `Usage {status: actual|estimated|unknown, value, unit, source}`，
//! 验收锚点「漏usage为unknown/null不是0」「未知计数保留」「不同币种单位
//! 不得直接相加」。
//!
//! 数据面：用量记录由**用量产生方**（run 完成、provider_test 等，后续任务）
//! 经 `V1Store::record_usage` 缝合点写入 `v1_usage_records`；本域的两个操作
//! 都是只读（合同 postcondition：无业务状态修改）。S3 当前没有生产写入方，
//! 空实例返回空集是诚实行为；单元测试用同一缝合点造数。
//!
//! 聚合规则（每指标独立）：
//! * 任一记录 `unknown` → 聚合 `unknown` / `null`（未知不当零，也无法给出
//!  诚实的部分和）；
//! * 无 `unknown` 但有 `estimated` → `estimated`，值为已知值之和；
//! * 全 `actual` → `actual`，值为和；
//! * 单位不一致（如混币种）→ `unknown` / `null`，unit 报 `mixed`
//!   （「不同币种单位不得直接相加」；`mixed` 是展示串，非协议常量，
//!   已在任务报告登记为合同缺口下的选择）；
//! * `source` 全一致则报该来源，否则 `mixed`。
//!
//! `groupBy=day` 的行形状（合同 `UsageRow` 无日期字段）按 (day, model) 分桶、
//! 时间升序输出；跨日同 scope+model 会得到多行——已作为合同缺口登记在任务
//! 报告，建议后续在 `UsageRow` 或 `pageInfo` 补日期字段。

use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_UNAVAILABLE, ContractError, EffectStatus, RetrySpec,
};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::V1Store;

const USAGE_RECORDS_TABLE_DDL: &str = "CREATE TABLE IF NOT EXISTS v1_usage_records (
    record_id TEXT PRIMARY KEY,
    occurred_at TEXT NOT NULL,
    project_id TEXT,
    session_id TEXT,
    provider_id TEXT,
    model_id TEXT,
    input_value REAL,
    input_status TEXT NOT NULL,
    input_source TEXT NOT NULL,
    output_value REAL,
    output_status TEXT NOT NULL,
    output_source TEXT NOT NULL,
    cost_value REAL,
    cost_status TEXT NOT NULL,
    cost_source TEXT NOT NULL,
    cost_unit TEXT NOT NULL,
    created_at TEXT NOT NULL
)";
const USAGE_TIME_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS idx_v1_usage_time ON v1_usage_records (occurred_at, record_id)";

/// The token-metric unit this domain reports when a caller omits one (the
/// ingestion seam requires a unit string per the contract `Usage` shape).
const TOKEN_UNIT: &str = "tokens";
/// Unit/source marker for aggregates whose members disagree (display string
/// only — see the module docs for the contract-gap rationale).
const MIXED_MARKER: &str = "mixed";

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema("v1_usage_records", USAGE_RECORDS_TABLE_DDL);
    reg.schema("idx_v1_usage_time", USAGE_TIME_INDEX_DDL);
    reg.handler("usage.snapshot", |rt, input, _key, _rev| {
        Box::pin(usage_snapshot(rt, input))
    });
    reg.handler("usage.summary", |rt, input, _key, _rev| {
        Box::pin(usage_summary(rt, input))
    });
}

// ---------------------------------------------------------------------------
// Scope model (local copy of the per-domain pattern)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum UsageScope {
    Instance,
    Project {
        project_id: String,
    },
    Session {
        project_id: String,
        session_id: String,
    },
}

impl UsageScope {
    fn to_scope_json(&self) -> Value {
        match self {
            Self::Instance => json!({"kind": "instance"}),
            Self::Project { project_id } => json!({"kind": "project", "projectId": project_id}),
            Self::Session {
                project_id,
                session_id,
            } => json!({"kind": "session", "projectId": project_id, "sessionId": session_id}),
        }
    }

    /// The query filter columns: (project_id, session_id) the records must
    /// carry (`None` = unrestricted at that level).
    fn record_filter(&self) -> (Option<String>, Option<String>) {
        match self {
            Self::Instance => (None, None),
            Self::Project { project_id } => (Some(project_id.clone()), None),
            Self::Session {
                project_id,
                session_id,
            } => (Some(project_id.clone()), Some(session_id.clone())),
        }
    }
}

/// Parses and validates the request scope, including the project/session
/// existence checks (INVALID_ARGUMENT for missing targets — the contract's
/// error lists carry no NOT_FOUND; `configuration.rs` precedent).
fn parse_scope(rt: &PublicV1Runtime, input: &Value) -> Result<UsageScope, ContractError> {
    let invalid = |message: String| {
        ContractError::new(
            CODE_INVALID_ARGUMENT,
            message,
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    };
    let Some(scope) = input.get("scope") else {
        return Err(invalid("input.scope is required".to_owned()));
    };
    match scope.get("kind").and_then(Value::as_str) {
        Some("instance") => Ok(UsageScope::Instance),
        Some("project") => {
            let Some(project_id) = scope.get("projectId").and_then(Value::as_str) else {
                return Err(invalid("scope.projectId is required".to_owned()));
            };
            if rt.store().get_project(project_id).is_err() {
                return Err(invalid(format!(
                    "scope project `{project_id}` does not exist"
                )));
            }
            Ok(UsageScope::Project {
                project_id: project_id.to_owned(),
            })
        }
        Some("session") => {
            let (Some(project_id), Some(session_id)) = (
                scope.get("projectId").and_then(Value::as_str),
                scope.get("sessionId").and_then(Value::as_str),
            ) else {
                return Err(invalid(
                    "scope.projectId and scope.sessionId are required".to_owned(),
                ));
            };
            let session = rt
                .store()
                .get_session(session_id)
                .map_err(|_| invalid(format!("scope session `{session_id}` does not exist")))?;
            if session.get("projectId").and_then(Value::as_str) != Some(project_id) {
                return Err(invalid(format!(
                    "session `{session_id}` does not belong to project `{project_id}`"
                )));
            }
            Ok(UsageScope::Session {
                project_id: project_id.to_owned(),
                session_id: session_id.to_owned(),
            })
        }
        other => Err(invalid(format!(
            "scope.kind must be instance, project or session; got `{other:?}`"
        ))),
    }
}

fn usage_sql_err(err: rusqlite::Error) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("sqlite storage error: {err}"),
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

// ---------------------------------------------------------------------------
// Stored records and the ingestion seam
// ---------------------------------------------------------------------------

/// One stored usage record (the columns of `v1_usage_records`).
struct StoredUsageRecord {
    record_id: String,
    occurred_at: String,
    project_id: Option<String>,
    session_id: Option<String>,
    provider_id: Option<String>,
    model_id: Option<String>,
    input: Metric,
    output: Metric,
    cost: Metric,
}

/// One metric triple as stored per record: status, optional value, unit
/// (tokens metrics carry the caller-supplied unit; cost carries its
/// currency), and the reporting source.
struct Metric {
    value: Option<f64>,
    status: String,
    unit: String,
    source: String,
}

/// The record shape the ingestion seam accepts — one run/completion-style
/// event with its scope, model, and the three contract `Usage` metrics.
/// Callers with plain numbers construct this with
/// [`UsageRecordIn::actual_tokens`]. Token metrics report the fixed unit
/// `tokens` (v1 stores plain token counts; the `cost` metric carries its
/// own currency unit, which **is** stored and drives the mixed-currency
/// aggregation rule).
pub(crate) struct UsageRecordIn {
    pub(crate) occurred_at: String,
    pub(crate) scope: UsageScope,
    pub(crate) model: Option<(String, String)>,
    pub(crate) input: Metric,
    pub(crate) output: Metric,
    pub(crate) cost: Metric,
}

impl UsageRecordIn {
    /// Convenience for the common shape: actual token counts with the given
    /// source, and an actual cost in `unit` currency. Unused in the S3 build
    /// outside tests (same writer situation as `record_usage`).
    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn actual_tokens(
        occurred_at: String,
        scope: UsageScope,
        model: Option<(String, String)>,
        input_tokens: f64,
        output_tokens: f64,
        cost: Option<f64>,
        cost_unit: &str,
        source: &str,
    ) -> Self {
        Self {
            occurred_at,
            scope,
            model,
            input: Metric {
                value: Some(input_tokens),
                status: "actual".to_owned(),
                unit: TOKEN_UNIT.to_owned(),
                source: source.to_owned(),
            },
            output: Metric {
                value: Some(output_tokens),
                status: "actual".to_owned(),
                unit: TOKEN_UNIT.to_owned(),
                source: source.to_owned(),
            },
            cost: Metric {
                value: cost,
                status: if cost.is_some() {
                    "actual".to_owned()
                } else {
                    // 「漏usage为unknown/null不是0」: a missing cost is
                    // unknown, never zero.
                    "unknown".to_owned()
                },
                unit: cost_unit.to_owned(),
                source: source.to_owned(),
            },
        }
    }
}

impl V1Store {
    /// The usage ingestion seam: durably records one usage event. Not a
    /// public operation — the writers are run completion, provider tests, and
    /// later S3 tasks; unit tests use it to seed deterministic data (unused
    /// in the S3 build outside tests for that reason — the `access_keys.rs`
    /// precedent for suppressing the warning deliberately).
    #[allow(dead_code)]
    pub(crate) fn record_usage(&self, record: UsageRecordIn) -> Result<String, ContractError> {
        let (project_id, session_id) = match &record.scope {
            UsageScope::Instance => (None, None),
            UsageScope::Project { project_id } => (Some(project_id.clone()), None),
            UsageScope::Session {
                project_id,
                session_id,
            } => (Some(project_id.clone()), Some(session_id.clone())),
        };
        let (provider_id, model_id) = match &record.model {
            Some((provider_id, model_id)) => (Some(provider_id.clone()), Some(model_id.clone())),
            None => (None, None),
        };
        let record_id = self.next_id();
        let conn = self.conn.lock().expect("sqlite mutex");
        conn.execute(
            "INSERT INTO v1_usage_records
                 (record_id, occurred_at, project_id, session_id, provider_id, model_id,
                  input_value, input_status, input_source,
                  output_value, output_status, output_source,
                  cost_value, cost_status, cost_source, cost_unit, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                record_id,
                record.occurred_at,
                project_id,
                session_id,
                provider_id,
                model_id,
                record.input.value,
                record.input.status,
                record.input.source,
                record.output.value,
                record.output.status,
                record.output.source,
                record.cost.value,
                record.cost.status,
                record.cost.source,
                record.cost.unit,
                Utc::now().to_rfc3339(),
            ],
        )
        .map_err(usage_sql_err)?;
        Ok(record_id)
    }

    /// Reads every usage record (ordered by time then id); callers filter.
    pub(crate) fn all_usage_records(&self) -> Result<Vec<StoredUsageRecord>, ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        read_all_usage_records(&conn)
    }
}

fn read_all_usage_records(conn: &Connection) -> Result<Vec<StoredUsageRecord>, ContractError> {
    let mut stmt = conn
        .prepare(
            "SELECT record_id, occurred_at, project_id, session_id, provider_id, model_id,
                    input_value, input_status, input_source,
                    output_value, output_status, output_source,
                    cost_value, cost_status, cost_source, cost_unit
             FROM v1_usage_records ORDER BY occurred_at ASC, record_id ASC",
        )
        .map_err(usage_sql_err)?;
    let records = stmt
        .query_map([], row_to_usage_record)
        .map_err(usage_sql_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(usage_sql_err)?;
    Ok(records)
}

fn row_to_usage_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredUsageRecord> {
    Ok(StoredUsageRecord {
        record_id: row.get(0)?,
        occurred_at: row.get(1)?,
        project_id: row.get(2)?,
        session_id: row.get(3)?,
        provider_id: row.get(4)?,
        model_id: row.get(5)?,
        input: Metric {
            value: row.get(6)?,
            status: row.get(7)?,
            unit: TOKEN_UNIT.to_owned(),
            source: row.get(8)?,
        },
        output: Metric {
            value: row.get(9)?,
            status: row.get(10)?,
            unit: TOKEN_UNIT.to_owned(),
            source: row.get(11)?,
        },
        cost: Metric {
            value: row.get(12)?,
            status: row.get(13)?,
            source: row.get(14)?,
            unit: row.get(15)?,
        },
    })
}

/// The record's natural scope: session when it carries a session, project
/// when only a project, instance otherwise.
fn record_scope(record: &StoredUsageRecord) -> UsageScope {
    match (&record.project_id, &record.session_id) {
        (Some(project_id), Some(session_id)) => UsageScope::Session {
            project_id: project_id.clone(),
            session_id: session_id.clone(),
        },
        (Some(project_id), None) => UsageScope::Project {
            project_id: project_id.clone(),
        },
        _ => UsageScope::Instance,
    }
}

fn record_model_json(record: &StoredUsageRecord) -> Value {
    match (&record.provider_id, &record.model_id) {
        (Some(provider_id), Some(model_id)) => {
            json!({"providerId": provider_id, "modelId": model_id})
        }
        _ => Value::Null,
    }
}

fn metric_to_usage(metric: &Metric) -> Value {
    let value = match (&metric.status[..], metric.value) {
        ("unknown", _) => Value::Null,
        (_, Some(value)) => json!(value),
        // A non-unknown status without a value cannot satisfy the contract's
        // if/then (`status != unknown` requires a number): report unknown.
        (_, None) => Value::Null,
    };
    let status = if value.is_null() && metric.status != "unknown" {
        "unknown"
    } else {
        &metric.status
    };
    json!({
        "status": status,
        "value": value,
        "unit": metric.unit,
        "source": metric.source,
    })
}

/// Aggregates one metric across records (module-doc rules).
fn aggregate_metric<'a>(metrics: impl Iterator<Item = &'a Metric>, default_unit: &str) -> Value {
    let mut any = false;
    let mut any_unknown = false;
    let mut any_estimated = false;
    let mut sum = 0.0;
    let mut units: Vec<&str> = Vec::new();
    let mut sources: Vec<&str> = Vec::new();
    for metric in metrics {
        any = true;
        any_unknown |= metric.status == "unknown";
        any_estimated |= metric.status == "estimated";
        if let Some(value) = metric.value {
            sum += value;
        }
        if !units.contains(&metric.unit.as_str()) {
            units.push(&metric.unit);
        }
        if !sources.contains(&metric.source.as_str()) {
            sources.push(&metric.source);
        }
    }
    if !any {
        return json!({
            "status": "unknown",
            "value": Value::Null,
            "unit": default_unit,
            "source": MIXED_MARKER,
        });
    }
    let mixed_units = units.len() > 1;
    let unit = if mixed_units { MIXED_MARKER } else { units[0] };
    let source = if sources.len() > 1 {
        MIXED_MARKER
    } else {
        sources[0]
    };
    if any_unknown || mixed_units {
        // Unknown members or incommensurable units: the honest aggregate is
        // unknown/null (never a partial sum presented as the total).
        return json!({
            "status": "unknown",
            "value": Value::Null,
            "unit": unit,
            "source": source,
        });
    }
    json!({
        "status": if any_estimated { "estimated" } else { "actual" },
        "value": sum,
        "unit": unit,
        "source": source,
    })
}

// ---------------------------------------------------------------------------
// Query filtering
// ---------------------------------------------------------------------------

struct UsageQuery {
    scope: UsageScope,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    model: Option<(String, String)>,
}

impl UsageQuery {
    fn parse(
        rt: &PublicV1Runtime,
        input: &Value,
    ) -> Result<(Self, Vec<StoredUsageRecord>), ContractError> {
        let scope = parse_scope(rt, input)?;
        let parse_bound = |field: &str| -> Result<Option<DateTime<Utc>>, ContractError> {
            match input.get(field) {
                None | Some(Value::Null) => Ok(None),
                Some(value) => {
                    let raw = value.as_str().unwrap_or_default();
                    DateTime::parse_from_rfc3339(raw)
                        .map(|dt| Some(dt.with_timezone(&Utc)))
                        .map_err(|_| {
                            ContractError::new(
                                CODE_INVALID_ARGUMENT,
                                format!("`{field}` must be an RFC 3339 date-time, got `{raw}`"),
                                RetrySpec::never(),
                                EffectStatus::NotApplied,
                            )
                        })
                }
            }
        };
        let from = parse_bound("from")?;
        let to = parse_bound("to")?;
        let model = match input.get("model") {
            None | Some(Value::Null) => None,
            Some(reference) => {
                let provider_id = reference
                    .get("providerId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let model_id = reference
                    .get("modelId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if provider_id.is_empty() || model_id.is_empty() {
                    return Err(ContractError::new(
                        CODE_INVALID_ARGUMENT,
                        "model.providerId and model.modelId must be non-empty".to_owned(),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    ));
                }
                Some((provider_id.to_owned(), model_id.to_owned()))
            }
        };

        let conn = rt.store().conn.lock().expect("sqlite mutex");
        let all = read_all_usage_records(&conn)?;
        drop(conn);

        let (filter_project, filter_session) = scope.record_filter();
        let query = Self {
            scope,
            from,
            to,
            model,
        };
        let filtered: Vec<StoredUsageRecord> = all
            .into_iter()
            .filter(|record| {
                if let Some(project_id) = &filter_project
                    && record.project_id.as_deref() != Some(project_id.as_str())
                {
                    return false;
                }
                if let Some(session_id) = &filter_session
                    && record.session_id.as_deref() != Some(session_id.as_str())
                {
                    return false;
                }
                if let Some((provider_id, model_id)) = &query.model
                    && (record.provider_id.as_deref() != Some(provider_id.as_str())
                        || record.model_id.as_deref() != Some(model_id.as_str()))
                {
                    return false;
                }
                if let Ok(occurred) = DateTime::parse_from_rfc3339(&record.occurred_at) {
                    let occurred = occurred.with_timezone(&Utc);
                    if let Some(from) = query.from
                        && occurred < from
                    {
                        return false;
                    }
                    if let Some(to) = query.to
                        && occurred > to
                    {
                        return false;
                    }
                }
                true
            })
            .collect();
        Ok((query, filtered))
    }
}

fn usage_current_rev(conn: &Connection) -> Result<String, ContractError> {
    let cur: i64 = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(usage_sql_err)?
        .unwrap_or(1);
    Ok(format!("rev-{cur:08}"))
}

// ---------------------------------------------------------------------------
// usage.snapshot
// ---------------------------------------------------------------------------

async fn usage_snapshot(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let page = input.get("page").and_then(Value::as_object);
    let limit = page
        .and_then(|p| p.get("limit"))
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 500) as usize;
    let offset = match page.and_then(|p| p.get("cursor")).and_then(Value::as_str) {
        Some(cursor) => cursor.parse::<usize>().map_err(|_| {
            ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!("invalid pagination cursor `{cursor}`"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?,
        None => 0,
    };

    let (_query, records) = UsageQuery::parse(&rt, &input)?;
    let conn = rt.store().conn.lock().expect("sqlite mutex");
    let snapshot_revision = usage_current_rev(&conn)?;
    drop(conn);

    let total = records.len();
    if offset > total {
        return Err(ContractError::new(
            CODE_INVALID_ARGUMENT,
            "pagination cursor beyond end of collection".to_owned(),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
        .into());
    }
    let end = offset.saturating_add(limit).min(total);
    let complete = end >= total;
    let items: Vec<Value> = records
        .iter()
        .skip(offset)
        .take(limit)
        .map(|record| {
            json!({
                "scope": record_scope(record).to_scope_json(),
                "model": record_model_json(record),
                "inputTokens": metric_to_usage(&record.input),
                "outputTokens": metric_to_usage(&record.output),
                "cost": metric_to_usage(&record.cost),
            })
        })
        .collect();
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "items": items,
            "pageInfo": {
                "snapshotRevision": snapshot_revision,
                "nextCursor": if complete { Value::Null } else { json!(end.to_string()) },
                "complete": complete,
            },
            "complete": complete,
        }),
    ))
}

// ---------------------------------------------------------------------------
// usage.summary
// ---------------------------------------------------------------------------

async fn usage_summary(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let group_by = input
        .get("groupBy")
        .and_then(Value::as_str)
        .unwrap_or("session");
    if !matches!(group_by, "project" | "session" | "model" | "day") {
        // Defensive: the schema enum gate normally rejects this first.
        return Err(ContractError::new(
            CODE_INVALID_ARGUMENT,
            format!("groupBy must be one of project|session|model|day; got `{group_by}`"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
        .into());
    }

    let (query, records) = UsageQuery::parse(&rt, &input)?;
    let query_scope = query.scope.clone();

    // Group key → (row scope, row model). Insertion order follows the
    // records' (occurred_at, record_id) order; rows are re-sorted below.
    let mut groups: Vec<(
        String,
        UsageScope,
        Option<(String, String)>,
        Vec<&StoredUsageRecord>,
    )> = Vec::new();
    let mut group_index: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut unknown_record_count: i64 = 0;
    for record in &records {
        if record.input.status == "unknown"
            || record.output.status == "unknown"
            || record.cost.status == "unknown"
        {
            unknown_record_count += 1;
        }
        let (key, scope, model) = match group_by {
            "session" => {
                let scope = record_scope(record);
                let key = format!(
                    "scope:{}",
                    serde_json::to_string(&scope.to_scope_json()).unwrap_or_default()
                );
                (key, scope, None)
            }
            "project" => {
                let scope = match &record.project_id {
                    Some(project_id) => UsageScope::Project {
                        project_id: project_id.clone(),
                    },
                    None => UsageScope::Instance,
                };
                let key = format!(
                    "scope:{}",
                    serde_json::to_string(&scope.to_scope_json()).unwrap_or_default()
                );
                (key, scope, None)
            }
            "model" => {
                let model = record
                    .provider_id
                    .as_deref()
                    .zip(record.model_id.as_deref())
                    .map(|(p, m)| (p.to_owned(), m.to_owned()));
                let key = format!(
                    "model:{}/{}",
                    record.provider_id.as_deref().unwrap_or("-"),
                    record.model_id.as_deref().unwrap_or("-")
                );
                (key, query_scope.clone(), model)
            }
            // "day": bucket by UTC calendar day *and* model; the row scope
            // is the query scope (see the module docs for the shape gap).
            _ => {
                let day = record.occurred_at.get(..10).unwrap_or("unknown").to_owned();
                let model = record
                    .provider_id
                    .as_deref()
                    .zip(record.model_id.as_deref())
                    .map(|(p, m)| (p.to_owned(), m.to_owned()));
                let key = format!(
                    "day:{day}:{}/{}",
                    record.provider_id.as_deref().unwrap_or("-"),
                    record.model_id.as_deref().unwrap_or("-")
                );
                (key, query_scope.clone(), model)
            }
        };
        match group_index.get(&key) {
            Some(index) => groups[*index].3.push(record),
            None => {
                group_index.insert(key.clone(), groups.len());
                groups.push((key, scope, model, vec![record]));
            }
        }
    }

    // Deterministic row order: by group key (day/model keys sort
    // chronologically/lexicographically; scope keys by their JSON form).
    groups.sort_by(|a, b| a.0.cmp(&b.0));
    let rows: Vec<Value> = groups
        .into_iter()
        .map(|(_, scope, model, members)| {
            json!({
                "scope": scope.to_scope_json(),
                "model": match &model {
                    Some((provider_id, model_id)) => json!({"providerId": provider_id, "modelId": model_id}),
                    None => Value::Null,
                },
                "inputTokens": aggregate_metric(members.iter().map(|r| &r.input), TOKEN_UNIT),
                "outputTokens": aggregate_metric(members.iter().map(|r| &r.output), TOKEN_UNIT),
                "cost": aggregate_metric(members.iter().map(|r| &r.cost), MIXED_MARKER),
            })
        })
        .collect();

    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "rows": rows,
            "unknownRecordCount": unknown_record_count,
            "asOf": Utc::now().to_rfc3339(),
        }),
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use agentcpm_pd_protocol::registry::OperationRegistry;

    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
    use crate::public_v1::store::InstanceMeta;
    use crate::public_v1::test_support::TempDir;

    use super::*;

    fn open_runtime(root_dir: &std::path::Path) -> (Arc<PublicV1Runtime>, String) {
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

    fn fresh_home(tag: &str) -> (TempDir, Arc<PublicV1Runtime>, String) {
        let tmp = TempDir::new(tag);
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, instance_id) = open_runtime(&root_dir);
        (tmp, rt, instance_id)
    }

    fn contract_code_of(err: &DispatchError) -> String {
        match err {
            DispatchError::Application(e) => e.code.clone(),
            DispatchError::Rpc { data, .. } => data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }
    }

    fn seed_project(rt: &Arc<PublicV1Runtime>, project_id: &str) {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        conn.execute(
            "INSERT INTO projects (project_id, name, root_json, registration_state, environment_id, revision, created_at)
             VALUES (?1, 'usage', '{}', 'registered', 'local', 'rev-1', ?2)",
            params![project_id, Utc::now().to_rfc3339()],
        )
        .unwrap();
    }

    fn seed_session(rt: &Arc<PublicV1Runtime>, project_id: &str, session_id: &str) {
        let conn = rt.store().conn.lock().expect("sqlite mutex");
        conn.execute(
            "INSERT INTO sessions (session_id, project_id, name, tags_json, revision, session_state, queue_revision, created_at)
             VALUES (?1, ?2, 'usage', '[]', 'rev-1', 'active', 'rev-1', ?3)",
            params![session_id, project_id, Utc::now().to_rfc3339()],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn empty_usage_answers_valid_empty_results() {
        let (_tmp, rt, iid) = fresh_home("usage-empty");
        for method in ["usage.snapshot", "usage.summary"] {
            let (outcome, result) = rt
                .dispatch(
                    method,
                    json!({"instanceId": iid, "scope": {"kind": "instance"}}),
                    None,
                    None,
                )
                .await
                .unwrap();
            assert_eq!(outcome, ResponseOutcome::Succeeded);
            OperationRegistry::global()
                .validate_result(method, &result)
                .unwrap_or_else(|e| panic!("{method} result fails schema: {e:?}"));
        }
        let snapshot = rt
            .dispatch(
                "usage.snapshot",
                json!({"instanceId": iid, "scope": {"kind": "instance"}}),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        assert_eq!(snapshot["items"].as_array().map(Vec::len), Some(0));
        assert_eq!(snapshot["complete"], true);
        assert_eq!(snapshot["pageInfo"]["complete"], true);
        let summary = rt
            .dispatch(
                "usage.summary",
                json!({"instanceId": iid, "scope": {"kind": "instance"}}),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        assert_eq!(summary["rows"].as_array().map(Vec::len), Some(0));
        assert_eq!(summary["unknownRecordCount"], 0);
        assert!(summary["asOf"].as_str().is_some_and(|t| !t.is_empty()));
    }

    #[tokio::test]
    async fn snapshot_returns_stored_rows_with_scope_model_and_time_filters() {
        let (_tmp, rt, iid) = fresh_home("usage-snap");
        let (project_id, session_id) = ("p-u1", "s-u1");
        seed_project(&rt, project_id);
        seed_session(&rt, project_id, session_id);

        rt.store()
            .record_usage(UsageRecordIn::actual_tokens(
                "2026-11-01T10:00:00+00:00".to_owned(),
                UsageScope::Session {
                    project_id: project_id.to_owned(),
                    session_id: session_id.to_owned(),
                },
                Some(("prov-a".to_owned(), "m-1".to_owned())),
                100.0,
                50.0,
                Some(0.25),
                "USD",
                "provider_reported",
            ))
            .unwrap();
        rt.store()
            .record_usage(UsageRecordIn::actual_tokens(
                "2026-11-02T10:00:00+00:00".to_owned(),
                UsageScope::Instance,
                Some(("prov-b".to_owned(), "m-2".to_owned())),
                10.0,
                5.0,
                None,
                "USD",
                "provider_reported",
            ))
            .unwrap();

        // Instance scope sees both records, ordered by time.
        let snapshot = rt
            .dispatch(
                "usage.snapshot",
                json!({"instanceId": iid, "scope": {"kind": "instance"}}),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        OperationRegistry::global()
            .validate_result("usage.snapshot", &snapshot)
            .unwrap();
        let items = snapshot["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0]["scope"],
            json!({"kind": "session", "projectId": project_id, "sessionId": session_id})
        );
        assert_eq!(items[0]["model"]["modelId"], "m-1");
        assert_eq!(items[0]["inputTokens"]["value"], 100.0);
        assert_eq!(items[0]["inputTokens"]["status"], "actual");
        assert_eq!(items[0]["inputTokens"]["unit"], "tokens");
        assert_eq!(items[0]["cost"]["value"], 0.25);
        // 「漏usage为unknown/null不是0」.
        assert_eq!(items[1]["cost"]["status"], "unknown");
        assert_eq!(items[1]["cost"]["value"], Value::Null);

        // Session scope filters to its own records.
        let session_only = rt
            .dispatch(
                "usage.snapshot",
                json!({"instanceId": iid, "scope": {"kind": "session", "projectId": project_id, "sessionId": session_id}}),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        assert_eq!(session_only["items"].as_array().map(Vec::len), Some(1));

        // Model filter.
        let model_only = rt
            .dispatch(
                "usage.snapshot",
                json!({
                    "instanceId": iid, "scope": {"kind": "instance"},
                    "model": {"providerId": "prov-b", "modelId": "m-2"},
                }),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        assert_eq!(model_only["items"].as_array().map(Vec::len), Some(1));
        assert_eq!(model_only["items"][0]["model"]["modelId"], "m-2");

        // 「过滤边界准确」: inclusive bounds include the boundary record.
        let bounded = rt
            .dispatch(
                "usage.snapshot",
                json!({
                    "instanceId": iid, "scope": {"kind": "instance"},
                    "from": "2026-11-01T10:00:00+00:00",
                    "to": "2026-11-01T10:00:00+00:00",
                }),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        assert_eq!(bounded["items"].as_array().map(Vec::len), Some(1));

        // Bad time bounds are rejected.
        let err = rt
            .dispatch(
                "usage.snapshot",
                json!({"instanceId": iid, "scope": {"kind": "instance"}, "from": "not-a-date"}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "INVALID_ARGUMENT");

        // Unknown scope targets are refused.
        let err = rt
            .dispatch(
                "usage.snapshot",
                json!({"instanceId": iid, "scope": {"kind": "project", "projectId": "nope"}}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code_of(&err), "INVALID_ARGUMENT");
    }

    #[tokio::test]
    async fn summary_aggregates_status_units_and_unknown_counts() {
        let (_tmp, rt, iid) = fresh_home("usage-sum");
        let (project_id, session_id) = ("p-u2", "s-u2a");
        seed_project(&rt, project_id);
        seed_session(&rt, project_id, session_id);
        seed_session(&rt, project_id, "s-u2b");

        let store = rt.store();
        // Two actual records + one estimated for session A, model m-1.
        // Dyadic cost fractions (0.25/0.5/0.125) keep the aggregate exact in
        // IEEE 754 so the assertion compares literal equality.
        for (input, output, cost) in [(100.0, 50.0, 0.25), (200.0, 100.0, 0.5)] {
            store
                .record_usage(UsageRecordIn::actual_tokens(
                    "2026-11-01T10:00:00+00:00".to_owned(),
                    UsageScope::Session {
                        project_id: project_id.to_owned(),
                        session_id: session_id.to_owned(),
                    },
                    Some(("prov-a".to_owned(), "m-1".to_owned())),
                    input,
                    output,
                    Some(cost),
                    "USD",
                    "provider_reported",
                ))
                .unwrap();
        }
        store
            .record_usage(UsageRecordIn {
                occurred_at: "2026-11-01T11:00:00+00:00".to_owned(),
                scope: UsageScope::Session {
                    project_id: project_id.to_owned(),
                    session_id: session_id.to_owned(),
                },
                model: Some(("prov-a".to_owned(), "m-1".to_owned())),
                input: Metric {
                    value: Some(30.0),
                    status: "estimated".to_owned(),
                    unit: TOKEN_UNIT.to_owned(),
                    source: "built_in_estimate".to_owned(),
                },
                output: Metric {
                    value: None,
                    status: "unknown".to_owned(),
                    unit: TOKEN_UNIT.to_owned(),
                    source: "built_in_estimate".to_owned(),
                },
                cost: Metric {
                    value: Some(0.125),
                    status: "estimated".to_owned(),
                    unit: "USD".to_owned(),
                    source: "built_in_estimate".to_owned(),
                },
            })
            .unwrap();
        // Session B: an unknown-cost record in EUR (a different currency).
        store
            .record_usage(UsageRecordIn::actual_tokens(
                "2026-11-02T10:00:00+00:00".to_owned(),
                UsageScope::Session {
                    project_id: project_id.to_owned(),
                    session_id: "s-u2b".to_owned(),
                },
                Some(("prov-a".to_owned(), "m-2".to_owned())),
                7.0,
                3.0,
                None,
                "EUR",
                "provider_reported",
            ))
            .unwrap();

        // Default groupBy=session: one row per session.
        let summary = rt
            .dispatch(
                "usage.summary",
                json!({"instanceId": iid, "scope": {"kind": "instance"}}),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        OperationRegistry::global()
            .validate_result("usage.summary", &summary)
            .unwrap();
        let rows = summary["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 2, "one row per session: {rows:?}");
        let row_a = rows
            .iter()
            .find(|r| r["scope"]["sessionId"] == session_id)
            .expect("session A row");
        // actual + actual + estimated -> estimated with the full sum.
        assert_eq!(row_a["inputTokens"]["status"], "estimated");
        assert_eq!(row_a["inputTokens"]["value"], 330.0);
        // One unknown output record poisons the output aggregate.
        assert_eq!(row_a["outputTokens"]["status"], "unknown");
        assert_eq!(row_a["outputTokens"]["value"], Value::Null);
        assert_eq!(row_a["cost"]["value"], 0.875);
        assert_eq!(row_a["cost"]["unit"], "USD");
        // 「未知计数保留」: the estimated record's unknown output and the
        // second session's missing cost each count once.
        assert_eq!(summary["unknownRecordCount"], 2);

        // groupBy=project: one row for the project (all records).
        let by_project = rt
            .dispatch(
                "usage.summary",
                json!({
                    "instanceId": iid,
                    "scope": {"kind": "project", "projectId": project_id},
                    "groupBy": "project",
                }),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        let rows = by_project["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        // Mixed currencies (USD + EUR) refuse to sum.
        assert_eq!(rows[0]["cost"]["status"], "unknown");
        assert_eq!(rows[0]["cost"]["value"], Value::Null);
        assert_eq!(rows[0]["cost"]["unit"], "mixed");

        // groupBy=model: one row per model, scope = the query scope.
        let by_model = rt
            .dispatch(
                "usage.summary",
                json!({
                    "instanceId": iid, "scope": {"kind": "instance"},
                    "groupBy": "model",
                }),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        let rows = by_model["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        for row in rows {
            assert_eq!(row["scope"], json!({"kind": "instance"}));
            assert!(row["model"]["modelId"].as_str().is_some());
        }

        // groupBy=day: chronological buckets (model split within a day).
        let by_day = rt
            .dispatch(
                "usage.summary",
                json!({
                    "instanceId": iid, "scope": {"kind": "instance"},
                    "groupBy": "day",
                }),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        let rows = by_day["rows"].as_array().unwrap();
        assert_eq!(
            rows.len(),
            2,
            "2026-11-01 (m-1) and 2026-11-02 (m-2): {rows:?}"
        );
        assert_eq!(rows[0]["inputTokens"]["value"], 330.0);

        // A time-bounded query restricts the aggregation.
        let bounded = rt
            .dispatch(
                "usage.summary",
                json!({
                    "instanceId": iid, "scope": {"kind": "instance"},
                    "from": "2026-11-02T00:00:00+00:00",
                }),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        let rows = bounded["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["inputTokens"]["value"], 7.0);
    }

    #[tokio::test]
    async fn usage_snapshot_paginates() {
        let (_tmp, rt, iid) = fresh_home("usage-page");
        for n in 0..3 {
            rt.store()
                .record_usage(UsageRecordIn::actual_tokens(
                    format!("2026-11-0{}T10:00:00+00:00", n + 1),
                    UsageScope::Instance,
                    None,
                    1.0,
                    1.0,
                    None,
                    "USD",
                    "provider_reported",
                ))
                .unwrap();
        }
        let first = rt
            .dispatch(
                "usage.snapshot",
                json!({"instanceId": iid, "scope": {"kind": "instance"}, "page": {"limit": 2}}),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        assert_eq!(first["items"].as_array().map(Vec::len), Some(2));
        assert_eq!(first["complete"], false);
        let cursor = first["pageInfo"]["nextCursor"].as_str().unwrap().to_owned();
        let second = rt
            .dispatch(
                "usage.snapshot",
                json!({"instanceId": iid, "scope": {"kind": "instance"}, "page": {"limit": 2, "cursor": cursor}}),
                None,
                None,
            )
            .await
            .unwrap()
            .1;
        assert_eq!(second["items"].as_array().map(Vec::len), Some(1));
        assert_eq!(second["complete"], true);
        assert_eq!(second["pageInfo"]["nextCursor"], Value::Null);
    }
}
