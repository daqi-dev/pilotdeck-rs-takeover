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
    with_ctx(|ctx| prompt_flow(ctx, "argument", &prompt))
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
        let code = prompt_flow(ctx, "stdinText", line);
        if code != EXIT_OK {
            return code;
        }
        // Old behavior: one blank line after each turn.
        println!();
    }
}

/// The shared prompt-entry flow (`default-argv` / `default-stdin`):
/// explicit default session policy from the shim config (no implicit
/// session is ever created), then `input.submit` + `input.follow` to the
/// turn's real terminal state.
// TODO-S3: input.submit / input.follow are not delivered yet; the calls
// below surface CAPABILITY_UNAVAILABLE from today's targets faithfully.
fn prompt_flow(ctx: &mut entry::EntryCtx<'_>, source: &str, text: &str) -> i32 {
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

    // TODO-S3: input.submit is not delivered yet.
    let request_key = state::generate_request_key();
    let receipt = match ctx.executor().invoke_keyed(
        "input.submit",
        &json!({
            "targetId": target_id,
            "sessionId": session_id,
            "source": source,
            "text": text,
        }),
        &request_key,
    ) {
        Ok(resp) => match entry::unwrap_result("input.submit", &resp) {
            Ok(result) => result,
            Err(code) => return code,
        },
        Err(e) => return entry::surface_exec_error("input.submit", &e),
    };
    let _ = receipt;

    // TODO-S3: input.follow is not delivered yet; line-oriented multi-turn
    // mode (lines) matches the old plain renderer semantics.
    loop {
        let resp = match ctx.executor().invoke(
            "input.follow",
            &json!({
                "targetId": target_id,
                "sessionId": session_id,
                "mode": "lines",
                "continueOnError": false,
            }),
        ) {
            Ok(resp) => resp,
            Err(e) => return entry::surface_exec_error("input.follow", &e),
        };
        let result = match entry::unwrap_result("input.follow", &resp) {
            Ok(result) => result,
            Err(code) => return code,
        };
        let Some(frame) = result.get("frame").filter(|f| f.is_object()) else {
            eprintln!("{SHIM_NAME}: input.follow returned no frame object");
            return EXIT_FAILED;
        };
        if let Some(events) = frame.get("events").and_then(Value::as_array) {
            for event in events {
                print_prompt_event(event);
            }
        }
        if frame.get("terminal").and_then(Value::as_bool) == Some(true) {
            return EXIT_OK;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// Old plain renderer semantics (cli-render.ts:3-24): assistant text
/// prints as-is, errors render as `[error:<code>]` lines.
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
    let text = crate::entry::run::extract_text(event);
    if !text.is_empty() {
        print!("{text}");
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
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
        assert_eq!(
            prompt_flow(&mut ctx, "argument", "hello world"),
            EXIT_FAILED
        );
        assert!(exec.calls.is_empty());

        // With a policy: input.submit receives the joined argv as text.
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
                outcome: Outcome::Succeeded,
                result: Some(json!({"receipt": {}})),
                error: None,
            }))
            .push(Ok(Response {
                version: None,
                request_id: None,
                outcome: Outcome::Succeeded,
                result: Some(json!({"frame": {
                    "events": [
                        {"type": "assistant_text_delta", "payload": {"delta": "hi"}},
                    ],
                    "terminal": true,
                }})),
                error: None,
            }));
        let mut ctx = EntryCtx::with_home(&mut exec, home.path());
        assert_eq!(prompt_flow(&mut ctx, "argument", "hello world"), EXIT_OK);
        assert_eq!(exec.methods(), ["input.submit", "input.follow"]);
        let submit = &exec.calls[0];
        assert_eq!(submit.input["source"], "argument");
        assert_eq!(submit.input["text"], "hello world");
        assert_eq!(submit.input["targetId"], "target-1");
        assert_eq!(submit.input["sessionId"], "shim-s-1");
        assert!(submit.request_key.is_some());
        let follow = &exec.calls[1];
        assert_eq!(follow.input["mode"], "lines");
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
        assert_eq!(prompt_flow(&mut ctx, "stdinText", "line"), EXIT_FAILED);
        assert_eq!(exec.methods(), ["input.submit"]);
    }

    #[test]
    fn empty_argv_reads_stdin_lines_in_one_session() {
        // prompt_flow is the per-turn unit (dispatch itself reads real
        // stdin, which the test harness does not provide); three non-empty
        // turns => exactly 3 submit/follow pairs, all in the same session.
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
        for _ in 0..3 {
            exec = exec
                .push(Ok(Response {
                    version: None,
                    request_id: None,
                    outcome: Outcome::Succeeded,
                    result: Some(json!({"receipt": {}})),
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
            if prompt_flow(&mut ctx, "stdinText", line) != EXIT_OK {
                failures += 1;
            }
        }
        assert_eq!(failures, 0);
        let methods = exec.methods();
        assert_eq!(
            methods,
            [
                "input.submit",
                "input.follow",
                "input.submit",
                "input.follow",
                "input.submit",
                "input.follow"
            ]
        );
        for call in &exec.calls {
            assert_eq!(call.input["sessionId"], "s", "same session all turns");
            if call.method == "input.submit" {
                assert_eq!(call.input["source"], "stdinText");
            }
        }
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
