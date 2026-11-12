//! `pilotdeck` entry. Dispatch order follows src/cli/pilotdeck.ts main().
mod chat_search;
mod cli_channel;
mod config;
mod gateway_setup;
pub(crate) mod paths;
mod probe;
mod proxy;
pub(crate) mod sdk;
mod server;
pub(crate) mod skills;
mod skills_migrate;
mod telemetry;
#[cfg(test)]
#[path = "../tests/common/negative_receipt.rs"]
pub(crate) mod test_negative_receipt;

use paths::Env;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;

#[derive(Debug)]
pub enum Failure {
    /// A thrown Error: TS main().catch prints its message and sets exit code 1.
    Error(String),
    /// A branch the Rust port has not reached yet. Never a parity result.
    Todo(&'static str),
}

// The String form of the reindentation in write_json_pretty_into, its bytes
// collected in memory. One implementation only (#983 fix-r11): sessions list
// and settings stream through the writer directly, so a second copy of the
// indent/escape logic here could drift unmutated past the negative controls.
pub(crate) fn json_pretty(raw: &str) -> String {
    let mut output = Vec::new();
    write_json_pretty_into(raw, &mut output).expect("#983 in-memory pretty write");
    String::from_utf8(output).expect("#983 pretty bytes are valid UTF-8")
}

pub fn stdout_line(text: &str) {
    let _ = writeln!(std::io::stdout().lock(), "{text}");
}

// json_pretty's reindentation written straight to stdout in bounded chunks: a
// sessions projection can be the whole response frame, and building the full
// pretty String first would hold a second copy of it in memory (#983). The
// bytes and the ignored-write-error policy match `stdout_line(&json_pretty(raw))`.
pub fn write_json_pretty_line(raw: &str) {
    let _ = try_write_json_pretty_line(raw);
}

fn try_write_json_pretty_line(raw: &str) -> std::io::Result<()> {
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::with_capacity(64 * 1024, stdout.lock());
    write_json_pretty_into(raw, &mut out)?;
    out.write_all(b"\n")?;
    out.flush()
}

// Reindent bytes from the JS codec, preserving property order, number spelling,
// lone surrogates and all whitespace inside strings. Callers own codec errors
// and the policy for values without a JSON representation.
fn write_json_pretty_into(raw: &str, out: &mut impl Write) -> std::io::Result<()> {
    fn char(out: &mut impl Write, value: char) -> std::io::Result<()> {
        let mut encoded = [0_u8; 4];
        out.write_all(value.encode_utf8(&mut encoded).as_bytes())
    }

    fn newline(out: &mut impl Write, depth: usize) -> std::io::Result<()> {
        out.write_all(b"\n")?;
        for _ in 0..depth {
            out.write_all(b"  ")?;
        }
        Ok(())
    }

    let mut chars = raw.chars().peekable();
    let (mut depth, mut quoted, mut escaped) = (0usize, false, false);
    while let Some(value) = chars.next() {
        if quoted {
            char(out, value)?;
            if escaped {
                escaped = false;
            } else if value == '\\' {
                escaped = true;
            } else if value == '"' {
                quoted = false;
            }
            continue;
        }
        match value {
            '"' => {
                quoted = true;
                char(out, value)?;
            }
            '{' | '[' => {
                char(out, value)?;
                let close = if value == '{' { '}' } else { ']' };
                if chars.peek() == Some(&close) {
                    char(out, close)?;
                    chars.next();
                } else {
                    depth += 1;
                    newline(out, depth)?;
                }
            }
            '}' | ']' => {
                depth -= 1;
                newline(out, depth)?;
                char(out, value)?;
            }
            ',' => {
                char(out, value)?;
                newline(out, depth)?;
            }
            ':' => out.write_all(b": ")?,
            _ => char(out, value)?,
        }
    }
    Ok(())
}

pub fn stderr_line(text: &str) {
    let _ = writeln!(std::io::stderr().lock(), "{text}");
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let env = Env::from_process();
    // `await installGlobalProxy()` at module load (pilotdeck.ts:28), before
    // any command runs.
    proxy::install_from_env(&env);
    let code = match std::env::current_dir() {
        Ok(cwd) => run(&argv, &env, &cwd),
        Err(_) => Err(Failure::Todo(
            "startup without a readable working directory",
        )),
    };
    ExitCode::from(match code {
        Ok(code) => code,
        Err(Failure::Error(message)) => {
            stderr_line(&message);
            1
        }
        Err(Failure::Todo(what)) => {
            stderr_line(&format!(
                "pilotdeck-rs TODO: {what} is not implemented yet."
            ));
            2
        }
    })
}

pub(crate) fn run(argv: &[String], env: &Env, cwd: &Path) -> Result<u8, Failure> {
    let command = argv.first().map(String::as_str);
    match command {
        Some("run" | "resume" | "sessions" | "settings" | "help" | "--help" | "-h") => {
            sdk::run_sdk_cli(argv, env, cwd)
        }
        Some("server") => server::run(argv, env, cwd),
        Some("gateway") => {
            if argv.get(1).map(String::as_str) == Some("setup") {
                return gateway_setup::run(&argv[2..], env);
            }
            stderr_line("Usage: pilotdeck gateway setup [feishu|weixin|wecom]");
            Ok(1)
        }
        Some("cron") => run_cron(env, cwd),
        Some("update") => run_update(),
        Some("skills") => skills_migrate::run(&argv[1..], env, cwd),
        Some("chat") => chat_search::run(&argv[1..], env, cwd),
        Some("tui") => {
            if !std::io::stdin().is_terminal() {
                stderr_line("pilotdeck tui requires an interactive terminal.");
                return Ok(1);
            }
            Err(Failure::Todo("pilotdeck tui"))
        }
        _ => cli_channel::run(argv, env, cwd),
    }
}

/// handleUpdateCommand (src/cli/pilotdeck.ts:351-360). The updater is the
/// install's own `scripts/update-web.mjs`, found by walking up from the
/// running code (findUpdateRoot, src/runtime/updateCommand.ts:6-11). An
/// install without it fails before anything runs, which is what a Rust build
/// is: it ships no Git Web updater.
fn run_update() -> Result<u8, Failure> {
    let exe = std::env::current_exe()
        .ok()
        .map(|exe| std::fs::canonicalize(&exe).unwrap_or(exe));
    let start = exe.as_deref().and_then(Path::parent);
    if find_update_root(start).is_some() {
        // Running it needs the Node runtime the baseline spawns
        // (`process.execPath`), which a Rust install does not have.
        return Err(Failure::Todo("pilotdeck update with a Git Web updater"));
    }
    stderr_line(
        "Update failed: This installation does not include the Git Web updater. \
         Use application settings or update manually.",
    );
    Ok(1)
}

fn find_update_root(start: Option<&Path>) -> Option<&Path> {
    start?
        .ancestors()
        .find(|dir| dir.join("scripts/update-web.mjs").exists())
}

/// handleCronCommand (src/cli/pilotdeck.ts:362-368): every cron subcommand,
/// usage errors included, first needs a reachable local server.
fn run_cron(env: &Env, cwd: &Path) -> Result<u8, Failure> {
    if probe::probe_gateway_server(env, cwd).is_none() {
        stderr_line("pilotdeck cron requires a running pilotdeck server.");
        return Ok(1);
    }
    // The probe address is fixed (probeServer.ts:16), so the connected
    // branch cannot be pointed at an isolated gateway and compared.
    Err(Failure::Todo("pilotdeck cron against a running server"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_negative_receipt::verify;

    /// findUpdateRoot: the nearest ancestor holding scripts/update-web.mjs,
    /// none when no ancestor has it.
    #[test]
    fn update_root_is_the_nearest_ancestor_with_the_updater() {
        let root = std::env::temp_dir().join(format!("pdrs-update-root-{}", std::process::id()));
        let deep = root.join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(find_update_root(Some(&deep)), None);
        std::fs::create_dir_all(root.join("a/scripts")).unwrap();
        std::fs::write(root.join("a/scripts/update-web.mjs"), "").unwrap();
        assert_eq!(
            find_update_root(Some(&deep)),
            Some(root.join("a").as_path())
        );
        assert_eq!(find_update_root(None), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// #983: the streamed pretty printer must be byte-identical to the
    /// two-String form it replaces, including escapes, lone surrogates,
    /// non-ASCII text, empty containers and deep nesting.
    #[test]
    fn streamed_pretty_matches_json_pretty_bytes() {
        for raw in [
            "[]",
            "{}",
            "0",
            "[[[[1]]]]",
            r#"[{"z":[{},[],"中\"[,]"],"a":-0,"b":1e+21,"s":"\ud800"}]"#,
            r#"{"a":{"b":[1,2]},"c":"}\"\"","d":"中"}"#,
            r#"[true,false,null,"\n\t\r\b\f/"]"#,
            r#"{"nested":{"empty":{},"list":[[],{},"x"]}}"#,
        ] {
            let mut streamed = Vec::new();
            write_json_pretty_into(raw, &mut streamed).expect("#983 streamed pretty write");
            verify(
                streamed == json_pretty(raw).as_bytes(),
                "#983 streamed pretty bytes",
            );
        }
    }
}
