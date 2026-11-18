//! `usage-errors` entry (`cli.compat.usage-errors`): shim-local standard
//! argument errors. Exit 2, message on stderr, no business writes, no
//! gateway, no home directory access.

use crate::{EXIT_USAGE, SHIM_NAME};

/// Emits one standard usage error and returns the exit code (2).
pub fn usage(message: &str) -> i32 {
    eprintln!("{SHIM_NAME}: {message}");
    eprintln!("{SHIM_NAME}: run `pilotdeck help` for usage");
    EXIT_USAGE
}

/// Unknown option in a known entry group.
pub fn unknown_option(option: &str) -> i32 {
    usage(&format!("unknown option `{option}`"))
}

/// Unknown subcommand inside a known command group.
pub fn unknown_subcommand(group: &str, word: &str) -> i32 {
    usage(&format!("unknown {group} subcommand `{word}`"))
}

/// Required argument missing in a known command group.
pub fn missing_argument(group: &str, what: &str) -> i32 {
    usage(&format!("{group} requires {what}"))
}

/// A reserved top-level word that must not be treated as a prompt
/// (`default-argv` entry: reserved words exit 2).
pub fn reserved_word(word: &str) -> i32 {
    usage(&format!(
        "`{word}` is a reserved word and cannot be used as a prompt argument"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_errors_exit_2() {
        assert_eq!(usage("bad"), EXIT_USAGE);
        assert_eq!(unknown_option("--bogus"), EXIT_USAGE);
        assert_eq!(unknown_subcommand("cron", "frob"), EXIT_USAGE);
        assert_eq!(missing_argument("run", "a <prompt>"), EXIT_USAGE);
        assert_eq!(reserved_word("update"), EXIT_USAGE);
    }
}
