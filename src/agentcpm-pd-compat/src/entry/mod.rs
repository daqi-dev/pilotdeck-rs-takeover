//! Entry modules: one file per legacy entry group, dispatched from
//! [`crate::run`]. Local entries (`help`, `reject`, `usage_errors`) never
//! start a gateway and never read a home directory; orchestration entries
//! receive an [`EntryCtx`] (an [`crate::executor::Executor`] plus the
//! explicitly resolved shim home) and perform every business call through
//! the executor.

pub mod chat_search;
pub mod cron;
pub mod guardian;
pub mod help;
pub mod reject;
pub mod resume;
pub mod run;
pub mod server;
pub mod sessions;
pub mod settings;
pub mod setup;
pub mod skills;
pub mod usage_errors;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::config::{self, CompatConfig, ConfigError};
use crate::executor::Executor;
use crate::{EXIT_FAILED, EXIT_RESULT_UNKNOWN, SHIM_NAME};

/// Context handed to every orchestration entry: the executor plus the
/// resolved shim home. Production resolves the home from the environment;
/// tests inject an isolated temporary directory (the real `~/.pilotdeck`
/// and `~/.agentcpm-pd` are never touched).
pub struct EntryCtx<'a> {
    executor: &'a mut dyn Executor,
    home: Result<PathBuf, ConfigError>,
}

impl<'a> EntryCtx<'a> {
    /// Production construction: the home comes from `AGENTCPM_PD_HOME`.
    pub fn from_env(executor: &'a mut dyn Executor) -> Self {
        let home = config::resolve_home(std::env::var_os(config::HOME_ENV).as_deref());
        Self { executor, home }
    }

    /// Test construction with an isolated home directory.
    pub fn with_home(executor: &'a mut dyn Executor, home: &Path) -> Self {
        Self {
            executor,
            home: Ok(home.to_path_buf()),
        }
    }

    /// The executor for business calls.
    pub fn executor(&mut self) -> &mut dyn Executor {
        self.executor
    }

    /// The resolved home, or `configuration_required` already emitted
    /// (`Err(exit code)`). Help/manifest/rejection paths never call this.
    pub fn require_home(&self) -> Result<&Path, i32> {
        match &self.home {
            Ok(home) => Ok(home),
            Err(e) => {
                eprintln!("{SHIM_NAME}: {e}");
                Err(EXIT_FAILED)
            }
        }
    }

    /// Loads the shim config; a missing file is an empty config, an invalid
    /// document is a surfaced failure (`Err(exit code)`).
    pub fn load_config(&self, home: &Path) -> Result<CompatConfig, i32> {
        match config::load(home) {
            Ok(Some(config)) => Ok(config),
            Ok(None) => Ok(CompatConfig::default()),
            Err(e) => {
                eprintln!("{SHIM_NAME}: {e}");
                Err(EXIT_FAILED)
            }
        }
    }

    /// Loads the legacy→new session mapping table (missing file = empty).
    pub fn load_sessions(&self, home: &Path) -> Result<crate::state::SessionMap, i32> {
        match crate::state::SessionMap::load(home) {
            Ok(map) => Ok(map),
            Err(message) => {
                eprintln!("{SHIM_NAME}: compat session mapping failed to load: {message}");
                Err(EXIT_FAILED)
            }
        }
    }
}

/// Emits a business failure (a `failed` envelope) faithfully and returns the
/// matching exit code: `RESULT_UNKNOWN` maps to exit 3, everything else to
/// exit 1. The error code and message are passed through verbatim —
/// `CAPABILITY_UNAVAILABLE` from a pending-S3 operation is surfaced, never
/// swallowed or turned into fake success.
pub(crate) fn surface_failure(method: &str, error: &crate::executor::ErrorPayload) -> i32 {
    eprintln!(
        "{SHIM_NAME}: {method} failed: {}: {}",
        error.code, error.message
    );
    if error.code == "RESULT_UNKNOWN" {
        EXIT_RESULT_UNKNOWN
    } else {
        EXIT_FAILED
    }
}

