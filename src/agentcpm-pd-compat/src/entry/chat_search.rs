//! `chat-search` entry (`cli.compat.chat-search`, adapted): `pilotdeck chat
//! search <keyword> [filters]`.
//!
//! `search.start`/`search.results` are pending S3 (`// TODO-S3` markers):
//! the orchestration is complete and failures (today typically
//! `CAPABILITY_UNAVAILABLE`) pass through verbatim. The scope is explicit
//! (`--all-projects` → instance; `--project` → that project; otherwise the
//! configured `defaultProjectId`; none of these → `configuration_required`)
//! — the old cwd default is not inherited. `--pilot-home` (direct scanning
//! of the legacy data root) is not projectable and fails with
//! `unsupported_projection` instead of silently searching somewhere else.
//! `matches[].lineNumber` (physical JSONL line numbers) and
//! `sessionsScanned` have no new equivalent and are listed as not
//! projectable.

use serde_json::{Value, json};

use crate::entry::{EntryCtx, call_op, call_op_keyed, note_dropped, parse_cli, usage_errors};
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

    // Explicit scope: all projects → instance; --project → that project;
    // otherwise the configured default project.
    let scope = if args.all_projects {
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
                     --project or --all-projects, or set defaultProjectId in \
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

    // TODO-S3: search.start is not delivered yet.
    let request_key = state::generate_request_key();
    let receipt = match call_op_keyed(ctx, "search.start", &start_input, &request_key) {
        Ok(receipt) => receipt,
        Err(code) => return code,
    };
    let Some(operation_id) = receipt
        .pointer("/identity/operationId")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        eprintln!("{SHIM_NAME}: search.start receipt carries no operationId");
        return EXIT_FAILED;
    };

    // Page the results to exhaustion.
    // TODO-S3: search.results is not delivered yet.
    let mut cursor: Option<String> = None;
    let mut hits: Vec<Value> = Vec::new();
    let mut indexing_complete;
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
        let next = result
            .pointer("/pageInfo/nextCursor")
            .and_then(Value::as_str)
            .map(str::to_owned);
        match next {
            Some(next_cursor) => cursor = Some(next_cursor),
            None => break,
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
    fn search_flow_starts_pages_and_projects() {
        let mut exec = FakeExecutor::new()
            .push(ok(
                json!({"identity": {"instanceId": "i", "operationId": "op-1"}}),
            ))
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
            ["search.start", "search.results", "search.results"]
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
        assert_eq!(exec.calls[2].input["page"]["cursor"], "c1");
    }
}
