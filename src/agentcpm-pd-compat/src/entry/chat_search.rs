//! `chat-search` entry (`cli.compat.chat-search`, adapted): `pilotdeck chat
//! search <keyword> [filters]`.
//!
//! `search.start`/`search.results` are delivered by S3; the orchestration
//! calls them through the executor and surfaces their failures verbatim.
//! `search.start` is async (`execution:"async"` in
//! `state/cli-v1-operations.json`): its receipt only records acceptance, so
//! the shim polls `operations.get` to a terminal state (bounded) before
//! paging `search.results` — a non-terminal operation there is
//! `OPERATION_NOT_READY`, and a failed/cancelled one is reported honestly
//! (exit 1) instead of being projected onto an empty match list.
//!
//! The scope is explicit
//! (`--session` → that session, legacy id resolved through the old→new
//! table into the contract's `{kind: session, projectId, sessionId}`
//! scope; `--all-projects` → instance; `--project` → that project;
//! otherwise the configured `defaultProjectId`; none of these →
//! `configuration_required`)
//! — the old cwd default is not inherited. `--pilot-home` (direct scanning
//! of the legacy data root) is not projectable and fails with
//! `unsupported_projection` instead of silently searching somewhere else.
//! `matches[].lineNumber` (physical JSONL line numbers) and
//! `sessionsScanned` have no new equivalent and are listed as not
//! projectable.

use std::time::Duration;

use serde_json::{Value, json};

use crate::entry::{
    EntryCtx, PageBudget, call_op, call_op_keyed, note_dropped, page_cursor_step, parse_cli,
    usage_errors,
};
use crate::state;
use crate::{EXIT_FAILED, EXIT_OK, SHIM_NAME};

/// Parsed `chat search` arguments.
#[derive(Clone, Debug, Default)]
pub struct ChatSearchArgs {
    pub keyword: Option<String>,
    pub project: Option<String>,
    pub all_projects: bool,
    pub limit: Option<u32>,
    pub json: bool,
    pub regex: bool,
    pub case_sensitive: bool,
    /// `user | assistant | all`
    pub role: Option<String>,
    pub session: Option<String>,
    pub pilot_home: Option<String>,
}

const ROLE_VALUES: [&str; 3] = ["user", "assistant", "all"];

/// Poll budget for waiting out the async `search.start` operation
/// (`operations.get` every [`SEARCH_POLL_INTERVAL`]; generous for a large
/// scan, still bounded — a search that never terminates is an error, not
/// a hang).
const SEARCH_MAX_POLLS: u32 = 600;
const SEARCH_POLL_INTERVAL: Duration = Duration::from_millis(500);

pub fn parse(rest: &[String]) -> Result<ChatSearchArgs, String> {
    if rest.first().map(String::as_str) != Some("search") {
        return Err("expected `chat search <keyword> [options]`".to_owned());
    }
    let spec = crate::entry::OptSpec {
        values: &[
            "--project",
            "--limit",
            "--role",
            "--session",
            "--pilot-home",
        ],
        flags: &["--all-projects", "--json", "--regex", "--case-sensitive"],
    };
    let parsed = parse_cli(&rest[1..], &spec)?;
    if parsed.positionals.len() > 1 {
        return Err("chat search takes at most one <keyword>".to_owned());
    }
    let limit = match parsed.value("--limit") {
        None => None,
        Some(raw) => Some(
            raw.parse::<u32>()
                .map_err(|_| format!("--limit expects a number, got `{raw}`"))?,
        ),
    };
    if let Some(role) = parsed.value("--role")
        && !ROLE_VALUES.contains(&role)
    {
        return Err(format!(
            "--role expects one of user|assistant|all, got `{role}`"
        ));
    }
    Ok(ChatSearchArgs {
        keyword: parsed.positionals.first().cloned(),
        project: parsed.value("--project").map(str::to_owned),
        all_projects: parsed.flag("--all-projects"),
        limit,
        json: parsed.flag("--json"),
        regex: parsed.flag("--regex"),
        case_sensitive: parsed.flag("--case-sensitive"),
        role: parsed.value("--role").map(str::to_owned),
        session: parsed.value("--session").map(str::to_owned),
        pilot_home: parsed.value("--pilot-home").map(str::to_owned),
    })
}