/// Emits a transport-level failure of one invocation.
pub(crate) fn surface_exec_error(method: &str, error: &crate::executor::ExecError) -> i32 {
    eprintln!("{SHIM_NAME}: {method} failed: {error}");
    EXIT_FAILED
}

/// Invokes one operation and unwraps its result, surfacing any failure the
/// standard way (`Err(exit code)`).
pub(crate) fn call_op(ctx: &mut EntryCtx<'_>, method: &str, input: &Value) -> Result<Value, i32> {
    match ctx.executor().invoke(method, input) {
        Ok(resp) => unwrap_result(method, &resp),
        Err(e) => Err(surface_exec_error(method, &e)),
    }
}

/// [`call_op`] with an explicit persistent request key (idempotent writes).
pub(crate) fn call_op_keyed(
    ctx: &mut EntryCtx<'_>,
    method: &str,
    input: &Value,
    request_key: &str,
) -> Result<Value, i32> {
    match ctx.executor().invoke_keyed(method, input, request_key) {
        Ok(resp) => unwrap_result(method, &resp),
        Err(e) => Err(surface_exec_error(method, &e)),
    }
}

/// Turns a response envelope into its result payload.
pub(crate) fn unwrap_result(method: &str, resp: &crate::executor::Response) -> Result<Value, i32> {
    if resp.is_success() {
        Ok(resp.result.clone().unwrap_or(Value::Null))
    } else {
        match &resp.error {
            Some(error) => Err(surface_failure(method, error)),
            None => {
                eprintln!("{SHIM_NAME}: {method} failed without an error payload");
                Err(EXIT_FAILED)
            }
        }
    }
}

/// Reports old fields that cannot be projected onto the new contract
/// (one stderr line; stdout output stays clean). The mapping data requires
/// unprojectable fields to be listed, never silently dropped.
pub(crate) fn note_dropped(context: &str, fields: &[&str]) {
    if !fields.is_empty() {
        eprintln!(
            "{SHIM_NAME}: note: {context}: not projectable onto the new contract: {}",
            fields.join(", ")
        );
    }
}

/// Total wall-clock budget for one cursor-paged loop (the two `run.events`
/// waits and the five list/history paging loops).
///
/// The baseline SDK waited for the real terminal state with no timeout; the
/// page-count bound this replaces (600 pages × 200 ms sleep ≈ 120 s, burned
/// in milliseconds by non-sleeping `caughtUp:false` backlog pages) killed
/// healthy runs — long tool chains and slow models routinely exceed two
/// minutes. 30 minutes keeps the "wait for the real terminal state"
/// semantics for any realistic run while still guaranteeing termination
/// against a stuck target (the shim must never hang forever; exit 1 lets
/// the SIGINT guardian clean an unsettled run up).
pub(crate) const PAGE_WAIT_WALL_CLOCK: Duration = Duration::from_secs(30 * 60);

/// Stagnation budget: how many consecutive no-progress pages one paged loop
/// tolerates before failing. A page counts as stagnant only when it is NOT
/// a target-sanctioned wait: a caught-up wait page (`caughtUp:true`, no new
/// rows) sleeps [`crate::entry::run::EVENTS_POLL_INTERVAL`] between polls and
/// is bounded by the wall clock alone, while a backlog page never sleeps.
/// Progress means new rows for the `run.events` waits or an advanced
/// continuation cursor for every loop (a page answering the cursor the
/// request carried re-delivers the same page); per the gateway contract a
/// healthy backlog page always carries rows or an advanced cursor
/// (store.rs: an empty page answers `nextCursor:null`). 100 consecutive
/// no-advance, no-sleep pages is therefore an unambiguously stuck target,
/// caught in milliseconds instead of burning the whole wall clock (100, not
/// 10, tolerates transient empty backlog pages).
pub(crate) const PAGE_STAGNATION_LIMIT: u32 = 100;

/// Shared budget for one cursor-paged loop: a total wall-clock deadline plus
/// a stagnation counter for consecutive pages that made no progress without
/// the target sanctioning a wait. Used by every paging loop in the crate
/// (the `run.events` waits in `entry::run` / `lib::prompt_turn` and the five
/// list/history loops), so a target that never ends its cursor chain is an
/// honest failure, never a hang.
pub(crate) struct PageBudget {
    /// Loop description for the failure message (e.g. "run.events for run
    /// r-1", "session.list for project p-1").
    label: String,
    wall_clock: Duration,
    deadline: Instant,
    stagnant: u32,
}

