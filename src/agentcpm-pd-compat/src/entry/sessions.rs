//! `sessions` entries (`cli.compat.sessions-list` supported,
//! `cli.compat.sessions-messages` adapted): `pilotdeck sessions [list]
//! [--project <key>]` and `pilotdeck sessions messages <session-id>
//! [--project <key>]`.
//!
//! - `list`: `session.list` paged to exhaustion (cursor loop) and
//!   aggregated, then projected onto the old `PilotDeckSessionInfo[]` array
//!   (2-space indent). A half page is never reported as complete success: a
//!   paging failure surfaces as exit 1. `sessionId` is the user-facing
//!   legacy id (reverse-mapped through the old→new table when known),
//!   `sessionKey` carries the new session identity; the old open-Record
//!   fields have no new equivalent and are listed as not projectable.
//! - `messages`: the legacy session id resolves through the mapping table
//!   (unknown → explicit failure), then `history.read` is paged
//!   (`fromEntryId`/pageInfo) and projected; history entries carrying
//!   non-text content parts (attachments/forks) cannot be downgraded and
//!   fail with `unsupported_projection` naming the entries instead of
//!   silently dropping data.

use serde_json::{Value, json};

use crate::entry::run::{legacy_flags, resolve_target_or_fail};
use crate::entry::{EntryCtx, call_op, note_dropped, parse_cli, usage_errors};
use crate::{EXIT_FAILED, EXIT_OK, SHIM_NAME};

/// Parsed `sessions` subcommand.
#[derive(Clone, Debug)]
pub enum SessionsArgs {
    /// `sessions [list]`
    List {
        project: Option<String>,
        flags: crate::entry::Parsed,
    },
    /// `sessions messages <session-id>`
    Messages {
        session_id: String,
        project: Option<String>,
        flags: crate::entry::Parsed,
    },
}

pub fn parse(rest: &[String]) -> Result<SessionsArgs, String> {
    // The old entries accepted connection options; only the target-relevant
    // subset is kept (--gateway-url/--auth-token participate in target
    // resolution, --project scopes the listing).
    let spec = crate::entry::OptSpec {
        values: &["--project", "--gateway-url", "--auth-token"],
        flags: &[],
    };
    let Some(first) = rest.first().map(String::as_str) else {
        return Ok(SessionsArgs::List {
            project: None,
            flags: crate::entry::Parsed::default(),
        });
    };
    match first {
        "list" => {
            let parsed = parse_cli(&rest[1..], &spec)?;
            if !parsed.positionals.is_empty() {
                return Err("sessions list takes no positional arguments".to_owned());
            }
            Ok(SessionsArgs::List {
                project: parsed.value("--project").map(str::to_owned),
                flags: parsed,
            })
        }
        "messages" => {
            let parsed = parse_cli(&rest[1..], &spec)?;
            let [session_id] = parsed.positionals.as_slice() else {
                return Err("sessions messages requires exactly one <session-id>".to_owned());
            };
            Ok(SessionsArgs::Messages {
                session_id: session_id.clone(),
                project: parsed.value("--project").map(str::to_owned),
                flags: parsed,
            })
        }
        other => Err(format!("unknown sessions subcommand `{other}`")),
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
    // Target resolution (the old entries accepted connection options).
    let parsed_flags = match &args {
        SessionsArgs::List { flags, .. } | SessionsArgs::Messages { flags, .. } => flags,
    };
    let flags = legacy_flags(parsed_flags);
    let target = match resolve_target_or_fail(&config, &flags) {
        Ok(target) => target,
        Err(code) => return code,
    };
    ctx.executor().set_target(target);

    match args {
        SessionsArgs::List { project, .. } => list(ctx, &home, &config, project),
        SessionsArgs::Messages { session_id, .. } => messages(ctx, &home, &session_id),
    }
}

/// Resolves the project scope: legacy `--project` or the configured
/// default; a missing project is `configuration_required` (the new op
/// requires a project — the shim never guesses).
fn resolve_project(
    config: &crate::config::CompatConfig,
    project: &Option<String>,
) -> Result<String, i32> {
    if let Some(project) = project {
        return Ok(project.clone());
    }
    match &config.default_project_id {
        Some(project) => Ok(project.clone()),
        None => {
            eprintln!(
                "{SHIM_NAME}: no project scope for the listing (error: configuration_required); \
                 pass --project or set defaultProjectId in \
                 $AGENTCPM_PD_HOME/compat/config.json"
            );
            Err(EXIT_FAILED)
        }
    }
}

fn list(
    ctx: &mut EntryCtx<'_>,
    home: &std::path::Path,
    config: &crate::config::CompatConfig,
    project: Option<String>,
) -> i32 {
    let project_id = match resolve_project(config, &project) {
        Ok(project) => project,
        Err(code) => return code,
    };

    // Page session.list to exhaustion.
    let mut cursor: Option<String> = None;
    let mut items: Vec<Value> = Vec::new();
    loop {
        let mut input = json!({"projectId": project_id, "page": {"limit": 100}});
        if let Some(cursor) = &cursor {
            input["page"]["cursor"] = json!(cursor);
        }
        let result = match call_op(ctx, "session.list", &input) {
            Ok(result) => result,
            Err(code) => return code,
        };
        if let Some(page_items) = result.get("items").and_then(Value::as_array) {
            items.extend(page_items.iter().cloned());
        }
        let next = result
            .pointer("/pageInfo/nextCursor")
            .and_then(Value::as_str)
            .map(str::to_owned);
        match next {
            Some(next_cursor) => cursor = Some(next_cursor),
            None => break,
        }
    }

    // Reverse-map new session ids to the user-facing legacy ids.
    let sessions = ctx.load_sessions(home).ok();
    let projected: Vec<Value> = items
        .iter()
        .map(|item| {
            let new_id = item.get("sessionId").and_then(Value::as_str).unwrap_or("");
            let user_facing = sessions
                .as_ref()
                .and_then(|m| m.legacy_for(new_id))
                .unwrap_or(new_id);
            json!({"sessionId": user_facing, "sessionKey": new_id})
        })
        .collect();
    note_dropped(
        "sessions list item",
        &["open-Record fields beyond sessionId/sessionKey"],
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&json!(projected)).unwrap_or_else(|_| "[]".to_owned())
    );
    EXIT_OK
}

