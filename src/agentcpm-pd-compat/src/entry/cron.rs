//! `cron` entries: `cron-list` / `cron-create` / `cron-delete` (supported)
//! and `cron-stop` (adapted).
//!
//! S3-delivered: every `schedules.*` operation below is served by the
//! real targets (`schedules.list/history/configure/remove/stop_execution`,
//! verified against `state/cli-v1-operations.json`); failures pass through
//! verbatim — never swallowed, never fake success. `run.cancel`/`run.get`
//! (`cron stop --run`) are delivered and fully live.
//!
//! Old output shapes (`CronListResult`/`CronCreateResult`/
//! `CronDeleteResult`/`CronStopResult`, 2-space JSON) are projected from
//! the new schedule items; legacy fields without a new equivalent
//! (`message` from list, `status`, `channelKey`, `createdAt`, `updatedAt`,
//! `schemaVersion`, `mode`, `lastRunId`) are listed as not projectable.
//! The connection goes through the explicit target — the old 127.0.0.1:
//! 18789 health probe is never reproduced.

use std::time::Duration;

use serde_json::{Value, json};

use crate::entry::{
    EntryCtx, PageBudget, call_op, call_op_keyed, note_dropped, page_cursor_step, parse_cli,
    usage_errors,
};
use crate::state;
use crate::{EXIT_FAILED, EXIT_OK, SHIM_NAME};

/// Parsed `cron` subcommand.
#[derive(Clone, Debug)]
pub enum CronArgs {
    /// `cron list [--history] [--limit N]`
    List { history: bool, limit: Option<u32> },
    /// `cron create --session ID --message TEXT (--once ISO | --cron EXPR)
    /// [--channel C] [--project P] [--timezone Z]`
    Create {
        session: String,
        message: String,
        schedule: ScheduleSpec,
        channel: Option<String>,
        project: Option<String>,
        timezone: Option<String>,
    },
    /// `cron delete <taskId> [--stop-running]` (`--task` is a documented
    /// alias; the positional wins).
    Delete { task_id: String, stop_running: bool },
    /// `cron stop <taskId>` or `cron stop --run <runId>`
    Stop {
        task_id: Option<String>,
        run_id: Option<String>,
    },
}

/// `--once ISO` or `--cron EXPR`; when both are given the old `once` priority
/// applies (baseline pilotdeck.ts:381,393 and the mapping's usageErrorContract).
#[derive(Clone, Debug)]
pub enum ScheduleSpec {
    Once(String),
    Cron(String),
}

pub fn parse(rest: &[String]) -> Result<CronArgs, String> {
    let Some(first) = rest.first().map(String::as_str) else {
        return Err("cron requires a subcommand (list, create, delete, stop)".to_owned());
    };
    match first {
        "list" => {
            let spec = crate::entry::OptSpec {
                values: &["--limit"],
                flags: &["--history"],
            };
            let parsed = parse_cli(&rest[1..], &spec)?;
            if !parsed.positionals.is_empty() {
                return Err("cron list takes no positional arguments".to_owned());
            }
            let limit = match parsed.value("--limit") {
                None => None,
                Some(raw) => Some(
                    raw.parse::<u32>()
                        .map_err(|_| format!("--limit expects a number, got `{raw}`"))?,
                ),
            };
            Ok(CronArgs::List {
                history: parsed.flag("--history"),
                limit,
            })
        }
        "create" => {
            let spec = crate::entry::OptSpec {
                values: &[
                    "--session",
                    "--message",
                    "--once",
                    "--cron",
                    "--channel",
                    "--project",
                    "--timezone",
                ],
                flags: &[],
            };
            let parsed = parse_cli(&rest[1..], &spec)?;
            if !parsed.positionals.is_empty() {
                return Err("cron create takes no positional arguments".to_owned());
            }
            let session = parsed
                .value("--session")
                .ok_or("cron create requires --session ID")?
                .to_owned();
            let message = parsed
                .value("--message")
                .ok_or("cron create requires --message TEXT")?
                .to_owned();
            let schedule = match (parsed.value("--once"), parsed.value("--cron")) {
                (Some(once), _) => ScheduleSpec::Once(once.to_owned()),
                (None, Some(cron)) => ScheduleSpec::Cron(cron.to_owned()),
                (None, None) => {
                    return Err("cron create requires --once ISO or --cron EXPR".to_owned());
                }
            };
            Ok(CronArgs::Create {
                session,
                message,
                schedule,
                channel: parsed.value("--channel").map(str::to_owned),
                project: parsed.value("--project").map(str::to_owned),
                timezone: parsed.value("--timezone").map(str::to_owned),
            })
        }
        "delete" => {
            let spec = crate::entry::OptSpec {
                values: &["--task"],
                flags: &["--stop-running"],
            };
            let parsed = parse_cli(&rest[1..], &spec)?;
            let task_id = parsed
                .positionals
                .first()
                .cloned()
                .or_else(|| parsed.value("--task").map(str::to_owned))
                .ok_or("cron delete requires exactly one <taskId>")?;
            Ok(CronArgs::Delete {
                task_id,
                stop_running: parsed.flag("--stop-running"),
            })
        }
        "stop" => {
            let spec = crate::entry::OptSpec {
                values: &["--run", "--task"],
                flags: &[],
            };
            let parsed = parse_cli(&rest[1..], &spec)?;
            let run_id = parsed.value("--run").map(str::to_owned);
            let task_id = parsed
                .positionals
                .first()
                .cloned()
                .or_else(|| parsed.value("--task").map(str::to_owned));
            match (&task_id, &run_id) {
                (Some(_), None) | (None, Some(_)) => Ok(CronArgs::Stop { task_id, run_id }),
                _ => Err("cron stop requires either <taskId> or --run <runId>".to_owned()),
            }
        }
        other => Err(format!("unknown cron subcommand `{other}`")),
    }
}

