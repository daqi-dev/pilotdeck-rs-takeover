//! Optional `pilotdeck` compatibility shim (S4 plan, approved decision C1 /
//! `cli.contract.compatibility-adapter`).
//!
//! The shim adapts the legacy `pilotdeck` CLI surface onto the agentcpm-pd
//! public operations only:
//!
//! - every business call goes through the `agentcpm-pd` public entry
//!   (`api invoke`, see [`executor::AgentcpmPdInvoke`]); no direct database
//!   access, no legacy runtime fallback, no second backend;
//! - the builtin compat [`manifest`] lists the 27 legacy entries with their
//!   dispositions (supported / adapted / rejected) and the contract version;
//!   per-entry mapping DATA lives in `src/mappings/*.json` and is joined on
//!   slug at load time (see [`mappings`]);
//! - rejected entries (update / sidecar / gateway binaries) fail locally with
//!   an explicit error code and never start a gateway or read `~/.pilotdeck`.
//!
//! Exit codes follow the agentcpm-pd CLI contract (`007` §6.8): 0 success,
//! 1 known failure, 2 usage error, 3 write result unknown, 130 SIGINT.

pub mod config;
pub mod entry;
pub mod executor;
pub mod install;
pub mod manifest;
pub mod mappings;
pub mod state;

use std::io::BufRead;

use serde_json::{Value, json};

use crate::entry::usage_errors;

/// Legacy `pilotdeck` name this shim installs as.
pub const SHIM_NAME: &str = "pilotdeck";

/// Contract exit codes (`007` §6.8; same numbers as
/// `agentcpm-pd-protocol::errors`, redeclared because this crate is an
/// independent workspace and only depends on serde/serde_json).
pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_RESULT_UNKNOWN: i32 = 3;
pub const EXIT_INTERRUPTED: i32 = 130;

/// Total entry count of the compat surface (`cli.case.compat.*`).
pub const ENTRY_COUNT: usize = 27;

/// Library entrypoint, mirroring the `agentcpm-pd-cli` shape: receives argv
/// without the program name and returns the process exit code.
pub fn run(argv: impl Iterator<Item = String>) -> i32 {
    let argv: Vec<String> = argv.collect();

    // Private SIGINT guardian mode for run/resume (see entry::guardian).
    if argv.first().map(String::as_str) == Some(entry::guardian::MODE_ARG) {
        return entry::guardian::main(&argv[1..]);
    }

    // Multi-call binary names: a distribution may install this same binary
    // under the legacy `pilotdeck-agent-loop-sidecar` / `pilotdeck-gateway`
    // names; those invocations are rejected before any dispatch.
    let invoked_as = std::env::args_os()
        .next()
        .as_deref()
        .map(entry::reject::invoked_name);
    if let Some(rejection) = entry::reject::bin_name_rejection(
        invoked_as.as_deref(),
        &argv,
        &entry::reject::LegacyEnv::from_env(),
    ) {
        return rejection.emit();
    }

    dispatch(&argv)
}

/// Pure argv dispatch (environment access is confined to the entry contexts).
fn dispatch(argv: &[String]) -> i32 {
    let Some(top) = argv.first() else {
        // default-stdin (#4): empty argv enters the line-by-line interactive
        // mode; every non-empty line is submitted as one turn of the
        // configured prompt session; EOF ends normally.
        return with_ctx(default_stdin);
    };

    match top.as_str() {
        "help" | "--help" | "-h" => entry::help::main(&argv[1..]),
        "--version" | "-V" => entry::help::version(),
        "compat" => compat_command(&argv[1..]),
        "update" => entry::reject::update().emit(),
        "run" => with_ctx(|ctx| entry::run::main(ctx, &argv[1..])),
        "resume" => with_ctx(|ctx| entry::resume::main(ctx, &argv[1..])),
        "sessions" => with_ctx(|ctx| entry::sessions::main(ctx, &argv[1..])),
        "settings" => with_ctx(|ctx| entry::settings::main(ctx, &argv[1..])),
        "server" => with_ctx(|ctx| entry::server::main(ctx, &argv[1..])),
        "cron" => with_ctx(|ctx| entry::cron::main(ctx, &argv[1..])),
        "gateway" => with_ctx(|ctx| entry::setup::main(ctx, &argv[1..])),
        "chat" => with_ctx(|ctx| entry::chat_search::main(ctx, &argv[1..])),
        "skills" => with_ctx(|ctx| entry::skills::main(ctx, &argv[1..])),
        word if word.starts_with('-') => entry::usage_errors::unknown_option(word),
        word => default_argv(word, argv),
    }
}

