//! `sdk-run` entry (`cli.compat.sdk-run`, supported): `pilotdeck run
//! <prompt> [--json] [--stream-json] [SDK options]`; without a prompt the
//! prompt text is read from stdin.
//!
//! Orchestration (mapping sequence): `session.create` → `run.submit` (the
//! shim generates and persists the request key BEFORE the call, so a lost
//! response is retried with the same key and never double-submits) →
//! `run.events` (cursor-paged wait for the real terminal state) →
//! `run.cancel` (only for legacy-compatible SIGINT/SIGTERM, performed by the
//! SIGINT-immune guardian process, see [`crate::entry::guardian`]).
//!
//! Old `PilotDeckResult` projection: `succeeded`→`completed{output}`,
//! `failed`→`failed{error: outcomeCode}`, `cancelled`/`interrupted`→
//! `aborted{reason}`, `result_unknown`→`result_unknown{recovery}`.
//! `usage`/`artifacts`/`finishReason` are legacy Gateway-private shapes and
//! are reported as not projectable (stderr note) instead of faked.

use std::io::IsTerminal;
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::{CompatConfig, LegacyFlags};
use crate::entry::{EntryCtx, call_op, note_dropped, parse_cli, usage_errors};
use crate::executor::ExecError;
use crate::state::{self, RunRecord, SessionMapping};
use crate::{EXIT_FAILED, EXIT_OK, EXIT_RESULT_UNKNOWN, SHIM_NAME};

/// Shared legacy SDK connection option names (the `context` entry's set).
pub(crate) const SDK_OPTION_NAMES: &[&str] = &[
    "--gateway-url",
    "--auth-token",
    "--project",
    "--channel",
    "--model",
    "--permission-mode",
];

/// Parsed `run`/`resume` arguments (both entries share the SDK option set).
#[derive(Clone, Debug, Default)]
pub struct RunArgs {
    /// Prompt positional; `None` means "read stdin".
    pub prompt: Option<String>,
    pub json: bool,
    pub stream_json: bool,
    /// Shared SDK connection options (crate-internal helper type).
    pub(crate) sdk: crate::entry::Parsed,
}

pub(crate) fn parse(rest: &[String]) -> Result<RunArgs, String> {
    let spec = crate::entry::OptSpec {
        values: SDK_OPTION_NAMES,
        flags: &["--json", "--stream-json"],
    };
    let parsed = parse_cli(rest, &spec)?;
    if parsed.positionals.len() > 1 {
        return Err("run takes at most one <prompt> argument".to_owned());
    }
    // Usage errors must precede any business write, so the model format is
    // validated during parsing.
    if let Some(raw) = parsed.value("--model")
        && parse_model_value(raw).is_none()
    {
        return Err("--model expects provider/model".to_owned());
    }
    Ok(RunArgs {
        prompt: parsed.positionals.first().cloned(),
        json: parsed.flag("--json"),
        stream_json: parsed.flag("--stream-json"),
        sdk: parsed,
    })
}

/// Legacy endpoint flags relevant for target resolution.
pub(crate) fn legacy_flags(sdk: &crate::entry::Parsed) -> LegacyFlags {
    LegacyFlags {
        gateway_url: sdk.value("--gateway-url").map(str::to_owned),
        project: sdk.value("--project").map(str::to_owned),
        channel: sdk.value("--channel").map(str::to_owned),
    }
}

/// Resolves the explicit target profile for the invocation (`Ok(None)` =
/// the CLI's own default under `AGENTCPM_PD_HOME`). Unmatched legacy flags
/// are the `context` entry's `configuration_required`: the shim never
/// guesses a target and never falls back to legacy 18789 probing.
pub(crate) fn resolve_target_or_fail(
    config: &CompatConfig,
    flags: &LegacyFlags,
) -> Result<Option<String>, i32> {
    match config.resolve_target(flags) {
        Ok(target) => Ok(target),
        Err(crate::config::NoLegacyMatch) => {
            eprintln!(
                "{SHIM_NAME}: no explicit target mapping matches the provided legacy options \
                 (error: configuration_required); register one in legacyTargets under \
                 $AGENTCPM_PD_HOME/compat/config.json"
            );
            Err(EXIT_FAILED)
        }
    }
}