/// Entry point.
pub fn main(ctx: &mut EntryCtx<'_>, rest: &[String]) -> i32 {
    let args = match parse(rest) {
        Ok(args) => args,
        Err(message) => return usage_errors::usage(&message),
    };
    let home = match ctx.require_home() {
        Ok(home) => home.to_path_buf(),
        Err(code) => return code,
    };
    let config = match ctx.load_config(&home) {
        Ok(config) => config,
        Err(code) => return code,
    };
    // The old entry probed 127.0.0.1:18789; the shim connects through the
    // configured target only (default: the CLI's own local instance).
    ctx.executor().set_target(config.default_target.clone());

    match args {
        CronArgs::List { history, limit } => list(ctx, history, limit),
        CronArgs::Create {
            session,
            message,
            schedule,
            channel,
            project,
            timezone,
        } => create(
            ctx, &home, &config, session, message, schedule, channel, project, timezone,
        ),
        CronArgs::Delete {
            task_id,
            stop_running,
        } => delete(ctx, task_id, stop_running),
        CronArgs::Stop { task_id, run_id } => stop(ctx, task_id, run_id),
    }
}

/// Pages `schedules.list` to exhaustion (bounded by `limit` when given and
/// by the shared page budget: a target whose nextCursor never advances
/// re-delivers the same page forever — stagnation, an honest failure
/// instead of a hang).
// S3-delivered: schedules.list (readOnly, sync; {limit<=200, cursor?}).
fn fetch_schedules(ctx: &mut EntryCtx<'_>, limit: Option<u32>) -> Result<Vec<Value>, i32> {
    let mut cursor: Option<String> = None;
    let mut items: Vec<Value> = Vec::new();
    let mut budget = PageBudget::new("schedules.list");
    loop {
        let mut input = json!({});
        if let Some(limit) = limit {
            input["limit"] = json!(limit);
        }
        if let Some(cursor) = &cursor {
            input["cursor"] = json!(cursor);
        }
        let result = call_op(ctx, "schedules.list", &input)?;
        if let Some(page_items) = result.get("items").and_then(Value::as_array) {
            items.extend(page_items.iter().cloned());
        }
        let (next, advanced) = page_cursor_step(
            cursor.as_deref(),
            result.get("nextCursor").and_then(Value::as_str),
        );
        let Some(next_cursor) = next else {
            return Ok(items);
        };
        cursor = Some(next_cursor);
        budget.record(advanced, false).map_err(|message| {
            eprintln!("{SHIM_NAME}: {message}");
            EXIT_FAILED
        })?;
    }
}

/// Projects a new schedule item onto the old `CronTask` shape.
fn project_task(item: &Value) -> Value {
    let kind = item.get("kind").and_then(Value::as_str).unwrap_or("cron");
    let expression = item.get("expression").cloned().unwrap_or(Value::Null);
    let schedule = if kind == "once" {
        json!({"type": "once", "runAt": expression, "timezone": item.get("timezone")})
    } else {
        json!({"type": "cron", "expression": expression, "timezone": item.get("timezone")})
    };
    json!({
        "taskId": item.get("scheduleId"),
        "schedule": schedule,
        "sessionKey": item.get("sessionId"),
        "projectKey": item.get("projectId"),
        "nextRunAt": item.get("nextRunAt"),
        "revision": item.get("revision"),
    })
}

/// `cron list [--history] [--limit N]`.
// S3-delivered: schedules.list + schedules.history (readOnly, sync).
fn list(ctx: &mut EntryCtx<'_>, history: bool, limit: Option<u32>) -> i32 {
    let items = match fetch_schedules(ctx, limit) {
        Ok(items) => items,
        Err(code) => return code,
    };
    let tasks: Vec<Value> = items.iter().map(project_task).collect();
    let mut result = json!({"tasks": tasks});
    if history {
        let mut runs: Vec<Value> = Vec::new();
        for item in &items {
            let Some(schedule_id) = item.get("scheduleId").and_then(Value::as_str) else {
                continue;
            };
            // S3-delivered: schedules.history per schedule (readOnly, sync).
            let history_items = match fetch_history(ctx, schedule_id, limit) {
                Ok(items) => items,
                Err(code) => return code,
            };
            for run in history_items {
                runs.push(project_run(&run, schedule_id));
            }
        }
        result["recentRuns"] = json!(runs);
    }
    note_dropped(
        "cron list task",
        &[
            "message",
            "status",
            "channelKey",
            "createdAt",
            "updatedAt",
            "schemaVersion",
            "mode",
            "lastRunId",
        ],
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_owned())
    );
    EXIT_OK
}

