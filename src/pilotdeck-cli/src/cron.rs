//! `pilotdeck cron` (handleCronCommand, src/cli/pilotdeck.ts:362-424). Every
//! subcommand, usage errors included, first needs a reachable server at the
//! fixed probe address; then one gateway request, its result printed as
//! `JSON.stringify(result, null, 2)`.
use super::paths::{self, Env};
use super::remote_gateway::{self, RemoteGateway};
use super::{Failure, stderr_line, stdout_line, write_json_pretty_line};
use pilotdeck_protocol::{JsObject, JsValue, stringify_json};
use std::path::Path;

/// `argv` is everything after `cron`.
pub fn run(argv: &[String], env: &Env, cwd: &Path) -> Result<u8, Failure> {
    let Some(mut gateway) = remote_gateway::connect_if_available(env, cwd) else {
        stderr_line("pilotdeck cron requires a running pilotdeck server.");
        return Ok(1);
    };
    let outcome = dispatch(&mut gateway, argv, cwd);
    // The baseline never closes this connection, so its process does not
    // exit once the command is done (it waits until the server goes away).
    // The port closes it and exits; see the cli-cron-online-* scenarios.
    gateway.close();
    outcome
}

fn dispatch(gateway: &mut RemoteGateway, argv: &[String], cwd: &Path) -> Result<u8, Failure> {
    let has = |flag: &str| argv.iter().any(|arg| arg == flag);
    match argv.first().map(String::as_str) {
        Some("list") => {
            let mut params = JsObject::new();
            params.insert("includeHistory".into(), JsValue::Bool(has("--history")));
            if let Some(limit) = read_number_flag(argv, "--limit") {
                params.insert("limit".into(), JsValue::Number(limit));
            }
            print_result(gateway.request("cron_list", params))
        }
        Some("create") => {
            let message = read_string_flag(argv, "--message");
            let session_key = read_string_flag(argv, "--session");
            let once = read_string_flag(argv, "--once");
            let cron = read_string_flag(argv, "--cron");
            let (Some(message), Some(session_key), true) =
                (message, session_key, once.is_some() || cron.is_some())
            else {
                stderr_line(
                    "Usage: pilotdeck cron create --session <sessionKey> --message <text> (--once <iso> | --cron <expr>)",
                );
                return Ok(1);
            };
            let channel_key = read_string_flag(argv, "--channel")
                .map_or_else(|| infer_channel_key(session_key), str::to_owned);
            let project_key = read_string_flag(argv, "--project")
                .map_or_else(|| cwd.to_string_lossy().into_owned(), str::to_owned);
            let mut schedule = JsObject::new();
            match once {
                Some(once) => {
                    schedule.insert("type".into(), "once".into());
                    schedule.insert("runAt".into(), once.into());
                }
                None => {
                    schedule.insert("type".into(), "cron".into());
                    schedule.insert("expression".into(), cron.unwrap_or_default().into());
                }
            }
            let mut params = JsObject::new();
            params.insert("message".into(), message.into());
            params.insert("sessionKey".into(), session_key.into());
            params.insert("channelKey".into(), JsValue::String(channel_key.into()));
            params.insert("projectKey".into(), JsValue::String(project_key.into()));
            params.insert("schedule".into(), JsValue::Object(schedule));
            if let Some(timezone) = read_string_flag(argv, "--timezone") {
                params.insert("timezone".into(), timezone.into());
            }
            print_result(gateway.request("cron_create", params))
        }
        Some("delete") => {
            // `argv[1] ?? readStringFlag(argv, "--task")`: whatever follows the
            // subcommand, even a flag, is the task id.
            let task_id = argv
                .get(1)
                .map(String::as_str)
                .or_else(|| read_string_flag(argv, "--task"));
            let Some(task_id) = task_id.filter(|id| !id.is_empty()) else {
                stderr_line("Usage: pilotdeck cron delete <taskId> [--stop-running]");
                return Ok(1);
            };
            let mut params = JsObject::new();
            params.insert("taskId".into(), task_id.into());
            params.insert("stopRunning".into(), JsValue::Bool(has("--stop-running")));
            print_result(gateway.request("cron_delete", params))
        }
        Some("stop") => {
            let task_id = argv
                .get(1)
                .map(String::as_str)
                .or_else(|| read_string_flag(argv, "--task"));
            let run_id = read_string_flag(argv, "--run");
            if task_id.is_none_or(str::is_empty) && run_id.is_none() {
                stderr_line(
                    "Usage: pilotdeck cron stop <taskId> or pilotdeck cron stop --run <runId>",
                );
                return Ok(1);
            }
            let mut params = JsObject::new();
            if let Some(task_id) = task_id {
                params.insert("taskId".into(), task_id.into());
            }
            if let Some(run_id) = run_id {
                params.insert("runId".into(), run_id.into());
            }
            print_result(gateway.request("cron_stop", params))
        }
        _ => {
            stderr_line("Usage: pilotdeck cron <list|create|delete|stop>");
            Ok(1)
        }
    }
}