/// Entry point.
pub fn main(ctx: &mut EntryCtx<'_>, rest: &[String]) -> i32 {
    let args = match parse(rest) {
        Ok(args) => args,
        Err(message) => return usage_errors::usage(&message),
    };
    let Some(keyword) = args.keyword.clone() else {
        return usage_errors::usage("chat search requires a <keyword>");
    };
    if args.pilot_home.is_some() {
        eprintln!(
            "{SHIM_NAME}: --pilot-home cannot be projected: the legacy data root is not \
             scanned by the new data source (error: unsupported_projection); migrate \
             with the t020 flow and search the new data source instead"
        );
        return EXIT_FAILED;
    }

    let home = match ctx.require_home() {
        Ok(home) => home.to_path_buf(),
        Err(code) => return code,
    };
    let config = match ctx.load_config(&home) {
        Ok(config) => config,
        Err(code) => return code,
    };

    // Explicit scope: --session → that session (mapping data
    // chat-search.json: `--session` 限定 sessionId; the legacy id resolves
    // through the old→new table into the contract's session scope, whose
    // oneOf branch requires BOTH projectId and sessionId — so --session
    // wins over the wider --all-projects/--project scopes, which it
    // subsumes); otherwise --all-projects → instance; --project → that
    // project; otherwise the configured default project.
    let scope = if let Some(session_arg) = &args.session {
        let sessions = match ctx.load_sessions(&home) {
            Ok(sessions) => sessions,
            Err(code) => return code,
        };
        let Some(mapping) = sessions.find(session_arg) else {
            return usage_errors::usage(&format!(
                "unknown session id `{session_arg}` for --session; ids printed by this \
                 shim are recorded under $AGENTCPM_PD_HOME/compat/sessions.json"
            ));
        };
        if mapping.project_id.is_empty() {
            return usage_errors::usage(&format!(
                "session `{session_arg}` carries no recorded project, so --session cannot \
                 be scoped to it"
            ));
        }
        json!({
            "kind": "session",
            "projectId": mapping.project_id,
            "sessionId": mapping.session_id,
        })
    } else if args.all_projects {
        json!({"kind": "instance"})
    } else {
        let project_id = match args
            .project
            .clone()
            .or_else(|| config.default_project_id.clone())
        {
            Some(project) => project,
            None => {
                eprintln!(
                    "{SHIM_NAME}: no search scope (error: configuration_required); pass \
                     --project, --session or --all-projects, or set defaultProjectId in \
                     $AGENTCPM_PD_HOME/compat/config.json"
                );
                return EXIT_FAILED;
            }
        };
        json!({"kind": "project", "projectId": project_id})
    };

    let mut start_input = json!({
        "scope": scope,
        "query": keyword,
        "syntax": if args.regex { "regex" } else { "literal" },
        "caseSensitive": args.case_sensitive,
    });
    if let Some(limit) = args.limit {
        start_input["pageLimit"] = json!(limit);
    }
    if let Some(role) = &args.role
        && role != "all"
    {
        start_input["roles"] = json!([role]);
    }

    let request_key = state::generate_request_key();
    let receipt = match call_op_keyed(ctx, "search.start", &start_input, &request_key) {
        Ok(receipt) => receipt,
        Err(code) => return code,
    };
    // Contract receipt (`$defs.receipt` oneOf kind="operation" branch):
    // `operationId` sits at the TOP LEVEL of the result, not under an
    // `identity` wrapper (gateway search.rs constructs it that way).
    let Some(operation_id) = receipt
        .get("operationId")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        eprintln!("{SHIM_NAME}: search.start receipt carries no operationId");
        return EXIT_FAILED;
    };

    // `search.start` is async: acceptance is not a terminal state. Poll
    // `operations.get` to the real one before touching `search.results`
    // (a non-terminal operation answers `OPERATION_NOT_READY`).
    match wait_search_terminal(
        ctx,
        &operation_id,
        receipt.get("state").and_then(Value::as_str),
        SEARCH_MAX_POLLS,
        SEARCH_POLL_INTERVAL,
    ) {
        Ok(()) => {}
        Err(code) => return code,
    }

    // Page the results to exhaustion, bounded by the shared page budget: a
    // target whose nextCursor never advances re-delivers the same page
    // forever — that is stagnation, an honest failure instead of a hang.
    let mut cursor: Option<String> = None;
    let mut hits: Vec<Value> = Vec::new();
    let mut indexing_complete;
    let mut budget = PageBudget::new(format!("search.results for operation {operation_id}"));
    loop {
        let mut input = json!({"operationId": operation_id});
        if let Some(cursor) = &cursor {
            input["page"] = json!({"cursor": cursor});
        }
        let result = match call_op(ctx, "search.results", &input) {
            Ok(result) => result,
            Err(code) => return code,
        };
        if let Some(items) = result.get("items").and_then(Value::as_array) {
            hits.extend(items.iter().cloned());
        }
        indexing_complete = result.get("indexingComplete").and_then(Value::as_bool) == Some(true);
        let (next, advanced) = page_cursor_step(
            cursor.as_deref(),
            result
                .pointer("/pageInfo/nextCursor")
                .and_then(Value::as_str),
        );
        let Some(next_cursor) = next else {
            break;
        };
        cursor = Some(next_cursor);
        if let Err(message) = budget.record(advanced, false) {
            eprintln!("{SHIM_NAME}: {message}");
            return EXIT_FAILED;
        }
    }

    // Old SessionSearchResult projection.
    let matches: Vec<Value> = hits.iter().map(project_match).collect();
    let result = json!({
        "query": keyword,
        "matches": matches,
        "truncated": !indexing_complete,
    });
    note_dropped("chat search", &["matches[].lineNumber", "sessionsScanned"]);
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_owned())
        );
    } else {
        print_text_result(&result);
    }
    EXIT_OK
}