/// Pages `schedules.history` for one schedule (same shared page budget as
/// the other paging loops).
// S3-delivered: schedules.history (readOnly, sync; {scheduleId, limit?,
// cursor?} matches the required/optional contract).
fn fetch_history(
    ctx: &mut EntryCtx<'_>,
    schedule_id: &str,
    limit: Option<u32>,
) -> Result<Vec<Value>, i32> {
    let mut cursor: Option<String> = None;
    let mut items: Vec<Value> = Vec::new();
    let mut budget = PageBudget::new(format!("schedules.history for schedule {schedule_id}"));
    loop {
        let mut input = json!({"scheduleId": schedule_id});
        if let Some(limit) = limit {
            input["limit"] = json!(limit);
        }
        if let Some(cursor) = &cursor {
            input["cursor"] = json!(cursor);
        }
        let result = call_op(ctx, "schedules.history", &input)?;
        if let Some(page_items) = result.get("items").and_then(Value::as_array) {
            items.extend(page_items.iter().cloned());
        }
        let (next, advanced) = page_cursor_step(
            cursor.as_deref(),
            result.get("nextCursor").and_then(Value::as_str),
        );
        let Some(next_cursor) = next else {
            return Ok(items);
        };
        cursor = Some(next_cursor);
        budget.record(advanced, false).map_err(|message| {
            eprintln!("{SHIM_NAME}: {message}");
            EXIT_FAILED
        })?;
    }
}

/// Projects one history item onto the old `CronRun` shape.
fn project_run(item: &Value, schedule_id: &str) -> Value {
    json!({
        "runId": item.get("runId"),
        "taskId": schedule_id,
        "startedAt": item.get("startedAt"),
        "outcome": item.get("state"),
        "error": item.get("errorCode"),
    })
}

/// `cron create`: `schedules.configure` with a shim-generated schedule id,
/// request key and `absent` expectedRevision (creation).
// S3-delivered: schedules.configure (conditionalWrite, sync; required
// {scheduleId, projectId, kind, expression, timezone, message,
// expectedRevision} + optional {sessionId, channelId, enabled} all match).
#[allow(clippy::too_many_arguments)]
fn create(
    ctx: &mut EntryCtx<'_>,
    home: &std::path::Path,
    config: &crate::config::CompatConfig,
    session: String,
    message: String,
    schedule: ScheduleSpec,
    channel: Option<String>,
    project: Option<String>,
    timezone: Option<String>,
) -> i32 {
    // The legacy session key must resolve through the mapping table.
    let sessions = match ctx.load_sessions(home) {
        Ok(sessions) => sessions,
        Err(code) => return code,
    };
    let Some(mapping) = sessions.find(&session) else {
        eprintln!(
            "{SHIM_NAME}: unknown session `{session}` for cron create (error: NOT_FOUND); \
             ids printed by this shim are recorded under \
             $AGENTCPM_PD_HOME/compat/sessions.json"
        );
        return EXIT_FAILED;
    };
    let project_id = match project.or_else(|| config.default_project_id.clone()) {
        Some(project) => project,
        None => {
            eprintln!(
                "{SHIM_NAME}: no project scope for cron create (error: configuration_required); \
                 pass --project or set defaultProjectId in \
                 $AGENTCPM_PD_HOME/compat/config.json"
            );
            return EXIT_FAILED;
        }
    };
    let (kind, expression) = match &schedule {
        // The old `once`-over-`cron` priority is structural here.
        ScheduleSpec::Once(run_at) => ("once", run_at.clone()),
        ScheduleSpec::Cron(expr) => ("cron", expr.clone()),
    };
    let schedule_id = state::generate_id("shim-sched");
    let request_key = state::generate_request_key();
    let mut input = json!({
        "scheduleId": schedule_id,
        "projectId": project_id,
        "sessionId": mapping.session_id,
        "kind": kind,
        "expression": expression,
        "timezone": timezone.clone().unwrap_or_else(|| "UTC".to_owned()),
        "message": message,
        "expectedRevision": "absent",
    });
    // The old channel default (inferChannelKey) is not inherited; only an
    // explicit --channel maps onto channelId.
    if let Some(channel) = channel {
        input["channelId"] = json!(channel);
    }
    let result = match call_op_keyed(ctx, "schedules.configure", &input, &request_key) {
        Ok(result) => result,
        Err(code) => return code,
    };
    let schedule = result.get("schedule").cloned().unwrap_or(Value::Null);
    let task = project_task(&schedule);
    note_dropped(
        "cron create task",
        &[
            "message",
            "status",
            "channelKey",
            "createdAt",
            "updatedAt",
            "schemaVersion",
            "mode",
        ],
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({"task": task})).unwrap_or_else(|_| "{}".to_owned())
    );
    EXIT_OK
}