/// Runs `f` with a lazily-built production executor and the env-resolved
/// entry context.
fn with_ctx(f: impl FnOnce(&mut entry::EntryCtx<'_>) -> i32) -> i32 {
    let mut exec = executor::AgentcpmPdInvoke::new();
    let mut ctx = entry::EntryCtx::from_env(&mut exec);
    f(&mut ctx)
}

/// Local diagnostics subcommand: read-only, verifiable manifest dump. Does
/// not start a gateway and does not read any home directory.
fn compat_command(rest: &[String]) -> i32 {
    match rest.first().map(String::as_str) {
        Some("manifest") if rest.len() == 1 => {
            let built = manifest::Manifest::builtin();
            match mappings::Mappings::load() {
                Ok(loaded) => {
                    println!("{}", built.summary_json(&loaded));
                    EXIT_OK
                }
                Err(e) => {
                    eprintln!("{SHIM_NAME}: compat mappings failed to load: {e}");
                    EXIT_FAILED
                }
            }
        }
        _ => usage_errors::usage(
            "expected exactly one subcommand: `compat manifest` (dumps the builtin compat manifest as JSON)",
        ),
    }
}

/// Unmatched top-level word: the `default-argv` entry treats it as a prompt
/// via `input.submit(source=argument)` after the mapping-data reserved-word
/// check (reserved words exit 2 instead of becoming prompts).
fn default_argv(word: &str, argv: &[String]) -> i32 {
    let Ok(loaded) = mappings::Mappings::load() else {
        // Compiled-in data failing to load is a shim bug.
        eprintln!("{SHIM_NAME}: compat mappings failed to load; cannot classify argv");
        return EXIT_FAILED;
    };
    if loaded.is_reserved_word(word) {
        return usage_errors::reserved_word(word);
    }
    // Old behavior: the whole argv joined with spaces is the prompt text
    // (CliChannel submit; one-shot, no stdin interaction).
    let prompt = argv.join(" ");
    with_ctx(|ctx| prompt_flow(ctx, &prompt))
}

/// The `default-stdin` entry: line-by-line multi-turn prompt mode. Lines
/// are processed as they arrive (CRLF trailing carriage returns and
/// surrounding whitespace trim away); blank lines are skipped; each line
/// is one turn of the SAME configured session, submitted sequentially and
/// awaited to its terminal state; EOF (including after an unterminated
/// final line) ends normally (exit 0).
fn default_stdin(ctx: &mut entry::EntryCtx<'_>) -> i32 {
    let stdin = std::io::stdin();
    let mut lock = stdin.lock();
    let mut raw = String::new();
    loop {
        raw.clear();
        match lock.read_line(&mut raw) {
            Ok(0) => return EXIT_OK, // EOF: normal end of the interactive mode
            Ok(_) => {}
            Err(e) => {
                eprintln!("{SHIM_NAME}: failed to read stdin: {e}");
                return EXIT_FAILED;
            }
        }
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let code = prompt_flow(ctx, line);
        if code != EXIT_OK {
            return code;
        }
        // Old behavior: one blank line after each turn.
        println!();
    }
}

/// The shared per-turn flow (`default-argv` / `default-stdin`): explicit
/// default session policy from the shim config (no implicit session is ever
/// created), then one `input.submit` and a paged `run.events` wait for the
/// turn's real terminal state.
///
/// `source` is always `argument`: the contract's oneOf (`argument` requires
/// `text`, `stdinText` forbids it) makes `stdinText` a CLI-child that reads
/// its own stdin to EOF and submits once — impossible to delegate from this
/// shim, which has already consumed stdin (`--input-json -` on the child
/// would also be `AMBIGUOUS_INPUT`). The shim therefore reads the text
/// itself and submits every turn as an explicit argument.
// S3-delivered: input.submit / run.events are served by the real targets;
// the former `input.follow` wait is retired (its InputFrame contract is a
// per-line submit stream, not a pollable terminal-state page).
fn prompt_flow(ctx: &mut entry::EntryCtx<'_>, text: &str) -> i32 {
    let home = match ctx.require_home() {
        Ok(home) => home.to_path_buf(),
        Err(code) => return code,
    };
    let config = match ctx.load_config(&home) {
        Ok(config) => config,
        Err(code) => return code,
    };
    let (Some(target_id), Some(session_id)) = (
        config.prompt_target_id.clone(),
        config.prompt_session_id.clone(),
    ) else {
        eprintln!(
            "{SHIM_NAME}: no default session policy for prompts (error: configuration_required); \
             set promptTargetId and promptSessionId in \
             $AGENTCPM_PD_HOME/compat/config.json"
        );
        return EXIT_FAILED;
    };

    // The `input.submit` local op derives the required `targetId` from the
    // `--target` profile; an explicit JSON targetId must equal it (contract
    // precondition, TARGET_MISMATCH). Passing the profile through --target
    // and omitting the field keeps a single source of truth, and `run.events`
    // then goes through the same profile.
    ctx.executor().set_target(Some(target_id));

    let request_key = state::generate_request_key();
    let receipt = match entry::call_op_keyed(
        ctx,
        "input.submit",
        &json!({
            "sessionId": session_id,
            "source": "argument",
            "text": text,
        }),
        &request_key,
    ) {
        Ok(receipt) => receipt,
        Err(code) => return code,
    };
    // Contract result (`$defs.RunAck`): `runId` sits at the top level of
    // the receipt (e2e: e2e_v1_input_replacement.rs reads
    // `result["receipt"]["runId"]`).
    let Some(run_id) = receipt.pointer("/receipt/runId").and_then(Value::as_str) else {
        eprintln!("{SHIM_NAME}: input.submit receipt carries no runId");
        return EXIT_FAILED;
    };
    prompt_turn(ctx, run_id, entry::run::EVENTS_POLL_INTERVAL)
}

/// Waits for one turn's real terminal state by paging `run.events`
/// through the shared wait loop ([`entry::run::page_run_events`]: cursor
/// handling, gap-replay dedup, and the wall-clock + stagnation page
/// budget live there once for both waits) and projecting every NEW event
/// through the legacy plain renderer: streamed text and `[error:<code>]`
/// lines.
fn prompt_turn(
    ctx: &mut entry::EntryCtx<'_>,
    run_id: &str,
    poll_interval: std::time::Duration,
) -> i32 {
    let mut failed = false;
    match entry::run::page_run_events(ctx, run_id, poll_interval, |event| {
        failed |= is_run_failure(event);
        print_prompt_event(event);
    }) {
        Ok(()) => {
            // Old exit codes: only a successful turn exits 0.
            if failed { EXIT_FAILED } else { EXIT_OK }
        }
        Err(code) => code,
    }
}

/// Terminal failure events of the new run model (`run.completed` is the
/// success one — the state event set of `run_driver.rs` / `store.rs`): a
/// turn ending in a non-success terminal state exits 1, matching the old
/// `default-argv` / `default-stdin` exit codes (0 success / 1 failure).
fn is_run_failure(event: &Value) -> bool {
    matches!(
        event.get("type").and_then(Value::as_str),
        Some("run.failed")
            | Some("run.cancelled")
            | Some("run.interrupted")
            | Some("run.result_unknown")
    )
}

/// Old plain renderer semantics (cli-render.ts:3-24): assistant text
/// prints as-is, errors render as `[error:<code>]` lines. In the new event
/// model the assistant output increment is `run.output.delta`
/// (`payload.text`, burst chunks `payload.delta`) and a failed run is the
/// `run.failed` event (`payload.errorCode`); both project onto the same
/// legacy lines (no byte-compatibility promise, per the mapping data).
fn print_prompt_event(event: &Value) {
    let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
    if event_type.contains("error") {
        let code = event
            .pointer("/payload/code")
            .and_then(Value::as_str)
            .unwrap_or("");
        let message = event
            .pointer("/payload/message")
            .and_then(Value::as_str)
            .unwrap_or("");
        println!("[error:{code}] {message}");
        return;
    }
    if event_type == "run.failed" {
        let code = event
            .pointer("/payload/errorCode")
            .and_then(Value::as_str)
            .unwrap_or("");
        println!("[error:{code}]");
        return;
    }
    let text = prompt_event_text(event);
    if !text.is_empty() {
        print!("{text}");
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
}

/// The assistant-text projection for one event: delegates to the shared
/// [`entry::run::extract_text`] filter (`run.output.delta` plus the legacy
/// assistant/subagent shapes).
fn prompt_event_text(event: &Value) -> String {
    crate::entry::run::extract_text(event)
}

#[cfg(test)]
pub(crate) mod testutil;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryCtx;
    use crate::executor::{ErrorPayload, FakeExecutor, Outcome, Response};
    use crate::testutil::TempDir;
    use serde_json::json;

    #[test]
    fn help_and_version_exit_ok() {
        for args in [
            vec!["help"],
            vec!["--help"],
            vec!["-h"],
            vec!["--version"],
            vec!["-V"],
        ] {
            let argv: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
            assert_eq!(dispatch(&argv), EXIT_OK, "argv: {args:?}");
        }
    }

    #[test]
    fn update_is_rejected_with_exit_1() {
        assert_eq!(dispatch(&["update".to_owned()]), EXIT_FAILED);
    }

    #[test]
    fn unknown_option_is_a_usage_error() {
        assert_eq!(dispatch(&["--bogus".to_owned()]), EXIT_USAGE);
    }

    #[test]
    fn reserved_words_exit_2_instead_of_becoming_prompts() {
        // From the mapping data (old top-level dispatch words + help and
        // version aliases). Words with their own dispatch never reach the
        // reserved-word check; `tui` does.
        assert_eq!(dispatch(&["tui".to_owned()]), EXIT_USAGE);
        assert_eq!(
            dispatch(&["tui".to_owned(), "extra".to_owned()]),
            EXIT_USAGE
        );
    }

    #[test]
    fn unknown_top_level_word_submits_a_prompt_through_the_configured_session() {
        // With no configured prompt policy: configuration_required.
        let home = TempDir::new("argv-noconfig");
        let mut exec = FakeExecutor::new();
        let mut ctx = EntryCtx::with_home(&mut exec, home.path());
        assert_eq!(prompt_flow(&mut ctx, "hello world"), EXIT_FAILED);
        assert!(exec.calls.is_empty());

        // With a policy: input.submit receives the joined argv as text and
        // the turn is awaited through run.events.
        let home = TempDir::new("argv-config");
        crate::config::save(
            home.path(),
            &crate::config::CompatConfig {
                prompt_target_id: Some("target-1".to_owned()),
                prompt_session_id: Some("shim-s-1".to_owned()),
                ..crate::config::CompatConfig::default()
            },
        )
        .unwrap();
        let mut exec = FakeExecutor::new()
            .push(Ok(Response {
                version: None,
                request_id: None,
                outcome: Outcome::Accepted,
                result: Some(json!({"receipt": {
                    "runId": "run-9",
                    "sessionId": "shim-s-1",
                    "accepted": "durable",
                    "state": "running",
                    "acceptedRevision": "rev-1",
                }})),
                error: None,
            }))
            .push(Ok(Response {
                version: None,
                request_id: None,
                outcome: Outcome::Succeeded,
                result: Some(json!({"frame": {
                    "events": [
                        {"type": "run.output.delta", "payload": {"text": "hi there"}},
                    ],
                    "caughtUp": true,
                    "terminal": true,
                }})),
                error: None,
            }));
        let mut ctx = EntryCtx::with_home(&mut exec, home.path());
        assert_eq!(prompt_flow(&mut ctx, "hello world"), EXIT_OK);
        assert_eq!(exec.methods(), ["input.submit", "run.events"]);
        let submit = &exec.calls[0];
        assert_eq!(
            submit.input,
            json!({
                "sessionId": "shim-s-1",
                "source": "argument",
                "text": "hello world",
            })
        );
        assert!(submit.request_key.is_some());
        // targetId is derived from --target by the local op, never sent.
        assert!(submit.input.get("targetId").is_none());
        assert_eq!(exec.target.as_deref(), Some("target-1"));
        let events = &exec.calls[1];
        assert_eq!(events.input, json!({"runId": "run-9"}));
        assert!(events.request_key.is_none());
    }

    #[test]
    fn capability_unavailable_from_input_submit_passes_through() {
        let home = TempDir::new("argv-cap");
        crate::config::save(
            home.path(),
            &crate::config::CompatConfig {
                prompt_target_id: Some("t".to_owned()),
                prompt_session_id: Some("s".to_owned()),
                ..crate::config::CompatConfig::default()
            },
        )
        .unwrap();
        let mut exec = FakeExecutor::new().push(Ok(Response {
            version: None,
            request_id: None,
            outcome: Outcome::Failed,
            result: None,
            error: Some(ErrorPayload {
                code: "CAPABILITY_UNAVAILABLE".to_owned(),
                message: "input.submit not served".to_owned(),
                details: None,
            }),
        }));
        let mut ctx = EntryCtx::with_home(&mut exec, home.path());
        assert_eq!(prompt_flow(&mut ctx, "line"), EXIT_FAILED);
        assert_eq!(exec.methods(), ["input.submit"]);
    }

    #[test]
    fn receipt_without_run_id_is_a_clear_failure() {
        let home = TempDir::new("argv-norunid");
        crate::config::save(
            home.path(),
            &crate::config::CompatConfig {
                prompt_target_id: Some("t".to_owned()),
                prompt_session_id: Some("s".to_owned()),
                ..crate::config::CompatConfig::default()
            },
        )
        .unwrap();
        let mut exec = FakeExecutor::new().push(Ok(Response {
            version: None,
            request_id: None,
            outcome: Outcome::Accepted,
            result: Some(json!({"receipt": {
                "sessionId": "s",
                "accepted": "durable",
                "state": "queued",
                "acceptedRevision": "rev-1",
            }})),
            error: None,
        }));
        let mut ctx = EntryCtx::with_home(&mut exec, home.path());
        assert_eq!(prompt_flow(&mut ctx, "line"), EXIT_FAILED);
        assert_eq!(exec.methods(), ["input.submit"]);
    }

    #[test]
    fn prompt_turn_pages_run_events_with_cursors_and_gaps() {
        let home = TempDir::new("argv-pages");
        crate::config::save(
            home.path(),
            &crate::config::CompatConfig {
                prompt_target_id: Some("t".to_owned()),
                prompt_session_id: Some("s".to_owned()),
                ..crate::config::CompatConfig::default()
            },
        )
        .unwrap();
        let mut exec = FakeExecutor::new()
            // submit
            .push(Ok(Response {
                version: None,
                request_id: None,
                outcome: Outcome::Accepted,
                result: Some(json!({"receipt": {"runId": "run-1", "state": "queued"}})),
                error: None,
            }))
            // page 1: one delta, not terminal yet
            .push(Ok(Response {
                version: None,
                request_id: None,
                outcome: Outcome::Succeeded,
                result: Some(json!({"frame": {
                    "events": [
                        {"type": "run.output.delta", "payload": {"text": "par"}},
                        {"type": "run.output.delta", "payload": {"delta": "tial"}},
                    ],
                    "caughtUp": true,
                    "terminal": false,
                    "nextCursor": "c1",
                }})),
                error: None,
            }))
            // page 2: gap frame restarts the query
            .push(Ok(Response {
                version: None,
                request_id: None,
                outcome: Outcome::Succeeded,
                result: Some(json!({"frame": {
                    "kind": "gap",
                    "reason": "cursor_expired",
                    "resourceId": "run-1",
                    "recoveryMethod": "run.snapshot",
                }})),
                error: None,
            }))
            // page 3 (restarted): still not caught up, no sleep before retry
            .push(Ok(Response {
                version: None,
                request_id: None,
                outcome: Outcome::Succeeded,
                result: Some(json!({"frame": {
                    "events": [],
                    "caughtUp": false,
                    "terminal": false,
                    "nextCursor": "c2",
                }})),
                error: None,
            }))
            // page 4: terminal
            .push(Ok(Response {
                version: None,
                request_id: None,
                outcome: Outcome::Succeeded,
                result: Some(json!({"frame": {
                    "events": [
                        {"type": "run.completed", "payload": {"runId": "run-1"}},
                    ],
                    "caughtUp": true,
                    "terminal": true,
                }})),
                error: None,
            }));
        let mut ctx = EntryCtx::with_home(&mut exec, home.path());
        assert_eq!(prompt_flow(&mut ctx, "x"), EXIT_OK);
        assert_eq!(exec.methods().len(), 5);
        let cursors: Vec<Option<&str>> = exec
            .calls
            .iter()
            .filter(|c| c.method == "run.events")
            .map(|c| c.input.get("afterCursor").and_then(Value::as_str))
            .collect();
        assert_eq!(
            cursors,
            [None, Some("c1"), None, Some("c2")],
            "gap restarts the query without a cursor"
        );
    }

    #[test]
    fn failed_turn_state_exits_1() {
        let home = TempDir::new("argv-failed");
        crate::config::save(
            home.path(),
            &crate::config::CompatConfig {
                prompt_target_id: Some("t".to_owned()),
                prompt_session_id: Some("s".to_owned()),
                ..crate::config::CompatConfig::default()
            },
        )
        .unwrap();
        let mut exec = FakeExecutor::new()
            .push(Ok(Response {
                version: None,
                request_id: None,
                outcome: Outcome::Accepted,
                result: Some(json!({"receipt": {"runId": "run-1", "state": "running"}})),
                error: None,
            }))
            .push(Ok(Response {
                version: None,
                request_id: None,
                outcome: Outcome::Succeeded,
                result: Some(json!({"frame": {
                    "events": [
                        {"type": "run.output.delta", "payload": {"text": "partial "}},
                        {"type": "run.failed", "payload": {"runId": "run-1", "errorCode": "E_TOOL"}},
                    ],
                    "caughtUp": true,
                    "terminal": true,
                }})),
                error: None,
            }));
        let mut ctx = EntryCtx::with_home(&mut exec, home.path());
        assert_eq!(prompt_flow(&mut ctx, "x"), EXIT_FAILED);
    }

    #[test]
    fn prompt_turn_empty_nonterminal_page_keeps_the_cursor() {
        // A non-terminal empty page answers `nextCursor: null` (no new
        // events yet, the turn is still running); the wait must KEEP the
        // previous cursor. Dropping it would replay the turn's events
        // from seq 0 on every such poll (duplicated output, growing text).
        let home = TempDir::new("argv-emptypage");
        let mut exec = FakeExecutor::new()
            .push(Ok(Response {
                version: None,
                request_id: None,
                outcome: Outcome::Succeeded,
                result: Some(json!({"frame": {
                    "events": [{"type": "run.output.delta", "payload": {"text": "aa"}}],
                    "caughtUp": true,
                    "terminal": false,
                    "nextCursor": "c1",
                }})),
                error: None,
            }))
            .push(Ok(Response {
                version: None,
                request_id: None,
                outcome: Outcome::Succeeded,
                result: Some(json!({"frame": {
                    "events": [],
                    "caughtUp": true,
                    "terminal": false,
                    "nextCursor": null,
                }})),
                error: None,
            }))
            .push(Ok(Response {
                version: None,
                request_id: None,
                outcome: Outcome::Succeeded,
                result: Some(json!({"frame": {
                    "events": [],
                    "caughtUp": true,
                    "terminal": true,
                }})),
                error: None,
            }));
        let mut ctx = EntryCtx::with_home(&mut exec, home.path());
        assert_eq!(
            prompt_turn(&mut ctx, "run-1", std::time::Duration::ZERO),
            EXIT_OK
        );
        let cursors: Vec<Option<&str>> = exec
            .calls
            .iter()
            .map(|c| c.input.get("afterCursor").and_then(Value::as_str))
            .collect();
        assert_eq!(
            cursors,
            [None, Some("c1"), Some("c1")],
            "the empty non-terminal page must not reset the cursor"
        );
    }

    #[test]
    fn prompt_turn_is_bounded_by_stagnation() {
        // A turn that never reaches a terminal state is an honest failure
        // after the stagnation budget, not an infinite poll loop.
        // caughtUp=false pages skip the retry sleep so the budget runs out
        // fast. The first page advances the cursor (None → "c"); every
        // later page repeats it.
        let home = TempDir::new("argv-bounded");
        let mut exec = FakeExecutor::new();
        for _ in 0..(entry::PAGE_STAGNATION_LIMIT + 5) {
            exec = exec.push(Ok(Response {
                version: None,
                request_id: None,
                outcome: Outcome::Succeeded,
                result: Some(json!({"frame": {
                    "events": [],
                    "caughtUp": false,
                    "terminal": false,
                    "nextCursor": "c",
                }})),
                error: None,
            }));
        }
        let mut ctx = EntryCtx::with_home(&mut exec, home.path());
        assert_eq!(
            prompt_turn(&mut ctx, "run-1", std::time::Duration::ZERO),
            EXIT_FAILED
        );
        assert_eq!(
            exec.calls.len() as u32,
            entry::PAGE_STAGNATION_LIMIT + 1,
            "the stagnation budget bounds the run.events polling"
        );
    }

    #[test]
    fn empty_argv_reads_stdin_lines_in_one_session() {
        // prompt_flow is the per-turn unit (dispatch itself reads real
        // stdin, which the test harness does not provide); three non-empty
        // turns => exactly 3 submit/events pairs, all in the same session
        // and the same target profile, each awaited to its terminal state.
        let home = TempDir::new("stdin-config");
        crate::config::save(
            home.path(),
            &crate::config::CompatConfig {
                prompt_target_id: Some("t".to_owned()),
                prompt_session_id: Some("s".to_owned()),
                ..crate::config::CompatConfig::default()
            },
        )
        .unwrap();
        let mut exec = FakeExecutor::new();
        for run in ["run-1", "run-2", "run-3"] {
            exec = exec
                .push(Ok(Response {
                    version: None,
                    request_id: None,
                    outcome: Outcome::Accepted,
                    result: Some(json!({"receipt": {"runId": run, "state": "queued"}})),
                    error: None,
                }))
                .push(Ok(Response {
                    version: None,
                    request_id: None,
                    outcome: Outcome::Succeeded,
                    result: Some(json!({"frame": {"events": [], "terminal": true}})),
                    error: None,
                }));
        }
        let mut ctx = EntryCtx::with_home(&mut exec, home.path());
        let mut failures = 0;
        for line in ["first", "second", "third"] {
            if prompt_flow(&mut ctx, line) != EXIT_OK {
                failures += 1;
            }
        }
        assert_eq!(failures, 0);
        let methods = exec.methods();
        assert_eq!(
            methods,
            [
                "input.submit",
                "run.events",
                "input.submit",
                "run.events",
                "input.submit",
                "run.events"
            ]
        );
        for call in &exec.calls {
            if call.method == "input.submit" {
                // Same session for every turn; each turn is an explicit
                // argument (`stdinText` + `text` would violate the
                // contract's oneOf from this shim).
                assert_eq!(call.input["sessionId"], "s", "same session all turns");
                assert_eq!(call.input["source"], "argument");
            }
        }
        assert_eq!(exec.target.as_deref(), Some("t"));
    }

    #[test]
    fn compat_manifest_dumps_json() {
        assert_eq!(
            dispatch(&["compat".to_owned(), "manifest".to_owned()]),
            EXIT_OK
        );
    }

    #[test]
    fn compat_without_manifest_is_a_usage_error() {
        assert_eq!(dispatch(&["compat".to_owned()]), EXIT_USAGE);
        assert_eq!(
            dispatch(&["compat".to_owned(), "manifest".to_owned(), "x".to_owned()]),
            EXIT_USAGE
        );
    }

    #[test]
    fn wired_entries_run_their_orchestrations() {
        // Each entry parses argv and invokes its first operation through
        // the executor (proved in the entry modules); here we check the
        // dispatch wiring reaches them (exit 1 = target op failed through
        // the fake's empty queue, not the old unwired placeholder).
        let cases: [(&str, Vec<&str>); 8] = [
            ("run", vec!["run", "hello"]),
            ("resume", vec!["resume", "s1", "hi"]),
            ("sessions", vec!["sessions", "list"]),
            ("settings", vec!["settings", "get"]),
            ("server", vec!["server"]),
            ("cron", vec!["cron", "list"]),
            ("chat", vec!["chat", "search", "kw"]),
            ("skills", vec!["skills", "migrate", "--source", "/x"]),
        ];
        for (top, argv) in cases {
            let argv: Vec<String> = argv.into_iter().map(String::from).collect();
            assert_eq!(dispatch(&argv), EXIT_FAILED, "top: {top}");
        }
        // Without sources the entry is a usage error, not an implicit scan.
        assert_eq!(
            dispatch(&["skills".to_owned(), "migrate".to_owned()]),
            EXIT_USAGE
        );
        // Missing required arguments are usage errors.
        assert_eq!(dispatch(&["resume".to_owned()]), EXIT_USAGE);
        assert_eq!(dispatch(&["cron".to_owned()]), EXIT_USAGE);
        assert_eq!(dispatch(&["chat".to_owned()]), EXIT_USAGE);
        // `gateway` without `setup` is a usage error inside the entry.
        assert_eq!(dispatch(&["gateway".to_owned()]), EXIT_USAGE);
    }

    #[test]
    fn guardian_mode_dispatches_before_everything_else() {
        // The guardian entry point needs no home; a bogus argument is its
        // own usage failure (exit 1, not the shim's usage-error path).
        assert_eq!(
            run(["__compat-guardian".to_owned(), "--bogus".to_owned(),].into_iter()),
            EXIT_FAILED
        );
    }
}