fn messages(ctx: &mut EntryCtx<'_>, home: &std::path::Path, session_id: &str) -> i32 {
    // Legacy session id must resolve through the mapping table.
    let sessions = match ctx.load_sessions(home) {
        Ok(sessions) => sessions,
        Err(code) => return code,
    };
    let Some(mapping) = sessions.find(session_id) else {
        eprintln!(
            "{SHIM_NAME}: unknown session id `{session_id}` (error: NOT_FOUND); \
             ids printed by this shim are recorded under \
             $AGENTCPM_PD_HOME/compat/sessions.json"
        );
        return EXIT_FAILED;
    };
    let new_session_id = mapping.session_id.clone();

    // Page history.read to exhaustion.
    // TODO-S3: history.read is not delivered yet; the calls below surface
    // CAPABILITY_UNAVAILABLE from today's targets faithfully.
    let mut from: Option<String> = None;
    let mut entries: Vec<Value> = Vec::new();
    loop {
        let mut input = json!({"sessionId": new_session_id, "page": {"limit": 100}});
        if let Some(from) = &from {
            input["fromEntryId"] = json!(from);
        }
        let result = match call_op(ctx, "history.read", &input) {
            Ok(result) => result,
            Err(code) => return code,
        };
        if let Some(page_items) = result.get("items").and_then(Value::as_array) {
            entries.extend(page_items.iter().cloned());
        }
        let next = result
            .pointer("/pageInfo/nextCursor")
            .and_then(Value::as_str)
            .map(str::to_owned);
        match next {
            Some(next_cursor) => from = Some(next_cursor),
            None => break,
        }
    }

    // Project history entries onto the old open-Record message shape.
    let mut projected: Vec<Value> = Vec::new();
    let mut unprojectable: Vec<String> = Vec::new();
    for entry in &entries {
        let mut message = json!({
            "id": entry.get("entryId").cloned().unwrap_or(Value::Null),
            "role": entry.get("role").cloned().unwrap_or(Value::Null),
            "createdAt": entry.get("createdAt").cloned().unwrap_or(Value::Null),
        });
        if let Some(run_id) = entry.get("runId").filter(|v| !v.is_null()) {
            message["runId"] = run_id.clone();
        }
        let mut text = String::new();
        let mut has_unprojectable = false;
        if let Some(parts) = entry.get("content").and_then(Value::as_array) {
            for part in parts {
                match part.get("kind").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(chunk) = part.get("text").and_then(Value::as_str) {
                            text.push_str(chunk);
                        }
                    }
                    _ => has_unprojectable = true,
                }
            }
        }
        if has_unprojectable {
            unprojectable.push(
                entry
                    .get("entryId")
                    .and_then(Value::as_str)
                    .unwrap_or("<unknown entry>")
                    .to_owned(),
            );
            continue;
        }
        message["text"] = json!(text);
        projected.push(message);
    }
    if !unprojectable.is_empty() {
        // No silent data loss: name the entries that cannot be downgraded.
        eprintln!(
            "{SHIM_NAME}: history entries with non-text content parts cannot be projected \
             (error: unsupported_projection): {}",
            unprojectable.join(", ")
        );
        return EXIT_FAILED;
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!(projected)).unwrap_or_else(|_| "[]".to_owned())
    );
    EXIT_OK
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

    #[test]
    fn parse_handles_list_messages_and_defaults() {
        assert!(matches!(
            parse(&argv(&[])).unwrap(),
            SessionsArgs::List { project: None, .. }
        ));
        assert!(matches!(
            parse(&argv(&["list", "--project", "k"])).unwrap(),
            SessionsArgs::List { project: Some(ref p), .. } if p == "k"
        ));
        let SessionsArgs::Messages {
            session_id,
            project,
            ..
        } = parse(&argv(&["messages", "s9", "--project", "k"])).unwrap()
        else {
            panic!("expected messages");
        };
        assert_eq!(session_id, "s9");
        assert_eq!(project.as_deref(), Some("k"));
        assert!(parse(&argv(&["frob"])).is_err());
        assert!(parse(&argv(&["messages"])).is_err());
        // Connection options are accepted (target resolution consumes them).
        assert!(parse(&argv(&["list", "--gateway-url", "ws://x"])).is_ok());
    }

    #[test]
    fn list_paginates_and_projects_the_old_array() {
        let home = TempDir::new("sessions-list");
        // A recorded session lets the reverse mapping resolve the user id.
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
        crate::config::save(
            home.path(),
            &crate::config::CompatConfig {
                default_project_id: Some("proj".to_owned()),
                ..crate::config::CompatConfig::default()
            },
        )
        .unwrap();

        let mut exec = FakeExecutor::new()
            .push(ok(json!({
                "items": [
                    {"sessionId": "new-1", "projectId": "proj"},
                    {"sessionId": "new-2", "projectId": "proj"},
                ],
                "pageInfo": {"snapshotRevision": "r", "nextCursor": "c1", "complete": false},
            })))
            .push(ok(json!({
                "items": [{"sessionId": "new-3", "projectId": "proj"}],
                "pageInfo": {"snapshotRevision": "r", "nextCursor": null, "complete": true},
            })));
        let code = main(&mut ctx(&mut exec, &home), &argv(&["list"]));
        assert_eq!(code, EXIT_OK);
        assert_eq!(exec.methods(), ["session.list", "session.list"]);
        // Second page used the cursor from the first.
        assert_eq!(exec.calls[1].input["page"]["cursor"], "c1");
        assert_eq!(exec.calls[0].input["projectId"], "proj");
    }

    #[test]
    fn list_without_a_project_scope_is_configuration_required() {
        let mut exec = FakeExecutor::new();
        let home = TempDir::new("sessions-noproject");
        let code = main(&mut ctx(&mut exec, &home), &argv(&["list"]));
        assert_eq!(code, EXIT_FAILED);
        assert!(exec.calls.is_empty());
    }

    #[test]
    fn messages_requires_a_mapped_session_and_reads_history() {
        let home = TempDir::new("sessions-messages");
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

        // Unknown legacy id: explicit failure, no business call.
        let mut exec = FakeExecutor::new();
        let code = main(&mut ctx(&mut exec, &home), &argv(&["messages", "nope"]));
        assert_eq!(code, EXIT_FAILED);
        assert!(exec.calls.is_empty());

        // Mapped id: history.read is paged with fromEntryId.
        let mut exec = FakeExecutor::new()
            .push(ok(json!({
                "items": [
                    {"entryId": "e1", "role": "user", "createdAt": "t1",
                     "content": [{"kind": "text", "text": "hello"}]},
                    {"entryId": "e2", "role": "assistant", "createdAt": "t2",
                     "content": [{"kind": "text", "text": "hi"}], "runId": "run-1"},
                ],
                "pageInfo": {"nextCursor": "c9", "complete": false},
            })))
            .push(ok(json!({
                "items": [],
                "pageInfo": {"nextCursor": null, "complete": true},
            })));
        let code = main(&mut ctx(&mut exec, &home), &argv(&["messages", "old-1"]));
        assert_eq!(code, EXIT_OK);
        assert_eq!(exec.methods(), ["history.read", "history.read"]);
        assert_eq!(exec.calls[0].input["sessionId"], "new-1");
        assert_eq!(exec.calls[1].input["fromEntryId"], "c9");
    }

    #[test]
    fn messages_with_attachments_fails_unsupported_projection() {
        let home = TempDir::new("sessions-attachments");
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
        let mut exec = FakeExecutor::new().push(ok(json!({
            "items": [
                {"entryId": "e1", "role": "user", "createdAt": "t1",
                 "content": [{"kind": "text", "text": "ok"}]},
                {"entryId": "e2", "role": "user", "createdAt": "t2",
                 "content": [
                    {"kind": "text", "text": "see"},
                    {"kind": "attachment", "artifactId": "a1"},
                 ]},
            ],
            "pageInfo": {"nextCursor": null, "complete": true},
        })));
        let code = main(&mut ctx(&mut exec, &home), &argv(&["messages", "old-1"]));
        assert_eq!(code, EXIT_FAILED);
        assert_eq!(exec.methods(), ["history.read"]);
    }

    #[test]
    fn main_usage_errors_exit_2() {
        let mut exec = FakeExecutor::new();
        let home = TempDir::new("sessions-usage");
        assert_eq!(main(&mut ctx(&mut exec, &home), &argv(&["bogus"])), 2);
    }
}