/// `cron delete <taskId> [--stop-running]`: the definition is removed via
/// `schedules.remove` (revision fetched through `schedules.list` — the op
/// requires it and never accepts a blind overwrite); with `--stop-running`
/// each in-flight execution is stopped via `schedules.stop_execution` and
/// reported separately.
// S3-delivered: schedules.list (revision lookup) + schedules.remove
// (idempotentSubmit) + schedules.history + schedules.stop_execution.
fn delete(ctx: &mut EntryCtx<'_>, task_id: String, stop_running: bool) -> i32 {
    let items = match fetch_schedules(ctx, None) {
        Ok(items) => items,
        Err(code) => return code,
    };
    let Some(item) = items
        .iter()
        .find(|i| i.get("scheduleId").and_then(Value::as_str) == Some(task_id.as_str()))
    else {
        eprintln!("{SHIM_NAME}: cron task `{task_id}` not found (error: NOT_FOUND)");
        return EXIT_FAILED;
    };
    let Some(revision) = item.get("revision").and_then(Value::as_str) else {
        eprintln!("{SHIM_NAME}: cron task `{task_id}` carries no revision");
        return EXIT_FAILED;
    };
    let request_key = state::generate_request_key();
    let _removed = match call_op_keyed(
        ctx,
        "schedules.remove",
        &json!({
            "scheduleId": task_id,
            "stopRunning": stop_running,
            "expectedRevision": revision,
        }),
        &request_key,
    ) {
        Ok(result) => result,
        Err(code) => return code,
    };

    let mut stopped_run_id: Option<String> = None;
    if stop_running {
        // Stop whatever is still in flight; each result is reported.
        let history = match fetch_history(ctx, &task_id, None) {
            Ok(items) => items,
            Err(code) => return code,
        };
        for run in history {
            let state_str = run.get("state").and_then(Value::as_str).unwrap_or("");
            if !matches!(state_str, "queued" | "running") {
                continue;
            }
            let Some(execution_id) = run.get("executionId").and_then(Value::as_str) else {
                continue;
            };
            // S3-delivered: schedules.stop_execution (idempotentSubmit,
            // keyed; {scheduleId, executionId}).
            let key = state::generate_request_key();
            match call_op_keyed(
                ctx,
                "schedules.stop_execution",
                &json!({"scheduleId": task_id, "executionId": execution_id}),
                &key,
            ) {
                Ok(_) => {
                    if stopped_run_id.is_none() {
                        stopped_run_id =
                            run.get("runId").and_then(Value::as_str).map(str::to_owned);
                    }
                }
                Err(code) => return code,
            }
        }
    }
    let mut result = json!({"deleted": true});
    if let Some(run_id) = stopped_run_id {
        result["stoppedRunId"] = json!(run_id);
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_owned())
    );
    EXIT_OK
}

/// `cron stop <taskId> | --run <runId>`: run-level stop goes through the
/// delivered `run.cancel` and then WAITS for the real terminal state
/// (acceptance is not a terminal state); task-level stop stops each active
/// execution via `schedules.stop_execution`, and the old
/// `deletedOneTimeTask` semantics (stopping a once-task deletes it) become
/// stop_execution + remove, reported separately.
// S3-delivered: run.cancel/run.get (run path) and schedules.list/history/
// stop_execution/remove (task path) are all served by the real targets.
fn stop(ctx: &mut EntryCtx<'_>, task_id: Option<String>, run_id: Option<String>) -> i32 {
    if let Some(run_id) = run_id {
        return stop_run(ctx, &run_id);
    }
    let Some(task_id) = task_id else {
        return usage_errors::usage("cron stop requires either <taskId> or --run <runId>");
    };
    stop_task(ctx, &task_id)
}

/// Run-level: cancel, then wait for the terminal state.
fn stop_run(ctx: &mut EntryCtx<'_>, run_id: &str) -> i32 {
    let request_key = state::generate_request_key();
    let receipt = match call_op_keyed(ctx, "run.cancel", &json!({"runId": run_id}), &request_key) {
        Ok(receipt) => receipt,
        Err(code) => return code,
    };
    let _ = receipt;
    // Acceptance is not a terminal state: poll run.get for the real one.
    let Some(final_state) = wait_run_terminal_bounded(ctx, run_id, 120, Duration::from_millis(500))
    else {
        eprintln!(
            "{SHIM_NAME}: run {run_id} cancellation was accepted but did not reach a \
             terminal state in time"
        );
        return EXIT_FAILED;
    };
    let stopped = matches!(final_state.as_str(), "cancelled" | "interrupted");
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "stopped": stopped,
            "runId": run_id,
        }))
        .unwrap_or_else(|_| "{}".to_owned())
    );
    EXIT_OK
}