/// `console.log(JSON.stringify(result, null, 2))`; a rejected request is the
/// thrown error main().catch prints.
fn print_result(result: Result<JsValue, String>) -> Result<u8, Failure> {
    let result = result.map_err(Failure::Error)?;
    match stringify_json(&result) {
        Ok(Some(raw)) => write_json_pretty_line(&raw),
        // JSON.stringify(undefined) is undefined, which console.log prints.
        Ok(None) => stdout_line("undefined"),
        Err(error) => return Err(Failure::Error(error.to_string())),
    }
    Ok(0)
}

/// readStringFlag (pilotdeck.ts:527-534): the argument after the first
/// occurrence of `flag`, unless it is empty or starts with `--`.
fn read_string_flag<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
    let index = argv.iter().position(|arg| arg == flag)?;
    argv.get(index + 1)
        .map(String::as_str)
        .filter(|value| !value.is_empty() && !value.starts_with("--"))
}

/// readNumberFlag (pilotdeck.ts:546-553): `Number.parseInt(value, 10)`, kept
/// only when finite.
fn read_number_flag(argv: &[String], flag: &str) -> Option<f64> {
    parse_int(read_string_flag(argv, flag)?).filter(|value| value.is_finite())
}

/// `Number.parseInt(text, 10)`: leading JS whitespace, an optional sign, then
/// the longest run of decimal digits; NaN (None) without a digit.
fn parse_int(text: &str) -> Option<f64> {
    let text = paths::js_trim(text);
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let end = digits
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(digits.len());
    if end == 0 {
        return None;
    }
    let value: f64 = digits[..end].parse().ok()?;
    Some(if negative { -value } else { value })
}

/// inferChannelKey (pilotdeck.ts:555-558).
fn infer_channel_key(session_key: &str) -> String {
    match session_key.find(':') {
        Some(index) if index > 0 => session_key[..index].to_owned(),
        _ => "cli".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_int_follows_number_parse_int_radix_ten() {
        let cases: &[(&str, Option<f64>)] = &[
            ("5", Some(5.0)),
            ("2x", Some(2.0)),
            (" 2", Some(2.0)),
            ("\u{a0}\t7", Some(7.0)),
            ("1.9", Some(1.0)),
            ("-1", Some(-1.0)),
            ("+3", Some(3.0)),
            ("0x10", Some(0.0)),
            ("1e3", Some(1.0)),
            ("abc", None),
            ("-", None),
            ("", None),
            ("99999999999999999999", Some(1e20)),
        ];
        for (text, expected) in cases {
            assert_eq!(parse_int(text), *expected, "{text:?}");
        }
        assert!(parse_int("-0").is_some_and(|zero| zero == 0.0 && zero.is_sign_negative()));
        assert_eq!(parse_int(&"9".repeat(400)), Some(f64::INFINITY));
    }

    #[test]
    fn string_flags_skip_empty_and_double_dash_values() {
        let argv: Vec<String> = ["create", "--message", "--session", "s", "--cron", "", "--x"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert_eq!(read_string_flag(&argv, "--message"), None);
        assert_eq!(read_string_flag(&argv, "--session"), Some("s"));
        assert_eq!(read_string_flag(&argv, "--cron"), None);
        assert_eq!(read_string_flag(&argv, "--x"), None);
        assert_eq!(read_string_flag(&argv, "--missing"), None);
    }

    #[test]
    fn channel_key_is_the_prefix_before_a_non_leading_colon() {
        assert_eq!(infer_channel_key("web:abc:d"), "web");
        assert_eq!(infer_channel_key(":lead"), "cli");
        assert_eq!(infer_channel_key("plain"), "cli");
    }
}
