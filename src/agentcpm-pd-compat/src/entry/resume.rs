//! `sdk-resume` entry (`cli.compat.sdk-resume`, supported): `pilotdeck resume
//! <session-id> <prompt> [SDK options]`.
//!
//! The legacy session id must resolve through the shim's persistent
//! old→new mapping table (`$AGENTCPM_PD_HOME/compat/sessions.json`) — an
//! unknown id fails explicitly (exit 1), never guessed. The mapping row is
//! validated with `session.get` (NOT_FOUND/FORBIDDEN from the target are
//! surfaced verbatim), then a NEW run is submitted to the existing session
//! with its own persisted request key (`run.submit` → `run.events` →
//! `run.cancel` via the shared flow in [`crate::entry::run`]). The old run
//! is never revived or re-observed.

use serde_json::json;

use crate::entry::run::{RunArgs, SDK_OPTION_NAMES, legacy_flags, resolve_target_or_fail};
use crate::entry::{EntryCtx, call_op, parse_cli, usage_errors};
use crate::{EXIT_FAILED, SHIM_NAME};

/// Parsed `resume` arguments.
#[derive(Clone, Debug)]
pub struct ResumeArgs {
    pub session_id: String,
    pub prompt: String,
    /// SDK options for target resolution and the run submission.
    pub(crate) sdk: crate::entry::Parsed,
    pub json: bool,
    pub stream_json: bool,
}

pub fn parse(rest: &[String]) -> Result<ResumeArgs, String> {
    let spec = crate::entry::OptSpec {
        values: SDK_OPTION_NAMES,
        flags: &["--json", "--stream-json"],
    };
    let parsed = parse_cli(rest, &spec)?;
    // Usage errors must precede any business write (same rule as `run`).
    if let Some(raw) = parsed.value("--model")
        && crate::entry::run::parse_model_value(raw).is_none()
    {
        return Err("--model expects provider/model".to_owned());
    }
    let [session_id, prompt] = parsed.positionals.as_slice() else {
        return Err("resume requires exactly <session-id> and <prompt>".to_owned());
    };
    Ok(ResumeArgs {
        session_id: session_id.clone(),
        prompt: prompt.clone(),
        sdk: parsed.clone(),
        json: parsed.flag("--json"),
        stream_json: parsed.flag("--stream-json"),
    })
}