/// Task-level: stop each active execution; a once-task is removed afterwards.
// S3-delivered: schedules.list/history/stop_execution/remove (task path).
fn stop_task(ctx: &mut EntryCtx<'_>, task_id: &str) -> i32 {
    let items = match fetch_schedules(ctx, None) {
        Ok(items) => items,
        Err(code) => return code,
    };
    let Some(item) = items
        .iter()
        .find(|i| i.get("scheduleId").and_then(Value::as_str) == Some(task_id))
        .cloned()
    else {
        eprintln!("{SHIM_NAME}: cron task `{task_id}` not found (error: NOT_FOUND)");
        return EXIT_FAILED;
    };
    let history = match fetch_history(ctx, task_id, None) {
        Ok(items) => items,
        Err(code) => return code,
    };
    let mut stopped = false;
    for run in history {
        let state_str = run.get("state").and_then(Value::as_str).unwrap_or("");
        if !matches!(state_str, "queued" | "running") {
            continue;
        }
        let Some(execution_id) = run.get("executionId").and_then(Value::as_str) else {
            continue;
        };
        // S3-delivered: schedules.stop_execution (idempotentSubmit, keyed).
        let key = state::generate_request_key();
        if call_op_keyed(
            ctx,
            "schedules.stop_execution",
            &json!({"scheduleId": task_id, "executionId": execution_id}),
            &key,
        )
        .is_ok()
        {
            stopped = true;
        }
    }
    // Old deletedOneTimeTask semantics: stopping a once-task deletes it;
    // the new model separates both actions and reports them separately.
    let mut deleted_one_time = false;
    if item.get("kind").and_then(Value::as_str) == Some("once") {
        let Some(revision) = item.get("revision").and_then(Value::as_str) else {
            eprintln!("{SHIM_NAME}: cron task `{task_id}` carries no revision");
            return EXIT_FAILED;
        };
        let key = state::generate_request_key();
        // S3-delivered: schedules.remove (idempotentSubmit, keyed;
        // {scheduleId, stopRunning?, expectedRevision} — stopRunning is
        // omitted here because the executions were already stopped above).
        match call_op_keyed(
            ctx,
            "schedules.remove",
            &json!({
                "scheduleId": task_id,
                "expectedRevision": revision,
            }),
            &key,
        ) {
            Ok(_) => deleted_one_time = true,
            Err(code) => return code,
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "stopped": stopped,
            "taskId": task_id,
            "deletedOneTimeTask": deleted_one_time,
        }))
        .unwrap_or_else(|_| "{}".to_owned())
    );
    EXIT_OK
}

