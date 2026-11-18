//! `entry-help` (`cli.compat.entry-help`): fully local help text with
//! migration notes and shim/target version discovery. Never starts a
//! gateway, never reads a home directory, and displays no tokens or business
//! data.

use crate::executor::{TARGET_BIN_ENV, TARGET_BIN_NAME, resolve_target_bin};
use crate::manifest::{CONTRACT_VERSION, Manifest};
use crate::mappings::Mappings;
use crate::{EXIT_OK, SHIM_NAME};

/// `pilotdeck help [--help|-h] [word]`.
pub fn main(rest: &[String]) -> i32 {
    match rest.first().map(String::as_str) {
        None => print!("{}", full_help()),
        // `help help` etc. keep showing the full text.
        Some("--help" | "-h" | "help") => print!("{}", full_help()),
        Some("compat") => print!("{}", compat_detail()),
        Some(word) => match detail_for_word(word) {
            Some(detail) => print!("{detail}"),
            None => return crate::entry::usage_errors::unknown_subcommand("help", word),
        },
    }
    EXIT_OK
}

/// Detail text for `help compat` (shim-local diagnostics namespace).
fn compat_detail() -> String {
    "compat manifest (local):\n  dumps the builtin 27-entry compat manifest as JSON\n  read-only: no gateway, no home directory, no tokens\n".to_owned()
}

/// `pilotdeck --version` / `-V`.
///
/// Adjudicated behavior (registered accepted difference): the legacy
/// baseline has NO `--version` implementation — the flag fell through to
/// argv-as-prompt (pilotdeck.ts:30-348 has no version branch; see the
/// `usage-errors` mapping data). The `entry-help` case instead requires
/// "帮助别名能发现 shim 与目标版本", so the shim prints the shim version,
/// the contract version, and the target discovery status, and exits 0.
/// This deviation from the old (prompt-submission) behavior is registered
/// in the mapping data's `usageErrorContract` for `usage-errors` and here.
pub fn version() -> i32 {
    println!(
        "{SHIM_NAME} (agentcpm-pd compatibility shim) {}",
        env!("CARGO_PKG_VERSION")
    );
    println!("contract: {CONTRACT_VERSION}");
    println!("target: {}", target_line());
    EXIT_OK
}

/// Resolves the target binary without executing it (path discovery only).
fn target_line() -> String {
    match resolve_target_bin(std::env::var_os(TARGET_BIN_ENV), std::env::var_os("PATH")) {
        Ok(path) => format!("{}", path.display()),
        Err(_) => {
            format!("`{TARGET_BIN_NAME}` not found on PATH (set {TARGET_BIN_ENV} to pin one)")
        }
    }
}

/// Builds the full help text from the builtin manifest (entries, dispositions,
/// mapping status) plus fixed migration notes.
fn full_help() -> String {
    let manifest = Manifest::builtin();
    let mappings = Mappings::load().ok();

    let mut out = String::new();
    out.push_str(&format!(
        "{SHIM_NAME} (agentcpm-pd compatibility shim) {}\n",
        env!("CARGO_PKG_VERSION")
    ));
    out.push_str(
        "\nThis `pilotdeck` command is a compatibility adapter for the legacy PilotDeck\n\
         CLI. It only calls agentcpm-pd public operations through the\n\
         `{TARGET_BIN_NAME}` CLI; it never starts the legacy gateway, never reads\n\
         ~/.pilotdeck implicitly, and has no second backend.\n\n\
         Usage:\n  {SHIM_NAME} <command> [options...]\n  {SHIM_NAME} compat manifest\n\n\
         Legacy entries and dispositions:\n",
    );
    for entry in manifest.entries() {
        let mapped = mappings
            .as_ref()
            .is_some_and(|m| m.find(entry.slug).is_some());
        out.push_str(&format!(
            "  {:<58} {:<10} {}\n",
            entry.argv_patterns[0],
            entry.disposition.as_str(),
            if mapped { "" } else { "(mapping data pending)" }
        ));
    }
    out.push_str(&format!(
        "\nMigration:\n\
         1. Install and initialize the new CLI: `{TARGET_BIN_NAME} instance init`.\n\
         2. Move legacy configuration explicitly; this shim never reads ~/.pilotdeck.\n\
         3. Rejected entries fail with an explicit error code and a migration pointer.\n\
         4. `pilotdeck compat manifest` dumps the machine-readable mapping status.\n\n\
         Shim version:  {}\n\
         Contract:      {CONTRACT_VERSION}\n\
         Target binary: {}\n",
        env!("CARGO_PKG_VERSION"),
        target_line()
    ));
    out
}

/// One-line detail for `help <word>`, matched against the manifest's legacy
/// argv patterns.
fn detail_for_word(word: &str) -> Option<String> {
    let manifest = Manifest::builtin();
    let mappings = Mappings::load().ok();
    let mut out = String::new();
    let mut matched = false;
    for entry in manifest.entries() {
        if entry.argv_patterns.iter().any(|p| p.starts_with(word)) {
            matched = true;
            out.push_str(&format!(
                "{} ({}):\n",
                entry.argv_patterns[0],
                entry.disposition.as_str()
            ));
            for p in &entry.argv_patterns[1..] {
                out.push_str(&format!("  {}\n", p));
            }
            if let Some(m) = mappings.as_ref().and_then(|m| m.find(entry.slug)) {
                let ops: Vec<&str> = m.sequence.iter().map(|s| s.op.as_str()).collect();
                if !ops.is_empty() {
                    out.push_str(&format!("  target operations: {}\n", ops.join(" -> ")));
                }
            }
        }
    }
    if matched { Some(out) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Locks PATH-dependent assertions? None here: `target_line` tolerates a
    // missing target binary, so no environment mutation is needed.

    #[test]
    fn full_help_lists_all_27_entries_and_migration_notes() {
        let text = full_help();
        for entry in Manifest::builtin().entries() {
            // Each entry's first argv pattern appears in the listing.
            assert!(
                text.contains(entry.argv_patterns[0]),
                "missing {}",
                entry.argv_patterns[0]
            );
        }
        assert_eq!(text.matches("Migration:\n").count(), 1);
        assert!(text.contains("never reads"));
        assert!(text.contains(CONTRACT_VERSION));
        assert!(text.contains("compat manifest"));
    }

    #[test]
    fn detail_for_known_and_unknown_words() {
        let run = detail_for_word("run").expect("run is a known word");
        assert!(run.contains("run <prompt>"));
        assert!(run.contains("target operations: session.create -> run.submit"));
        let sessions = detail_for_word("sessions").expect("sessions known");
        assert!(sessions.contains("sessions [list]"));
        assert!(sessions.contains("sessions messages <session-id>"));
        assert!(detail_for_word("frobnicate").is_none());
    }

    #[test]
    fn version_reports_shim_contract_and_target_status() {
        // `version()` prints; assert the composed status line instead.
        let line = target_line();
        assert!(
            line.contains(TARGET_BIN_NAME) || line.contains(TARGET_BIN_ENV),
            "target line must name the target or the pin hook: {line}"
        );
    }
}