/// Entry point: map the legacy session, validate it, run the shared flow.
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
    let flags = legacy_flags(&args.sdk);
    let target = match resolve_target_or_fail(&config, &flags) {
        Ok(target) => target,
        Err(code) => return code,
    };
    ctx.executor().set_target(target);

    // The legacy session id must exist in the shim's mapping table.
    let sessions = match ctx.load_sessions(&home) {
        Ok(sessions) => sessions,
        Err(code) => return code,
    };
    let Some(mapping) = sessions.find(&args.session_id) else {
        eprintln!(
            "{SHIM_NAME}: unknown session id `{}` (error: NOT_FOUND); \
             ids printed by this shim are recorded under \
             $AGENTCPM_PD_HOME/compat/sessions.json",
            args.session_id
        );
        return EXIT_FAILED;
    };
    let new_session_id = mapping.session_id.clone();

    // Validate the session on the target; a missing/forbidden session is
    // surfaced verbatim.
    if let Err(code) = call_op(ctx, "session.get", &json!({"sessionId": new_session_id})) {
        return code;
    }

    // Submit a new run to the existing session through the shared flow.
    let run_args = RunArgs {
        prompt: Some(args.prompt.clone()),
        json: args.json,
        stream_json: args.stream_json,
        sdk: args.sdk.clone(),
    };
    crate::entry::run::run_prompt(
        ctx,
        &run_args,
        &args.prompt,
        Some((&args.session_id, &new_session_id)),
    )
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

    fn ok(result: serde_json::Value) -> Result<Response, crate::executor::ExecError> {
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
    fn parse_requires_session_id_and_prompt() {
        let args = parse(&argv(&["s1", "continue this", "--project", "p"])).unwrap();
        assert_eq!(args.session_id, "s1");
        assert_eq!(args.prompt, "continue this");
        assert!(parse(&argv(&["s1"])).is_err());
        assert!(parse(&argv(&[])).is_err());
        assert!(parse(&argv(&["--bogus", "s", "p"])).is_err());
        assert!(parse(&argv(&["--model", "x", "s", "p"])).is_err());
    }

    #[test]
    fn unknown_legacy_session_id_fails_explicitly_without_business_calls() {
        let mut exec = FakeExecutor::new();
        let home = TempDir::new("resume-unknown");
        let code = main(&mut ctx(&mut exec, &home), &argv(&["nope", "hi"]));
        assert_eq!(code, EXIT_FAILED);
        assert!(exec.calls.is_empty());
    }

    #[test]
    fn mapped_session_runs_the_shared_flow_with_a_new_run() {
        let home = TempDir::new("resume-flow");
        let mut map = SessionMap::default();
        map.record(
            home.path(),
            SessionMapping {
                legacy_session_id: "old-1".to_owned(),
                session_id: "new-1".to_owned(),
                project_id: "shim-p-1".to_owned(),
                created_at: "1".to_owned(),
            },
        )
        .unwrap();

        let mut exec = FakeExecutor::new()
            .push(ok(json!({"session": {"sessionId": "new-1"}})))
            .push(ok(json!({"kind": "run", "runId": "run-7", "durable": true,
                       "acceptedRevision": "rev-7", "deduplicated": false,
                       "state": "queued"})))
            .push(ok(json!({"frame": {"events": [
                {"type": "run.output.delta", "payload": {"text": "resumed"}}
            ], "caughtUp": true, "terminal": true}})))
            .push(ok(json!({"runId": "run-7", "state": "succeeded"})));
        let code = main(&mut ctx(&mut exec, &home), &argv(&["old-1", "continue"]));
        assert_eq!(code, crate::EXIT_OK);
        assert_eq!(
            exec.methods(),
            ["session.get", "run.submit", "run.events", "run.get"]
        );
        // The new run goes to the MAPPED (new) session id.
        assert_eq!(exec.calls[1].input["sessionId"], "new-1");
        assert_eq!(exec.calls[1].input["input"][0]["text"], "continue");
    }

    #[test]
    fn resume_wait_extracts_run_output_delta_text() {
        let mut exec = FakeExecutor::new().push(ok(json!({"frame": {"events": [
            {"type": "run.output.delta", "payload": {"text": "resumed "}},
            {"type": "run.output.delta", "payload": {"delta": "turn"}}
        ], "caughtUp": true, "terminal": true}})));
        let home = TempDir::new("resume-delta");
        let mut context = ctx(&mut exec, &home);
        let text = crate::entry::run::wait_for_terminal(
            &mut context,
            "run-7",
            false,
            std::time::Duration::ZERO,
        );
        assert_eq!(text, Ok("resumed turn".to_owned()));
    }

    #[test]
    fn session_get_failure_is_surfaced_verbatim() {
        let home = TempDir::new("resume-get-fail");
        let mut map = SessionMap::default();
        map.record(
            home.path(),
            SessionMapping {
                legacy_session_id: "old-2".to_owned(),
                session_id: "new-2".to_owned(),
                project_id: "shim-p-1".to_owned(),
                created_at: "1".to_owned(),
            },
        )
        .unwrap();
        let mut exec = FakeExecutor::new().push(Ok(Response {
            version: None,
            request_id: None,
            outcome: Outcome::Failed,
            result: None,
            error: Some(crate::executor::ErrorPayload {
                code: "NOT_FOUND".to_owned(),
                message: "session gone".to_owned(),
                details: None,
            }),
        }));
        let code = main(&mut ctx(&mut exec, &home), &argv(&["old-2", "hi"]));
        assert_eq!(code, EXIT_FAILED);
        assert_eq!(exec.methods(), ["session.get"]);
    }

    #[test]
    fn parse_args_carry_json_flags_into_the_shared_flow() {
        let args = parse(&argv(&["s", "p", "--json"])).unwrap();
        assert!(args.json && !args.stream_json);
    }
}