/// Resolves the prompt: positional, else non-interactive stdin (trimmed).
/// A missing prompt is a standard usage error (exit 2) per the mapping
/// data (the old CLI threw with exit 1; the shim aligns to the new
/// contract).
fn resolve_prompt(args: &RunArgs) -> Result<String, String> {
    if let Some(prompt) = &args.prompt {
        return Ok(prompt.clone());
    }
    if std::io::stdin().is_terminal() {
        return Err(
            "run requires a <prompt> argument (or a prompt on non-interactive stdin)".to_owned(),
        );
    }
    let mut buf = String::new();
    if let Err(e) = std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf) {
        return Err(format!("failed to read the prompt from stdin: {e}"));
    }
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        return Err(
            "run requires a <prompt> argument (or a prompt on non-interactive stdin)".to_owned(),
        );
    }
    Ok(trimmed.to_owned())
}

/// Entry point for `run`.
pub fn main(ctx: &mut EntryCtx<'_>, rest: &[String]) -> i32 {
    let args = match parse(rest) {
        Ok(args) => args,
        Err(message) => return usage_errors::usage(&message),
    };
    let prompt = match resolve_prompt(&args) {
        Ok(prompt) => prompt,
        Err(message) => return usage_errors::usage(&message),
    };
    run_prompt(ctx, &args, &prompt, None)
}