impl PageBudget {
    /// Starts the [`PAGE_WAIT_WALL_CLOCK`] budget for one loop.
    pub(crate) fn new(label: impl Into<String>) -> Self {
        Self::with_wall_clock(label, PAGE_WAIT_WALL_CLOCK)
    }

    /// [`PageBudget::new`] with an explicit wall clock (tests use fast
    /// values; production always passes [`PAGE_WAIT_WALL_CLOCK`]).
    pub(crate) fn with_wall_clock(label: impl Into<String>, wall_clock: Duration) -> Self {
        Self {
            label: label.into(),
            wall_clock,
            deadline: Instant::now() + wall_clock,
            stagnant: 0,
        }
    }

    /// Records one fetched page; `Err` carries the failure message.
    ///
    /// - `progressed`: the page advanced the query (new rows for the event
    ///   waits, an advanced continuation cursor for every loop) — resets the
    ///   stagnation counter;
    /// - `slept`: the loop slept after the page (a target-sanctioned wait,
    ///   `caughtUp:true`) — the stagnation counter stays untouched, the wall
    ///   clock alone bounds the waiting.
    ///
    /// A page that neither progressed nor slept increments the stagnation
    /// counter; reaching [`PAGE_STAGNATION_LIMIT`] fails the loop.
    pub(crate) fn record(&mut self, progressed: bool, slept: bool) -> Result<(), String> {
        if progressed {
            self.stagnant = 0;
        } else if !slept {
            self.stagnant += 1;
            if self.stagnant >= PAGE_STAGNATION_LIMIT {
                return Err(format!(
                    "{} stalled: {} consecutive pages made no progress (the continuation \
                     cursor did not advance)",
                    self.label, PAGE_STAGNATION_LIMIT
                ));
            }
        }
        if Instant::now() >= self.deadline {
            return Err(format!(
                "{} exceeded the page-wait wall clock ({:?})",
                self.label, self.wall_clock
            ));
        }
        Ok(())
    }
}

/// Normalizes one page's continuation cursor for a paging loop: `null`,
/// absent and empty-string cursors all end the loop (paging with `""` is
/// meaningless — a target answering `""` forever must not hang the shim),
/// otherwise returns the next cursor together with whether it ADVANCED over
/// the cursor the request carried. A page answering the same cursor the
/// request sent re-delivers the same page — that is never progress, so it
/// feeds the [`PageBudget`] stagnation counter.
pub(crate) fn page_cursor_step(
    previous: Option<&str>,
    next: Option<&str>,
) -> (Option<String>, bool) {
    let next = next.filter(|s| !s.is_empty());
    let advanced = match (previous, next) {
        (None, None) => false,
        (None, Some(_)) => true,
        (Some(_), None) => false,
        (Some(prev), Some(next)) => prev != next,
    };
    (next.map(str::to_owned), advanced)
}

/// Option specification for the tiny shared argv parser.
pub(crate) struct OptSpec {
    /// Options that carry a value (`--name value` / `--name=value`).
    pub values: &'static [&'static str],
    /// Boolean flags.
    pub flags: &'static [&'static str],
}

/// Result of the shared argv parser.
#[derive(Clone, Debug, Default)]
pub struct Parsed {
    pub positionals: Vec<String>,
    pub flags: Vec<String>,
    pub values: Vec<(String, String)>,
}

impl Parsed {
    pub fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }

    pub fn value(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn values_of(&self, name: &str) -> Vec<&str> {
        self.values
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .collect()
    }
}