/// Waits for the started search operation to reach a terminal state via
/// `operations.get` (bounded; the receipt's advisory `state` only skips the
/// polling when it is already terminal). Terminal-but-not-succeeded states
/// and a poll budget overrun are honest failures (exit 1) — never an empty
/// match list that fakes success.
fn wait_search_terminal(
    ctx: &mut EntryCtx<'_>,
    operation_id: &str,
    receipt_state: Option<&str>,
    max_polls: u32,
    interval: Duration,
) -> Result<(), i32> {
    crate::entry::wait_operation_terminal(
        ctx,
        "search",
        operation_id,
        receipt_state,
        max_polls,
        interval,
    )
    .map(|_| ())
}

/// Projects one `SearchHit` onto the old match shape: `sessionTitle` and
/// `createdAt` are not part of the new hit and are reported as dropped
/// (module doc); `text` and `snippet` both carry the excerpt (the new hit
/// has no separate full-text field).
fn project_match(hit: &Value) -> Value {
    json!({
        "sessionId": hit.get("sessionId"),
        "projectKey": hit.get("projectId"),
        "role": hit.get("role"),
        "text": hit.get("excerpt"),
        "snippet": hit.get("excerpt"),
    })
}

/// English text rendering following the old format semantics (header,
/// numbered matches with `> snippet` lines, truncation notice).
fn print_text_result(result: &Value) {
    let query = result.get("query").and_then(Value::as_str).unwrap_or("");
    let matches = result
        .get("matches")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    println!("Search results for: {query}");
    if matches.is_empty() {
        println!("No matches found.");
        return;
    }
    for (index, m) in matches.iter().enumerate() {
        let session = m.get("sessionId").and_then(Value::as_str).unwrap_or("");
        let short_id: String = session
            .chars()
            .rev()
            .take(8)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        println!(
            "{}. {} (`{}`) · {}",
            index + 1,
            m.get("role").and_then(Value::as_str).unwrap_or(""),
            short_id,
            m.get("projectKey").and_then(Value::as_str).unwrap_or(""),
        );
        if let Some(snippet) = m.get("snippet").and_then(Value::as_str) {
            println!("   > {snippet}");
        }
    }
    if result.get("truncated").and_then(Value::as_bool) == Some(true) {
        println!("Results may be incomplete (indexing was not complete).");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryCtx;
    use crate::executor::{ErrorPayload, FakeExecutor, Outcome, Response};
    use crate::state::{SessionMap, SessionMapping};
    use crate::testutil::TempDir;

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

    fn failed(code: &str) -> Result<Response, crate::executor::ExecError> {
        Ok(Response {
            version: None,
            request_id: None,
            outcome: Outcome::Failed,
            result: None,
            error: Some(ErrorPayload {
                code: code.to_owned(),
                message: "not served".to_owned(),
                details: None,
            }),
        })
    }

    fn ctx<'a>(exec: &'a mut FakeExecutor, home: &TempDir) -> EntryCtx<'a> {
        EntryCtx::with_home(exec, home.path())
    }

    #[test]
    fn parse_keyword_and_filters() {
        let args = parse(&argv(&[
            "search",
            "hello",
            "--project",
            "p",
            "--limit",
            "10",
            "--json",
            "--regex",
            "--case-sensitive",
            "--role",
            "user",
            "--session",
            "s",
            "--pilot-home",
            "/tmp/x",
        ]))
        .unwrap();
        assert_eq!(args.keyword.as_deref(), Some("hello"));
        assert_eq!(args.project.as_deref(), Some("p"));
        assert_eq!(args.limit, Some(10));
        assert!(args.json && args.regex && args.case_sensitive && !args.all_projects);
        assert_eq!(args.role.as_deref(), Some("user"));
        assert!(
            parse(&argv(&["search", "--all-projects"]))
                .unwrap()
                .all_projects
        );
    }

    #[test]
    fn parse_rejects_bad_shapes() {
        assert!(parse(&argv(&["find", "x"])).is_err());
        assert!(parse(&argv(&["search", "--role", "system"])).is_err());
        assert!(parse(&argv(&["search", "--limit", "x"])).is_err());
        assert!(parse(&argv(&["search", "a", "b"])).is_err());
        assert!(parse(&argv(&["search", "--bogus"])).is_err());
    }

    #[test]
    fn missing_keyword_is_a_usage_error() {
        let mut exec = FakeExecutor::new();
        let home = TempDir::new("search-keyword");
        assert_eq!(main(&mut ctx(&mut exec, &home), &argv(&["search"])), 2);
        assert!(exec.calls.is_empty());
    }

    #[test]
    fn pilot_home_is_rejected_as_unsupported_projection() {
        let mut exec = FakeExecutor::new();
        let home = TempDir::new("search-pilothome");
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&["search", "kw", "--pilot-home", "/old/root"]),
        );
        assert_eq!(code, EXIT_FAILED);
        assert!(exec.calls.is_empty());
    }

    #[test]
    fn scope_requires_project_or_all_projects_or_default() {
        let mut exec = FakeExecutor::new();
        let home = TempDir::new("search-scope");
        let code = main(&mut ctx(&mut exec, &home), &argv(&["search", "kw"]));
        assert_eq!(code, EXIT_FAILED);
        assert!(exec.calls.is_empty());

        // --all-projects selects the instance scope.
        let mut exec = FakeExecutor::new().push(failed("CAPABILITY_UNAVAILABLE"));
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&["search", "kw", "--all-projects"]),
        );
        assert_eq!(code, EXIT_FAILED);
        assert_eq!(exec.calls[0].input["scope"], json!({"kind": "instance"}));
    }

    #[test]
    fn session_flag_scopes_search_start_to_the_mapped_session() {
        // --session 限定 sessionId (mapping data chat-search.json): the
        // legacy id resolves through the old→new table into the contract's
        // session scope (projectId + sessionId both required).
        let home = TempDir::new("search-session");
        let mut map = SessionMap::default();
        map.record(
            home.path(),
            SessionMapping {
                legacy_session_id: "old-1".to_owned(),
                session_id: "new-1".to_owned(),
                project_id: "proj".to_owned(),
                created_at: "1".to_owned(),
            },
        )
        .unwrap();
        let mut exec = FakeExecutor::new()
            .push(ok(json!({
                "kind": "operation", "operationId": "op-6", "durable": true,
                "acceptedRevision": "rev-6", "deduplicated": false, "state": "succeeded",
            })))
            .push(ok(json!({
                "items": [],
                "pageInfo": {"nextCursor": null, "complete": true},
                "indexingComplete": true,
            })));
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&["search", "kw", "--session", "old-1"]),
        );
        assert_eq!(code, EXIT_OK);
        assert_eq!(
            exec.calls[0].input["scope"],
            json!({"kind": "session", "projectId": "proj", "sessionId": "new-1"})
        );

        // An unknown legacy id cannot be projected into a session scope:
        // explicit exit 2, never a silent fall-through to another scope.
        let mut exec = FakeExecutor::new();
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&["search", "kw", "--session", "nope"]),
        );
        assert_eq!(code, crate::EXIT_USAGE);
        assert!(exec.calls.is_empty());
    }

    #[test]
    fn search_flow_starts_pages_and_projects() {
        let mut exec = FakeExecutor::new()
            .push(ok(
                json!({"kind": "operation", "operationId": "op-1", "durable": true,
                       "acceptedRevision": "rev-1", "deduplicated": false,
                       "state": "queued"}),
            ))
            .push(ok(json!({
                "operation": {
                    "operationId": "op-1", "operationType": "search",
                    "scope": {"kind": "instance"}, "state": "succeeded",
                    "revision": "rev-1", "cancelRequested": false,
                    "effectStatus": "known", "progress": [],
                },
            })))
            .push(ok(json!({
                "items": [
                    {"projectId": "p", "sessionId": "s1", "entryId": "e1",
                     "role": "user", "excerpt": "hello world", "historyRevision": "r"},
                ],
                "pageInfo": {"nextCursor": "c1", "complete": false},
                "indexingComplete": true,
            })))
            .push(ok(json!({
                "items": [],
                "pageInfo": {"nextCursor": null, "complete": true},
                "indexingComplete": true,
            })));
        let home = TempDir::new("search-flow");
        crate::config::save(
            home.path(),
            &crate::config::CompatConfig {
                default_project_id: Some("proj".to_owned()),
                ..crate::config::CompatConfig::default()
            },
        )
        .unwrap();
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&["search", "hello", "--regex", "--role", "user"]),
        );
        assert_eq!(code, EXIT_OK);
        assert_eq!(
            exec.methods(),
            [
                "search.start",
                "operations.get",
                "search.results",
                "search.results"
            ]
        );
        let start = &exec.calls[0].input;
        assert_eq!(
            start["scope"],
            json!({"kind": "project", "projectId": "proj"})
        );
        assert_eq!(start["syntax"], "regex");
        assert_eq!(start["roles"], json!(["user"]));
        assert!(exec.calls[0].request_key.is_some());
        assert_eq!(exec.calls[1].input["operationId"], "op-1");
        assert_eq!(exec.calls[2].input["operationId"], "op-1");
        assert_eq!(exec.calls[3].input["page"]["cursor"], "c1");
    }

    #[test]
    fn search_polls_the_async_operation_to_a_terminal_state() {
        // queued → running → succeeded: `search.results` is only touched
        // after the terminal state (a non-terminal operation answers
        // OPERATION_NOT_READY).
        let mut exec = FakeExecutor::new()
            .push(ok(
                json!({"kind": "operation", "operationId": "op-2", "durable": true,
                       "acceptedRevision": "rev-1", "deduplicated": false,
                       "state": "queued"}),
            ))
            .push(ok(operation_get("op-2", "running")))
            .push(ok(operation_get("op-2", "succeeded")))
            .push(ok(json!({
                "items": [{"projectId": "p", "sessionId": "s1", "entryId": "e1",
                           "role": "assistant", "excerpt": "hit", "historyRevision": "r"}],
                "pageInfo": {"nextCursor": null, "complete": true},
                "indexingComplete": true,
            })));
        let home = TempDir::new("search-async");
        crate::config::save(
            home.path(),
            &crate::config::CompatConfig {
                default_project_id: Some("proj".to_owned()),
                ..crate::config::CompatConfig::default()
            },
        )
        .unwrap();
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&["search", "hit", "--json", "--project", "proj"]),
        );
        assert_eq!(code, EXIT_OK);
        assert_eq!(
            exec.methods(),
            [
                "search.start",
                "operations.get",
                "operations.get",
                "search.results"
            ]
        );
        assert_eq!(exec.calls[1].input["operationId"], "op-2");
        assert_eq!(exec.calls[2].input["operationId"], "op-2");
        assert_eq!(exec.calls[3].input["operationId"], "op-2");
    }

    #[test]
    fn failed_search_operation_is_reported_not_projected_onto_empty() {
        let mut exec = FakeExecutor::new()
            .push(ok(
                json!({"kind": "operation", "operationId": "op-3", "durable": true,
                       "acceptedRevision": "rev-1", "deduplicated": false,
                       "state": "queued"}),
            ))
            .push(ok(json!({
                "operation": {
                    "operationId": "op-3", "operationType": "search",
                    "scope": {"kind": "instance"}, "state": "failed",
                    "revision": "rev-1", "cancelRequested": false,
                    "effectStatus": "known", "progress": [],
                    "error": {"code": "INDEX_UNAVAILABLE", "message": "index rebuilding"},
                },
            })));
        let home = TempDir::new("search-failed");
        crate::config::save(
            home.path(),
            &crate::config::CompatConfig {
                default_project_id: Some("proj".to_owned()),
                ..crate::config::CompatConfig::default()
            },
        )
        .unwrap();
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&["search", "kw", "--project", "proj"]),
        );
        assert_eq!(code, EXIT_FAILED);
        assert_eq!(
            exec.methods(),
            ["search.start", "operations.get"],
            "no search.results after a failed operation"
        );
    }

    #[test]
    fn wait_search_terminal_is_bounded_and_reports_a_missing_state() {
        let mut exec = FakeExecutor::new()
            .push(ok(operation_get("op-4", "running")))
            .push(ok(operation_get("op-4", "running")));
        let home = TempDir::new("search-bound");
        let mut context = ctx(&mut exec, &home);
        // Budget overrun: honest failure instead of an infinite poll.
        let code = wait_search_terminal(
            &mut context,
            "op-4",
            Some("queued"),
            2,
            Duration::from_millis(1),
        );
        assert_eq!(code, Err(EXIT_FAILED));
        assert_eq!(exec.methods(), ["operations.get", "operations.get"]);

        // A succeeded response without `operation.state` breaks the
        // contract — fail fast instead of spinning until the budget.
        let mut exec = FakeExecutor::new().push(ok(json!({"unrelated": true})));
        let mut context = ctx(&mut exec, &home);
        let code = wait_search_terminal(
            &mut context,
            "op-5",
            Some("queued"),
            100,
            Duration::from_millis(1),
        );
        assert_eq!(code, Err(EXIT_FAILED));
        assert_eq!(exec.methods(), ["operations.get"]);
    }

    #[test]
    fn search_results_paging_is_bounded_against_a_stuck_cursor() {
        // A target that keeps answering the same nextCursor re-delivers the
        // same page forever; the shared page budget turns that into an
        // honest exit 1 after PAGE_STAGNATION_LIMIT stagnant pages instead
        // of a hang. The receipt is already terminal so the operation poll
        // is skipped; the first results page advances the cursor
        // (None → "c1"), every later page repeats it.
        let home = TempDir::new("search-results-stuck");
        crate::config::save(
            home.path(),
            &crate::config::CompatConfig {
                default_project_id: Some("proj".to_owned()),
                ..crate::config::CompatConfig::default()
            },
        )
        .unwrap();
        let mut exec = FakeExecutor::new().push(ok(json!({
            "kind": "operation", "operationId": "op-9", "durable": true,
            "acceptedRevision": "rev-1", "deduplicated": false, "state": "succeeded",
        })));
        for _ in 0..(crate::entry::PAGE_STAGNATION_LIMIT + 5) {
            exec = exec.push(ok(json!({
                "items": [
                    {"projectId": "p", "sessionId": "s1", "entryId": "e1",
                     "role": "user", "excerpt": "hit", "historyRevision": "r"},
                ],
                "pageInfo": {"nextCursor": "c1", "complete": false},
                "indexingComplete": true,
            })));
        }
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&["search", "kw", "--project", "proj"]),
        );
        assert_eq!(code, EXIT_FAILED);
        let results_calls = exec
            .calls
            .iter()
            .filter(|c| c.method == "search.results")
            .count() as u32;
        assert_eq!(
            results_calls,
            crate::entry::PAGE_STAGNATION_LIMIT + 1,
            "1 advancing first page + PAGE_STAGNATION_LIMIT stagnant pages"
        );
    }

    /// A minimal `operations.get` result document.
    fn operation_get(operation_id: &str, state: &str) -> Value {
        json!({
            "operation": {
                "operationId": operation_id, "operationType": "search",
                "scope": {"kind": "instance"}, "state": state,
                "revision": "rev-1", "cancelRequested": false,
                "effectStatus": "known", "progress": [],
            },
        })
    }
}