/// Shared submission flow for `run` (new session) and `resume` (mapped
/// session): resolve target → submit with a persisted request key → wait →
/// project the old output. `resume` passes the pre-resolved session ids
/// `(user-facing legacy id, new session id)`.
pub(crate) fn run_prompt(
    ctx: &mut EntryCtx<'_>,
    args: &RunArgs,
    prompt: &str,
    resumed: Option<(&str, &str)>,
) -> i32 {
    let home = match ctx.require_home() {
        Ok(home) => home.to_path_buf(),
        Err(code) => return code,
    };
    let config = match ctx.load_config(&home) {
        Ok(config) => config,
        Err(code) => return code,
    };
    let flags = legacy_flags(&args.sdk);
    if args.sdk.value("--auth-token").is_some() {
        // Never consumed, never stored, never logged.
        eprintln!(
            "{SHIM_NAME}: note: --auth-token is not consumed by the shim; \
             credentials live in the target profile"
        );
    }
    let target = match resolve_target_or_fail(&config, &flags) {
        Ok(target) => target,
        Err(code) => return code,
    };
    ctx.executor().set_target(target.clone());

    // Session: create fresh (`run`) or reuse the mapped one (`resume`).
    let (session_id, project_id, user_facing_id) = match resumed {
        Some((legacy_id, new_id)) => {
            let project = state::SessionMap::load(&home)
                .ok()
                .and_then(|map| {
                    map.sessions
                        .iter()
                        .find(|m| m.session_id == new_id)
                        .map(|m| m.project_id.clone())
                })
                .unwrap_or_else(|| state::generate_id("shim-p"));
            (new_id.to_owned(), project, legacy_id.to_owned())
        }
        None => {
            let session_id = state::generate_id("shim-s");
            let project_id = args
                .sdk
                .value("--project")
                .map(str::to_owned)
                .unwrap_or_else(|| state::generate_id("shim-p"));
            // `context` entry (mapping sequence): an explicit --project maps
            // onto a new projectId validated with `project.get`
            // (NOT_FOUND/authorization failures surface verbatim); generated
            // project ids are validated by session.create itself.
            if args.sdk.value("--project").is_some()
                && let Err(code) = call_op(ctx, "project.get", &json!({"projectId": project_id}))
            {
                return code;
            }
            let created = match call_op(
                ctx,
                "session.create",
                &json!({
                    "sessionId": session_id,
                    "projectId": project_id,
                }),
            ) {
                Ok(created) => created,
                Err(code) => return code,
            };
            // The session exists server-side; record the old→new mapping
            // (an identity row for shim-created sessions; the t020
            // migration can add true legacy ids to the same table).
            let mut sessions = match ctx.load_sessions(&home) {
                Ok(sessions) => sessions,
                Err(code) => return code,
            };
            if let Err(message) = sessions.record(
                &home,
                SessionMapping {
                    legacy_session_id: session_id.clone(),
                    session_id: created
                        .pointer("/session/sessionId")
                        .and_then(Value::as_str)
                        .unwrap_or(&session_id)
                        .to_owned(),
                    project_id: project_id.clone(),
                    created_at: unix_now(),
                },
            ) {
                eprintln!("{SHIM_NAME}: failed to persist the session mapping: {message}");
                return EXIT_FAILED;
            }
            (session_id.clone(), project_id, session_id)
        }
    };

    // Write-ahead request key: persisted before submit so a lost response
    // is retried with the same key (never double-submits) and the guardian
    // can recover the run.
    let request_key = state::generate_request_key();
    let mut record = RunRecord::new(
        request_key.clone(),
        session_id.clone(),
        project_id,
        prompt.to_owned(),
    );
    if let Err(message) = record.save(&home) {
        eprintln!("{SHIM_NAME}: failed to persist the run request key: {message}");
        return EXIT_FAILED;
    }

    // SIGINT guardian (production only): survives Ctrl+C because its shell
    // wrapper ignores INT/TERM; wakes when this process dies and submits
    // run.cancel for an unsettled record.
    let guardian = crate::entry::guardian::spawn_if_live(
        ctx.executor().is_live(),
        &record.path(&home),
        target.as_deref(),
    );

    let mut submit_input = json!({
        "sessionId": session_id,
        "input": [{"kind": "text", "text": prompt}],
    });
    if let Some(raw) = args.sdk.value("--model")
        && let Some(model) = parse_model_value(raw)
    {
        submit_input["model"] = model;
    }
    if let Some(mode) = args.sdk.value("--permission-mode") {
        // Old permission-mode strings map onto the permissionRevision
        // pointer per the mapping data; an unknown value is rejected by
        // the server and surfaced faithfully.
        submit_input["permissionRevision"] = json!(mode);
    }

    let run_id = match submit_with_retry(ctx, &submit_input, &request_key, &mut record, &home) {
        Ok(run_id) => run_id,
        Err(code) => return code,
    };
    // Keep the guardian handle alive until the end of the flow: dropping it
    // (here or at return) closes its stdin pipe, which is the wake signal.
    // A settled record tells it to exit quietly.
    let _ = guardian;

    // Wait for the real terminal state, streaming as we go.
    let text = match wait_for_terminal(ctx, &run_id, args.stream_json) {
        Ok(text) => text,
        Err(code) => return code,
    };

    let run = match call_op(ctx, "run.get", &json!({"runId": run_id})) {
        Ok(result) => result.get("run").cloned().unwrap_or(result),
        Err(code) => return code,
    };
    record.state = state::RUN_SETTLED.to_owned();
    record.terminal_state = run.get("state").and_then(Value::as_str).map(str::to_owned);
    let _ = record.save(&home);

    emit_output(args, &user_facing_id, &run, &text)
}

/// Parses `--model provider/model` into the contract's `{providerId, modelId}`.
pub(crate) fn parse_model_value(raw: &str) -> Option<Value> {
    let (provider, model) = raw.split_once('/')?;
    if provider.is_empty() || model.is_empty() {
        return None;
    }
    Some(json!({"providerId": provider, "modelId": model}))
}