/// Minimal long-option parser shared by the entry modules. Unknown options,
/// values on valueless flags, and missing values are usage errors (message
/// without the `pilotdeck:` prefix; callers emit via `usage_errors::usage`).
pub(crate) fn parse_cli(rest: &[String], spec: &OptSpec) -> Result<Parsed, String> {
    let mut out = Parsed::default();
    let mut i = 0;
    while i < rest.len() {
        let token = &rest[i];
        if token == "--" {
            out.positionals.extend(rest[i + 1..].iter().cloned());
            break;
        }
        let Some(body) = token.strip_prefix("--") else {
            out.positionals.push(token.clone());
            i += 1;
            continue;
        };
        let (name, inline) = match body.split_once('=') {
            Some((n, v)) => (n.to_owned(), Some(v.to_owned())),
            None => (body.to_owned(), None),
        };
        // Spec entries carry their `--` prefixes; compare the full form.
        let long = format!("--{name}");
        if spec.values.contains(&long.as_str()) {
            let value = match inline {
                Some(v) => v,
                None => match rest.get(i + 1) {
                    Some(v) if !v.starts_with('-') => {
                        i += 1;
                        v.clone()
                    }
                    _ => return Err(format!("option {long} requires a value")),
                },
            };
            out.values.push((long, value));
        } else if spec.flags.contains(&long.as_str()) {
            if inline.is_some() {
                return Err(format!("option {long} does not take a value"));
            }
            out.flags.push(long);
        } else {
            return Err(format!("unknown option {long}"));
        }
        i += 1;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_budget_stalls_only_on_consecutive_no_progress_pages() {
        let mut budget = PageBudget::new("unit loop");
        // 99 stagnant pages stay inside the budget.
        for _ in 0..(PAGE_STAGNATION_LIMIT - 1) {
            assert!(budget.record(false, false).is_ok());
        }
        // The PAGE_STAGNATION_LIMIT-th stagnant page fails.
        let err = budget.record(false, false).unwrap_err();
        assert!(err.contains("stalled"), "message: {err}");
        assert!(err.contains("unit loop"), "message: {err}");

        // Progress resets the counter: 99 stagnant, progress, 99 stagnant.
        let mut budget = PageBudget::new("unit loop");
        for _ in 0..(PAGE_STAGNATION_LIMIT - 1) {
            assert!(budget.record(false, false).is_ok());
        }
        assert!(budget.record(true, false).is_ok());
        for _ in 0..(PAGE_STAGNATION_LIMIT - 1) {
            assert!(budget.record(false, false).is_ok());
        }
        assert!(budget.record(false, false).is_err());
    }

    #[test]
    fn page_budget_sanctioned_waits_never_stall() {
        // A caught-up wait page (no rows, no cursor advance) sleeps between
        // polls; the wall clock alone bounds it — thousands of them must not
        // trip the stagnation counter (the old page-count budget killed
        // healthy silent waits this way).
        let mut budget = PageBudget::new("unit loop");
        for _ in 0..(PAGE_STAGNATION_LIMIT * 10) {
            assert!(budget.record(false, true).is_ok());
        }
    }

    #[test]
    fn page_budget_wall_clock_fails_the_wait() {
        let mut budget = PageBudget::with_wall_clock("unit loop", Duration::ZERO);
        let err = budget.record(true, false).unwrap_err();
        assert!(err.contains("wall clock"), "message: {err}");
        assert!(err.contains("unit loop"), "message: {err}");
    }

    #[test]
    fn page_cursor_step_treats_empty_and_missing_as_terminal() {
        assert_eq!(page_cursor_step(None, None), (None, false));
        // An empty-string cursor is not a continuation (paging with "" is
        // meaningless): it terminates like null.
        assert_eq!(page_cursor_step(None, Some("")), (None, false));
        assert_eq!(page_cursor_step(Some("c1"), Some("")), (None, false));
        assert_eq!(page_cursor_step(Some("c1"), None), (None, false));
        // Advancing cursors are progress.
        assert_eq!(
            page_cursor_step(None, Some("c1")),
            (Some("c1".into()), true)
        );
        assert_eq!(
            page_cursor_step(Some("c1"), Some("c2")),
            (Some("c2".into()), true)
        );
        // The same cursor the request carried re-delivers the same page:
        // never progress.
        assert_eq!(
            page_cursor_step(Some("c1"), Some("c1")),
            (Some("c1".into()), false)
        );
    }
}