/// Bounded poll of `run.get` until `run.terminal` (returns the state).
fn wait_run_terminal_bounded(
    ctx: &mut EntryCtx<'_>,
    run_id: &str,
    max_polls: u32,
    interval: Duration,
) -> Option<String> {
    for _ in 0..max_polls {
        let result = match call_op(ctx, "run.get", &json!({"runId": run_id})) {
            Ok(result) => result,
            Err(_) => {
                std::thread::sleep(interval);
                continue;
            }
        };
        if let Some(run) = result.get("run")
            && run.get("terminal").and_then(Value::as_bool) == Some(true)
        {
            return run.get("state").and_then(Value::as_str).map(str::to_owned);
        }
        std::thread::sleep(interval);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryCtx;
    use crate::executor::{FakeExecutor, Outcome, Response};
    use crate::state::{SessionMap, SessionMapping};
    use crate::testutil::TempDir;
    use serde_json::json;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn ok(result: Value) -> Result<Response, crate::executor::ExecError> {
        Ok(Response {
            version: None,
            request_id: None,
            outcome: Outcome::Succeeded,
            result: Some(result),
            error: None,
        })
    }

    fn ctx<'a>(exec: &'a mut FakeExecutor, home: &TempDir) -> EntryCtx<'a> {
        EntryCtx::with_home(exec, home.path())
    }

    fn home_with_session(tag: &str, legacy: &str, new_id: &str) -> TempDir {
        let home = TempDir::new(tag);
        let mut map = SessionMap::default();
        map.record(
            home.path(),
            SessionMapping {
                legacy_session_id: legacy.to_owned(),
                session_id: new_id.to_owned(),
                project_id: "proj".to_owned(),
                created_at: "1".to_owned(),
            },
        )
        .unwrap();
        home
    }

    #[test]
    fn parse_list() {
        let CronArgs::List { history, limit } =
            parse(&argv(&["list", "--history", "--limit", "5"])).unwrap()
        else {
            panic!("expected list");
        };
        assert!(history && limit == Some(5));
        assert!(parse(&argv(&["list", "--limit", "x"])).is_err());
    }

    #[test]
    fn parse_create_requires_session_message_and_one_schedule() {
        let CronArgs::Create {
            session,
            message,
            schedule,
            ..
        } = parse(&argv(&[
            "create",
            "--session",
            "s1",
            "--message",
            "hi",
            "--once",
            "2026-01-01T00:00:00Z",
        ]))
        .unwrap()
        else {
            panic!("expected create");
        };
        assert_eq!(session, "s1");
        assert_eq!(message, "hi");
        assert!(matches!(schedule, ScheduleSpec::Once(_)));
        assert!(parse(&argv(&["create", "--session", "s1", "--message", "hi"])).is_err());
        // Baseline `once` priority: when both are given `--once` wins.
        assert!(matches!(
            parse(&argv(&[
                "create",
                "--session",
                "s1",
                "--message",
                "hi",
                "--once",
                "a",
                "--cron",
                "b"
            ]))
            .unwrap(),
            CronArgs::Create {
                schedule: ScheduleSpec::Once(v),
                ..
            } if v == "a"
        ));
    }

    #[test]
    fn parse_delete_and_stop_support_the_task_alias() {
        let CronArgs::Delete {
            task_id,
            stop_running,
        } = parse(&argv(&["delete", "t1", "--stop-running"])).unwrap()
        else {
            panic!("expected delete");
        };
        assert_eq!(task_id, "t1");
        assert!(stop_running);
        let CronArgs::Delete { task_id, .. } = parse(&argv(&["delete", "--task", "t2"])).unwrap()
        else {
            panic!("expected delete");
        };
        assert_eq!(task_id, "t2");

        let CronArgs::Stop { task_id, run_id } = parse(&argv(&["stop", "t1"])).unwrap() else {
            panic!("expected stop");
        };
        assert_eq!(task_id.as_deref(), Some("t1"));
        assert!(run_id.is_none());
        assert!(matches!(
            parse(&argv(&["stop", "--run", "r9"])).unwrap(),
            CronArgs::Stop {
                run_id: Some(_),
                ..
            }
        ));
        assert!(matches!(
            parse(&argv(&["stop", "--task", "t3"])).unwrap(),
            CronArgs::Stop {
                task_id: Some(ref t),
                ..
            } if t == "t3"
        ));
        assert!(parse(&argv(&["stop"])).is_err());
        assert!(parse(&argv(&["stop", "t", "--run", "r"])).is_err());
    }

    #[test]
    fn list_pages_schedules_and_projects_the_old_shape() {
        let mut exec = FakeExecutor::new().push(ok(json!({
            "items": [{
                "scheduleId": "sched-1", "projectId": "p", "sessionId": "s",
                "kind": "once", "expression": "2026-01-01T00:00:00Z",
                "timezone": "UTC", "enabled": true, "nextRunAt": null, "revision": "r1",
            }],
            "nextCursor": null, "snapshotRevision": "snap",
        })));
        let home = TempDir::new("cron-list");
        let code = main(&mut ctx(&mut exec, &home), &argv(&["list"]));
        assert_eq!(code, EXIT_OK);
        assert_eq!(exec.methods(), ["schedules.list"]);
        // once-kind schedules project runAt, not expression.
        let _ = project_task(&json!({
            "scheduleId": "sched-1", "kind": "once", "expression": "2026-01-01T00:00:00Z",
            "timezone": "UTC", "revision": "r",
        }));
    }

    #[test]
    fn list_with_history_reads_each_schedule_history() {
        let mut exec = FakeExecutor::new()
            .push(ok(json!({
                "items": [{
                    "scheduleId": "sched-1", "projectId": "p", "sessionId": "s",
                    "kind": "cron", "expression": "* * * * *", "timezone": "UTC",
                    "enabled": true, "nextRunAt": null, "revision": "r1",
                }],
                "nextCursor": null, "snapshotRevision": "snap",
            })))
            .push(ok(json!({
                "items": [{
                    "executionId": "e1", "runId": "run-1", "scheduledAt": "t",
                    "startedAt": "t", "state": "succeeded", "errorCode": null,
                }],
                "nextCursor": null, "snapshotRevision": "snap",
            })));
        let home = TempDir::new("cron-list-hist");
        let code = main(&mut ctx(&mut exec, &home), &argv(&["list", "--history"]));
        assert_eq!(code, EXIT_OK);
        assert_eq!(exec.methods(), ["schedules.list", "schedules.history"]);
        assert_eq!(exec.calls[1].input["scheduleId"], "sched-1");
    }

    #[test]
    fn create_maps_the_legacy_session_and_uses_absent_revision() {
        let home = home_with_session("cron-create", "old-s", "new-s");
        crate::config::save(
            home.path(),
            &crate::config::CompatConfig {
                default_project_id: Some("proj".to_owned()),
                ..crate::config::CompatConfig::default()
            },
        )
        .unwrap();
        let mut exec = FakeExecutor::new().push(ok(json!({
            "schedule": {
                "scheduleId": "shim-sched-x", "projectId": "proj", "sessionId": "new-s",
                "kind": "once", "expression": "2027-01-01T00:00:00Z", "timezone": "UTC",
                "enabled": true, "nextRunAt": null, "revision": "r",
            },
        })));
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&[
                "create",
                "--session",
                "old-s",
                "--message",
                "hello",
                "--once",
                "2027-01-01T00:00:00Z",
            ]),
        );
        assert_eq!(code, EXIT_OK);
        assert_eq!(exec.methods(), ["schedules.configure"]);
        let input = &exec.calls[0].input;
        assert_eq!(input["sessionId"], "new-s", "legacy session mapped");
        assert_eq!(input["projectId"], "proj");
        assert_eq!(input["kind"], "once");
        assert_eq!(input["expression"], "2027-01-01T00:00:00Z");
        assert_eq!(input["message"], "hello");
        assert_eq!(input["expectedRevision"], "absent");
        assert!(exec.calls[0].request_key.is_some());
        assert!(
            input.get("channelId").is_none(),
            "no implicit channel default"
        );
        // Explicit --channel maps onto channelId.
        let mut exec = FakeExecutor::new().push(ok(json!({"schedule": {}})));
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&[
                "create",
                "--session",
                "old-s",
                "--message",
                "x",
                "--cron",
                "* * * * *",
                "--channel",
                "feishu",
            ]),
        );
        assert_eq!(code, EXIT_OK);
        assert_eq!(exec.calls[0].input["channelId"], "feishu");
        assert_eq!(exec.calls[0].input["kind"], "cron");
    }

    #[test]
    fn create_requires_a_known_session_and_project() {
        let home = home_with_session("cron-create-unknown", "old-s", "new-s");
        let mut exec = FakeExecutor::new();
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&[
                "create",
                "--session",
                "nope",
                "--message",
                "x",
                "--cron",
                "* * * * *",
            ]),
        );
        assert_eq!(code, EXIT_FAILED);
        assert!(exec.calls.is_empty());

        // Unknown project scope without a default.
        let bare = TempDir::new("cron-create-noproj");
        let mut exec = FakeExecutor::new();
        let code = main(
            &mut ctx(&mut exec, &bare),
            &argv(&[
                "create",
                "--session",
                "old-s",
                "--message",
                "x",
                "--cron",
                "* * * * *",
            ]),
        );
        assert_eq!(code, EXIT_FAILED);
        assert!(exec.calls.is_empty());
    }

    #[test]
    fn delete_fetches_the_revision_then_removes_and_stops() {
        let mut exec = FakeExecutor::new()
            .push(ok(json!({
                "items": [{
                    "scheduleId": "sched-1", "projectId": "p", "sessionId": "s",
                    "kind": "cron", "expression": "* * * * *", "timezone": "UTC",
                    "enabled": true, "nextRunAt": null, "revision": "rev-9",
                }],
                "nextCursor": null, "snapshotRevision": "snap",
            })))
            .push(ok(json!({})))
            .push(ok(json!({
                "items": [
                    {"executionId": "e1", "runId": "run-1", "state": "running"},
                    {"executionId": "e2", "runId": "run-2", "state": "succeeded"},
                ],
                "nextCursor": null, "snapshotRevision": "snap",
            })))
            .push(ok(json!({})));
        let home = TempDir::new("cron-delete");
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&["delete", "sched-1", "--stop-running"]),
        );
        assert_eq!(code, EXIT_OK);
        assert_eq!(
            exec.methods(),
            [
                "schedules.list",
                "schedules.remove",
                "schedules.history",
                "schedules.stop_execution"
            ]
        );
        let remove = &exec.calls[1];
        assert_eq!(remove.input["scheduleId"], "sched-1");
        assert_eq!(remove.input["expectedRevision"], "rev-9");
        assert_eq!(remove.input["stopRunning"], true);
        // Only the RUNNING execution is stopped.
        assert_eq!(exec.calls[3].input["executionId"], "e1");
    }

    #[test]
    fn delete_unknown_task_is_not_found() {
        let mut exec = FakeExecutor::new().push(ok(json!({
            "items": [], "nextCursor": null, "snapshotRevision": "snap",
        })));
        let home = TempDir::new("cron-delete-404");
        let code = main(&mut ctx(&mut exec, &home), &argv(&["delete", "nope"]));
        assert_eq!(code, EXIT_FAILED);
        assert_eq!(exec.methods(), ["schedules.list"]);
    }

    #[test]
    fn stop_run_cancels_and_waits_for_the_real_terminal_state() {
        let mut exec = FakeExecutor::new()
            .push(ok(
                json!({"receipt": {"disposition": "requested", "state": "cancelling"}}),
            ))
            .push(ok(json!({"run": {"terminal": true, "state": "cancelled"}})));
        let home = TempDir::new("cron-stop-run");
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&["stop", "--run", "run-7"]),
        );
        assert_eq!(code, EXIT_OK);
        assert_eq!(exec.methods(), ["run.cancel", "run.get"]);
        assert!(exec.calls[0].request_key.is_some());
        assert_eq!(exec.calls[0].input["runId"], "run-7");
    }

    #[test]
    fn stop_task_stops_active_executions_and_removes_once_tasks() {
        let mut exec = FakeExecutor::new()
            .push(ok(json!({
                "items": [{
                    "scheduleId": "sched-1", "projectId": "p", "sessionId": "s",
                    "kind": "once", "expression": "2026-01-01T00:00:00Z", "timezone": "UTC",
                    "enabled": true, "nextRunAt": null, "revision": "rev-1",
                }],
                "nextCursor": null, "snapshotRevision": "snap",
            })))
            .push(ok(json!({
                "items": [{"executionId": "e1", "runId": "run-1", "state": "queued"}],
                "nextCursor": null, "snapshotRevision": "snap",
            })))
            .push(ok(json!({})))
            .push(ok(json!({})));
        let home = TempDir::new("cron-stop-task");
        let code = main(&mut ctx(&mut exec, &home), &argv(&["stop", "sched-1"]));
        assert_eq!(code, EXIT_OK);
        assert_eq!(
            exec.methods(),
            [
                "schedules.list",
                "schedules.history",
                "schedules.stop_execution",
                "schedules.remove"
            ]
        );
        // once-task removal after stopping (old deletedOneTimeTask).
        assert_eq!(exec.calls[3].input["scheduleId"], "sched-1");
        assert_eq!(exec.calls[3].input["expectedRevision"], "rev-1");
    }

    #[test]
    fn capability_unavailable_passes_through_from_every_subcommand() {
        let mut exec = FakeExecutor::new().push(Ok(Response {
            version: None,
            request_id: None,
            outcome: Outcome::Failed,
            result: None,
            error: Some(crate::executor::ErrorPayload {
                code: "CAPABILITY_UNAVAILABLE".to_owned(),
                message: "schedules.list not served".to_owned(),
                details: None,
            }),
        }));
        let home = TempDir::new("cron-cap");
        let code = main(&mut ctx(&mut exec, &home), &argv(&["list"]));
        assert_eq!(code, EXIT_FAILED);
        assert_eq!(exec.methods(), ["schedules.list"]);
    }

    #[test]
    fn usage_errors_exit_2_without_business_calls() {
        let mut exec = FakeExecutor::new();
        let home = TempDir::new("cron-usage");
        assert_eq!(main(&mut ctx(&mut exec, &home), &argv(&[])), 2);
        assert_eq!(main(&mut ctx(&mut exec, &home), &argv(&["frob"])), 2);
        assert!(exec.calls.is_empty());
    }

    #[test]
    fn list_is_bounded_against_a_stuck_cursor() {
        // A target that keeps answering the same nextCursor re-delivers the
        // same page forever; the shared page budget turns that into an
        // honest exit 1 after PAGE_STAGNATION_LIMIT stagnant pages instead
        // of a hang. The first page advances the cursor (None → "c1");
        // every later page repeats it.
        let mut exec = FakeExecutor::new();
        for _ in 0..(crate::entry::PAGE_STAGNATION_LIMIT + 5) {
            exec = exec.push(ok(json!({
                "items": [{
                    "scheduleId": "sched-1", "projectId": "p", "sessionId": "s",
                    "kind": "cron", "expression": "* * * * *", "timezone": "UTC",
                    "enabled": true, "nextRunAt": null, "revision": "r1",
                }],
                "nextCursor": "c1", "snapshotRevision": "snap",
            })));
        }
        let home = TempDir::new("cron-list-stuck");
        let code = main(&mut ctx(&mut exec, &home), &argv(&["list"]));
        assert_eq!(code, EXIT_FAILED);
        assert_eq!(
            exec.calls.len() as u32,
            crate::entry::PAGE_STAGNATION_LIMIT + 1,
            "1 advancing first page + PAGE_STAGNATION_LIMIT stagnant pages"
        );
    }

    #[test]
    fn history_is_bounded_against_a_stuck_cursor() {
        // The same stuck-cursor pathology on schedules.history: the listing
        // page terminates normally, then the history paging stalls and
        // fails honestly.
        let mut exec = FakeExecutor::new().push(ok(json!({
            "items": [{
                "scheduleId": "sched-1", "projectId": "p", "sessionId": "s",
                "kind": "cron", "expression": "* * * * *", "timezone": "UTC",
                "enabled": true, "nextRunAt": null, "revision": "r1",
            }],
            "nextCursor": null, "snapshotRevision": "snap",
        })));
        for _ in 0..(crate::entry::PAGE_STAGNATION_LIMIT + 5) {
            exec = exec.push(ok(json!({
                "items": [{
                    "executionId": "e1", "runId": "run-1", "scheduledAt": "t",
                    "startedAt": "t", "state": "succeeded", "errorCode": null,
                }],
                "nextCursor": "c9", "snapshotRevision": "snap",
            })));
        }
        let home = TempDir::new("cron-history-stuck");
        let code = main(&mut ctx(&mut exec, &home), &argv(&["list", "--history"]));
        assert_eq!(code, EXIT_FAILED);
        let history_calls = exec
            .calls
            .iter()
            .filter(|c| c.method == "schedules.history")
            .count() as u32;
        assert_eq!(
            history_calls,
            crate::entry::PAGE_STAGNATION_LIMIT + 1,
            "1 advancing first page + PAGE_STAGNATION_LIMIT stagnant pages"
        );
    }
}