/// Submits the run with the persisted key. A lost response (the child ran
/// but produced no parseable envelope — the write may have applied) is
/// retried ONCE with the same key; a second loss is `RESULT_UNKNOWN`
/// (exit 3), never a silent resubmission with a new key.
fn submit_with_retry(
    ctx: &mut EntryCtx<'_>,
    input: &Value,
    request_key: &str,
    record: &mut RunRecord,
    home: &std::path::Path,
) -> Result<String, i32> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match ctx
            .executor()
            .invoke_keyed("run.submit", input, request_key)
        {
            Ok(resp) => {
                let result = crate::entry::unwrap_result("run.submit", &resp)?;
                // Contract receipt (`$defs.receipt` oneOf kind="run" branch):
                // `runId` sits at the TOP LEVEL of the result, not under an
                // `identity` wrapper (e2e usage: e2e_v1_collections.rs
                // `submitted["runId"]`).
                let run_id = result
                    .get("runId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let Some(run_id) = run_id else {
                    eprintln!("{SHIM_NAME}: run.submit receipt carries no runId");
                    return Err(EXIT_FAILED);
                };
                record.state = state::RUN_RUNNING.to_owned();
                record.run_id = Some(run_id.clone());
                let _ = record.save(home);
                return Ok(run_id);
            }
            Err(e) if matches!(e, ExecError::Exit { .. } | ExecError::InvalidStdout { .. }) => {
                // Response lost; the persisted key makes a retry safe.
                if attempt == 1 {
                    eprintln!(
                        "{SHIM_NAME}: run.submit response lost; retrying once with the same request key {request_key}"
                    );
                    continue;
                }
                eprintln!("{SHIM_NAME}: run.submit failed: {e}");
                return Err(EXIT_RESULT_UNKNOWN);
            }
            Err(e) => {
                eprintln!("{SHIM_NAME}: run.submit failed: {e}");
                return Err(EXIT_FAILED);
            }
        }
    }
}

/// Waits for the run's real terminal state by paging `run.events`
/// (`terminal: true` on the frame). Gap frames restart the query (bounded);
/// assistant text increments accumulate for the plain/--json projections;
/// `--stream-json` prints one JSON line per event (event-level projection,
/// no byte-compatibility promise).
fn wait_for_terminal(
    ctx: &mut EntryCtx<'_>,
    run_id: &str,
    stream_json: bool,
) -> Result<String, i32> {
    let mut after: Option<String> = None;
    let mut text = String::new();
    let mut restarts = 0u32;
    loop {
        let mut input = json!({"runId": run_id});
        if let Some(cursor) = &after {
            input["afterCursor"] = json!(cursor);
        }
        let result = call_op(ctx, "run.events", &input)?;
        let Some(frame) = result.get("frame").filter(|f| f.is_object()) else {
            eprintln!("{SHIM_NAME}: run.events returned no frame object");
            return Err(EXIT_FAILED);
        };
        if frame.get("kind").and_then(Value::as_str) == Some("gap") {
            restarts += 1;
            if restarts > 3 {
                eprintln!("{SHIM_NAME}: run.events cursor repeatedly expired for run {run_id}");
                return Err(EXIT_FAILED);
            }
            eprintln!("{SHIM_NAME}: note: run.events cursor gap; restarting the event query");
            after = None;
            continue;
        }
        if let Some(events) = frame.get("events").and_then(Value::as_array) {
            for event in events {
                if stream_json && let Ok(line) = serde_json::to_string(event) {
                    println!("{line}");
                }
                text.push_str(&extract_text(event));
            }
        }
        if frame.get("terminal").and_then(Value::as_bool) == Some(true) {
            return Ok(text);
        }
        after = frame
            .get("nextCursor")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let caught_up = frame.get("caughtUp").and_then(Value::as_bool) != Some(false);
        if caught_up {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

/// Old plain-renderer text filter (sdkCli.ts:128-133): events whose type
/// mentions `assistant` (or a subagent message) contribute their text or
/// delta payload.
pub(crate) fn extract_text(event: &Value) -> String {
    let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
    if !event_type.contains("assistant") && !event_type.contains("subagent") {
        return String::new();
    }
    for holder in [event.get("payload"), event.get("data"), Some(event)] {
        let Some(holder) = holder else { continue };
        for field in ["text", "delta"] {
            if let Some(text) = holder.get(field).and_then(Value::as_str) {
                return text.to_owned();
            }
        }
    }
    String::new()
}

/// Projects the old `PilotDeckResult` shape from the new run state.
pub(crate) fn old_result(run: &Value, text: &str) -> Value {
    let state = run.get("state").and_then(Value::as_str).unwrap_or("");
    match state {
        "succeeded" => {
            let mut result = json!({"status": "completed"});
            if !text.is_empty() {
                result["output"] = json!(text);
            }
            result
        }
        "failed" => json!({
            "status": "failed",
            "error": run.get("outcomeCode").cloned().unwrap_or(Value::Null),
        }),
        "cancelled" | "interrupted" => json!({"status": "aborted", "reason": state}),
        _ => json!({
            "status": "result_unknown",
            "recovery": {
                "sessionId": run.get("sessionId").cloned().unwrap_or(Value::Null),
                "runId": run.get("runId").cloned().unwrap_or(Value::Null),
            },
        }),
    }
}

/// Exit code for an old-status projection: only `completed` succeeds.
pub(crate) fn exit_for_status(status: &str) -> i32 {
    if status == "completed" {
        EXIT_OK
    } else {
        EXIT_FAILED
    }
}

/// Emits the old CLI output shape (`--json` single compact line, plain text
/// semantics otherwise; `--stream-json` already printed its events).
pub(crate) fn emit_output(
    args: &RunArgs,
    user_facing_session: &str,
    run: &Value,
    text: &str,
) -> i32 {
    note_dropped("run result", &["usage", "artifacts", "finishReason"]);
    let result = old_result(run, text);
    let status = result["status"].as_str().unwrap_or("").to_owned();
    if args.json {
        // Old: JSON.stringify({sessionId, result}) — one compact line.
        let doc = json!({"sessionId": user_facing_session, "result": result});
        println!("{}", serde_json::to_string(&doc).unwrap_or_default());
        return exit_for_status(&status);
    }
    if args.stream_json {
        return exit_for_status(&status);
    }
    if status == "completed" && !text.is_empty() {
        // Old: completed with stream text printed just the stream text.
        println!("{text}");
    } else if status == "completed" {
        match result.get("output") {
            Some(Value::String(output)) => println!("{output}"),
            _ => println!("{}", serde_json::to_string(&result).unwrap_or_default()),
        }
    } else {
        println!("{}", serde_json::to_string(&result).unwrap_or_default());
    }
    exit_for_status(&status)
}

fn unix_now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryCtx;
    use crate::executor::{ErrorPayload, FakeExecutor, Outcome, Response};
    use crate::state::RUN_RUNNING;
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
    fn parse_accepts_prompt_options_and_stdin_mode() {
        let args = parse(&argv(&[
            "hello world",
            "--json",
            "--model",
            "openai/gpt-x",
            "--permission-mode=auto",
        ]))
        .unwrap();
        assert_eq!(args.prompt.as_deref(), Some("hello world"));
        assert!(args.json && !args.stream_json);
        assert_eq!(args.sdk.value("--model"), Some("openai/gpt-x"));
        assert_eq!(args.sdk.value("--permission-mode"), Some("auto"));

        let stdin_mode = parse(&argv(&["--stream-json"])).unwrap();
        assert_eq!(stdin_mode.prompt, None);
        assert!(stdin_mode.stream_json);
    }

    #[test]
    fn parse_rejects_unknown_options_and_extra_positionals() {
        assert!(parse(&argv(&["--bogus"])).is_err());
        assert!(parse(&argv(&["a", "b"])).is_err());
        assert!(parse(&argv(&["--json=1"])).is_err());
    }

    #[test]
    fn parse_model_splits_provider_and_model() {
        assert_eq!(
            parse_model_value("openai/gpt-4o"),
            Some(json!({"providerId": "openai", "modelId": "gpt-4o"}))
        );
        assert_eq!(parse_model_value("nonsense"), None);
        assert_eq!(parse_model_value("/x"), None);
        assert_eq!(parse_model_value("x/"), None);
    }

    #[test]
    fn old_result_maps_new_states_onto_the_legacy_shape() {
        let run = json!({"state": "succeeded", "sessionId": "s", "runId": "r"});
        assert_eq!(
            old_result(&run, "hello"),
            json!({"status": "completed", "output": "hello"})
        );
        let run = json!({"state": "failed", "outcomeCode": "E_TOOL"});
        assert_eq!(
            old_result(&run, ""),
            json!({"status": "failed", "error": "E_TOOL"})
        );
        let run = json!({"state": "cancelled"});
        assert_eq!(
            old_result(&run, ""),
            json!({"status": "aborted", "reason": "cancelled"})
        );
        let run = json!({"state": "result_unknown", "sessionId": "s", "runId": "r"});
        assert_eq!(
            old_result(&run, ""),
            json!({"status": "result_unknown", "recovery": {"sessionId": "s", "runId": "r"}})
        );
        assert_eq!(exit_for_status("completed"), EXIT_OK);
        assert_eq!(exit_for_status("failed"), EXIT_FAILED);
    }

    #[test]
    fn extract_text_follows_the_old_assistant_filter() {
        let event = json!({"type": "assistant_text_delta", "payload": {"delta": "hi"}});
        assert_eq!(extract_text(&event), "hi");
        let event = json!({"type": "assistant.message", "data": {"text": "yo"}});
        assert_eq!(extract_text(&event), "yo");
        let event = json!({"type": "assistant_text_delta", "text": "top"});
        assert_eq!(extract_text(&event), "top");
        let event = json!({"type": "tool_started", "payload": {"text": "nope"}});
        assert_eq!(extract_text(&event), "");
    }

    #[test]
    fn main_reports_usage_errors_exit_2() {
        let mut exec = FakeExecutor::new();
        let home = TempDir::new("run-usage");
        assert_eq!(main(&mut ctx(&mut exec, &home), &argv(&["--bogus"])), 2);
        assert_eq!(
            main(&mut ctx(&mut exec, &home), &argv(&["--model", "x"])),
            2
        );
        assert!(exec.calls.is_empty(), "usage errors precede business calls");
    }

    #[test]
    fn orchestration_creates_session_submits_with_key_and_awaits_terminal() {
        let mut exec = FakeExecutor::new()
            .push(ok(
                json!({"session": {"sessionId": "shim-s-1", "sessionState": "open"}}),
            ))
            .push(ok(
                json!({"kind": "run", "runId": "run-9", "durable": true,
                       "acceptedRevision": "rev-1", "deduplicated": false,
                       "state": "queued"}),
            ))
            .push(ok(json!({"frame": {
                "events": [
                    {"type": "assistant_text_delta", "payload": {"delta": "hello "}},
                    {"type": "assistant_text_delta", "payload": {"delta": "world"}},
                    {"type": "tool_started", "payload": {"name": "bash"}},
                ],
                "caughtUp": true, "terminal": false, "nextCursor": "c1",
            }})))
            .push(ok(json!({"frame": {
                "events": [],
                "caughtUp": true, "terminal": true,
            }})))
            .push(ok(
                json!({"runId": "run-9", "state": "succeeded", "sessionId": "shim-s-1"}),
            ));
        let home = TempDir::new("run-flow");
        let code = main(&mut ctx(&mut exec, &home), &argv(&["hi there"]));
        assert_eq!(code, EXIT_OK, "completed run exits 0");
        assert_eq!(
            exec.methods(),
            [
                "session.create",
                "run.submit",
                "run.events",
                "run.events",
                "run.get"
            ]
        );
        // Submit carries the prompt and a persisted request key; the
        // session id is the shim-generated one from session.create.
        let create = &exec.calls[0];
        let generated_session = create.input["sessionId"].as_str().unwrap().to_owned();
        let submit = &exec.calls[1];
        assert!(submit.request_key.is_some());
        assert_eq!(submit.input["input"][0]["text"], "hi there");
        assert_eq!(submit.input["sessionId"], generated_session);
        let record = state::RunRecord::load(home.path(), submit.request_key.as_deref().unwrap())
            .unwrap()
            .expect("record persisted");
        assert_eq!(record.state, state::RUN_SETTLED);
        assert_eq!(record.session_id, generated_session);
        assert_eq!(record.run_id.as_deref(), Some("run-9"));
        // Session mapping recorded (identity row).
        let map = state::SessionMap::load(home.path()).unwrap();
        assert!(map.find(&generated_session).is_some());
    }

    #[test]
    fn lost_submit_response_retries_once_with_the_same_key() {
        let mut exec = FakeExecutor::new()
            .push(ok(json!({"session": {"sessionId": "shim-s-1"}})))
            .push(Err(crate::executor::ExecError::Exit {
                code: Some(1),
                stderr: "target died".to_owned(),
            }))
            .push(ok(
                json!({"kind": "run", "runId": "run-2", "durable": true,
                       "acceptedRevision": "rev-2", "deduplicated": false,
                       "state": "queued"}),
            ))
            .push(ok(
                json!({"frame": {"events": [], "caughtUp": true, "terminal": true}}),
            ))
            .push(ok(json!({"runId": "run-2", "state": "succeeded"})));
        let home = TempDir::new("run-retry");
        let code = main(&mut ctx(&mut exec, &home), &argv(&["again"]));
        assert_eq!(code, EXIT_OK);
        let keys: Vec<Option<&str>> = exec
            .calls
            .iter()
            .filter(|c| c.method == "run.submit")
            .map(|c| c.request_key.as_deref())
            .collect();
        assert_eq!(keys.len(), 2, "exactly one retry");
        assert_eq!(keys[0], keys[1], "retry must reuse the persisted key");
    }

    #[test]
    fn twice_lost_submit_response_is_result_unknown_exit_3() {
        let mut exec = FakeExecutor::new()
            .push(ok(json!({"session": {"sessionId": "shim-s-1"}})))
            .push(Err(crate::executor::ExecError::InvalidStdout {
                message: "garbage".to_owned(),
            }))
            .push(Err(crate::executor::ExecError::InvalidStdout {
                message: "garbage".to_owned(),
            }));
        let home = TempDir::new("run-unknown");
        let code = main(&mut ctx(&mut exec, &home), &argv(&["x"]));
        assert_eq!(code, EXIT_RESULT_UNKNOWN);
    }

    #[test]
    fn capability_unavailable_from_target_is_passed_through() {
        // e.g. a target that does not serve session.create today.
        let mut exec = FakeExecutor::new().push(failed("CAPABILITY_UNAVAILABLE"));
        let home = TempDir::new("run-capability");
        let code = main(&mut ctx(&mut exec, &home), &argv(&["x"]));
        assert_eq!(code, EXIT_FAILED);
        assert_eq!(exec.methods(), ["session.create"]);
    }

    #[test]
    fn unmatched_legacy_flags_are_configuration_required() {
        let mut exec = FakeExecutor::new();
        let home = TempDir::new("run-target");
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&["--gateway-url", "ws://old:18789/ws", "hi"]),
        );
        assert_eq!(code, EXIT_FAILED);
        assert!(
            exec.calls.is_empty(),
            "no business call before target resolution"
        );
    }

    #[test]
    fn matched_legacy_flags_set_the_target_on_the_executor() {
        let home = TempDir::new("run-target-ok");
        let config = CompatConfig {
            legacy_targets: vec![crate::config::LegacyTargetMapping {
                gateway_url: Some("ws://old:18789/ws".to_owned()),
                project: None,
                channel: None,
                target: "old-gw".to_owned(),
            }],
            ..CompatConfig::default()
        };
        crate::config::save(home.path(), &config).unwrap();

        let mut exec = FakeExecutor::new()
            .push(ok(json!({"session": {"sessionId": "shim-s-1"}})))
            .push(ok(
                json!({"kind": "run", "runId": "run-3", "durable": true,
                       "acceptedRevision": "rev-3", "deduplicated": false,
                       "state": "queued"}),
            ))
            .push(ok(
                json!({"frame": {"events": [], "caughtUp": true, "terminal": true}}),
            ))
            .push(ok(json!({"runId": "run-3", "state": "succeeded"})));
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&["--gateway-url", "ws://old:18789/ws", "hi"]),
        );
        assert_eq!(code, EXIT_OK);
        assert_eq!(exec.target.as_deref(), Some("old-gw"));
    }

    #[test]
    fn explicit_project_is_validated_with_project_get_first() {
        // context entry: --project maps onto a projectId validated by
        // project.get before the session is created.
        let home = TempDir::new("run-project-get");
        crate::config::save(
            home.path(),
            &crate::config::CompatConfig {
                legacy_targets: vec![crate::config::LegacyTargetMapping {
                    gateway_url: Some("ws://old:18789/ws".to_owned()),
                    project: None,
                    channel: None,
                    target: "old-gw".to_owned(),
                }],
                ..crate::config::CompatConfig::default()
            },
        )
        .unwrap();
        let mut exec = FakeExecutor::new().push(failed("NOT_FOUND"));
        let code = main(
            &mut ctx(&mut exec, &home),
            &argv(&[
                "--gateway-url",
                "ws://old:18789/ws",
                "--project",
                "proj-9",
                "hi",
            ]),
        );
        assert_eq!(code, EXIT_FAILED);
        assert_eq!(exec.methods(), ["project.get"]);
        assert_eq!(exec.calls[0].input["projectId"], "proj-9");
    }

    #[test]
    fn run_record_state_transitions_cover_submit_and_terminal() {
        let mut exec = FakeExecutor::new()
            .push(ok(json!({"session": {"sessionId": "shim-s-1"}})))
            .push(failed("INVALID_ARGUMENT"));
        let home = TempDir::new("run-fail");
        let code = main(&mut ctx(&mut exec, &home), &argv(&["boom"]));
        assert_eq!(code, EXIT_FAILED);
        let key = exec.calls[1].request_key.clone().unwrap();
        let record = state::RunRecord::load(home.path(), &key)
            .unwrap()
            .expect("write-ahead record exists even on submit failure");
        assert_eq!(record.state, state::RUN_SUBMITTING);
        assert_eq!(record.run_id, None);

        // Successful submit flips the record to running before the wait.
        let mut exec = FakeExecutor::new()
            .push(ok(json!({"session": {"sessionId": "shim-s-2"}})))
            .push(ok(
                json!({"kind": "run", "runId": "run-4", "durable": true,
                       "acceptedRevision": "rev-4", "deduplicated": false,
                       "state": "queued"}),
            ))
            .push(failed("UNAUTHENTICATED"));
        let home = TempDir::new("run-events-fail");
        let code = main(&mut ctx(&mut exec, &home), &argv(&["x"]));
        assert_eq!(code, EXIT_FAILED);
        let key = exec.calls[1].request_key.clone().unwrap();
        let record = state::RunRecord::load(home.path(), &key).unwrap().unwrap();
        assert_eq!(record.state, RUN_RUNNING);
        assert_eq!(record.run_id.as_deref(), Some("run-4"));
    }
}
