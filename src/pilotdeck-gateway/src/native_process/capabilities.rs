//! The capability list `describe_server` advertises, in one table.
//!
//! TS baseline: `resources/PilotDeck/src/gateway/client/InProcessGateway.ts:2088-2163`
//! builds the array from `this.options.<callback>` presence, and `hello_ok`
//! forwards that same `describeServer()` (`server/GatewayWsConnection.ts:112-115`).
//! The recorded output of the baseline for the three fixture config states is
//! `tests/fixtures/native-management/oracle.json` (`contract.capabilities`); the output of a real
//! `pilotdeck server` is compared by `scripts/bbx`.
//!
//! One entry per advertised name; `methods` is the wire set the name promises
//! and `wired` is the provider probe. The list is an observable output
//! (`hello_ok.serverInfo.capabilities` and `describe_server`), so names and
//! order follow what the baseline emits. Names whose methods this process does
//! not serve yet are advertised all the same and listed with what they answer
//! in [`ADVERTISED_NOT_SERVED`].

use super::methods::registered_methods;
use super::requests::Requests;

/// One advertised capability.
pub(super) struct Capability {
    /// The name that appears in `capabilities`.
    pub(super) name: &'static str,
    /// The wire methods the name promises. Empty means "the method whose name
    /// is the capability name", which is the shape of all but three entries;
    /// the exceptions spell their methods out.
    pub(super) methods: &'static [&'static str],
    /// The provider probe. `false` means the delegate is not bound in this
    /// process, so advertising the name would promise a call that can only
    /// answer `CAPABILITY_UNAVAILABLE`.
    pub(super) wired: fn(&Requests) -> bool,
}

/// A provider with no configuration axis in this process: the delegate is
/// compiled in and always bound (e.g. `upload_gateway`, `mcp`, the registry).
fn compiled_in(_: &Requests) -> bool {
    true
}

/// `options.runRegistry`: reserved runs until the registry is open.
fn runs(requests: &Requests) -> bool {
    requests.runs.is_some()
}

/// `options.cron`: the six control names need the configured cron runtime.
fn cron(requests: &Requests) -> bool {
    requests.cron.is_some()
}

/// Advertised names, in the order the TS baseline emits them
/// (`InProcessGateway.ts:2089-2156`). `pilotdeck server` on the baseline binds
/// every delegate below unconditionally except cron, so the recorded
/// `hello_ok` of an unconfigured server carries 53 names
/// (`scripts/bbx`, scenario `server-entry-handshake`).
// One row per name: the table is read as a table (and diffed against the
// baseline output row by row), so it is laid out by hand rather than exploded
// into five lines per entry by rustfmt.
#[rustfmt::skip]
pub(super) const CAPABILITIES: &[Capability] = &[
    Capability { name: "upload_create", methods: &[], wired: compiled_in },
    Capability { name: "upload_get", methods: &[], wired: compiled_in },
    Capability { name: "upload_part", methods: &[], wired: compiled_in },
    Capability { name: "upload_complete", methods: &[], wired: compiled_in },
    Capability { name: "upload_cancel", methods: &[], wired: compiled_in },
    Capability { name: "run_get", methods: &[], wired: runs },
    Capability { name: "run_events", methods: &[], wired: runs },
    Capability { name: "run_reattach", methods: &[], wired: runs },
    Capability { name: "project_files_list", methods: &[], wired: compiled_in },
    Capability { name: "commands_list", methods: &[], wired: compiled_in },
    Capability { name: "model_catalog_list", methods: &[], wired: compiled_in },
    Capability { name: "session_model_get", methods: &[], wired: compiled_in },
    Capability { name: "session_model_set", methods: &[], wired: compiled_in },
    Capability { name: "session_model_clear", methods: &[], wired: compiled_in },
    Capability { name: "delete_session", methods: &[], wired: compiled_in },
    // Listed in [`ADVERTISED_NOT_SERVED`].
    Capability {
        name: "session_transcript_archive",
        methods: &["export_session_transcript", "restore_session_transcript"],
        wired: compiled_in,
    },
    // The name covers two requests: the manifest and its entries.
    Capability {
        name: "native_transcript_archive",
        methods: &["native_archive_manifest", "native_archive_entries"],
        wired: compiled_in,
    },
    // The artifact name is separate in TS for the same archive: it streams a
    // blob rather than a JSON projection.
    Capability {
        name: "native_transcript_archive_artifact",
        methods: &["native_archive_artifact"],
        wired: compiled_in,
    },
    // The baseline binds memory, snapshots and always-on control in every
    // `pilotdeck server` (createLocalGateway.ts:1238-1271,
    // ProjectAutomationBundle.ts:364), so the names are unconditional; what
    // the methods answer when the Rust delegate is not configured is listed
    // in [`ADVERTISED_NOT_SERVED`].
    Capability { name: "memory_list", methods: &[], wired: compiled_in },
    Capability { name: "memory_wipe", methods: &[], wired: compiled_in },
    Capability { name: "manager_sessions", methods: &[], wired: compiled_in },
    Capability { name: "snapshot_list", methods: &[], wired: compiled_in },
    Capability { name: "snapshot_get", methods: &[], wired: compiled_in },
    Capability { name: "snapshot_restore", methods: &[], wired: compiled_in },
    // Gated on `options.cron` (`InProcessGateway.ts:2114-2123`).
    // `CRON_METHODS` owns both sides (#1325).
    Capability { name: "cron_create", methods: &[], wired: cron },
    Capability { name: "cron_list", methods: &[], wired: cron },
    Capability { name: "cron_update", methods: &[], wired: cron },
    Capability { name: "cron_delete", methods: &[], wired: cron },
    Capability { name: "cron_stop", methods: &[], wired: cron },
    Capability { name: "cron_run_now", methods: &[], wired: cron },
    Capability { name: "always_on_apply", methods: &[], wired: compiled_in },
    Capability { name: "always_on_abort", methods: &[], wired: compiled_in },
    Capability { name: "always_on_rerun_plan", methods: &[], wired: compiled_in },
    Capability { name: "mcp_server_status", methods: &[], wired: compiled_in },
    Capability { name: "set_mcp_servers", methods: &[], wired: compiled_in },
    Capability { name: "mcp_server_reconnect", methods: &[], wired: compiled_in },
    Capability { name: "mcp_server_toggle", methods: &[], wired: compiled_in },
    Capability { name: "set_mcp_permission_mode_override", methods: &[], wired: compiled_in },
    Capability { name: "project_file_read", methods: &[], wired: compiled_in },
    Capability { name: "set_permission_mode", methods: &[], wired: compiled_in },
    Capability { name: "apply_flag_settings", methods: &[], wired: compiled_in },
    Capability { name: "update_settings", methods: &[], wired: compiled_in },
    Capability { name: "resolve_settings", methods: &[], wired: compiled_in },
    Capability { name: "set_session_thinking", methods: &[], wired: compiled_in },
    Capability { name: "output_styles_list", methods: &[], wired: compiled_in },
    Capability { name: "set_output_style", methods: &[], wired: compiled_in },
    Capability { name: "reload_output_styles", methods: &[], wired: compiled_in },
    Capability { name: "usage_snapshot", methods: &[], wired: compiled_in },
    Capability { name: "model_usage_snapshot", methods: &[], wired: compiled_in },
    // The capability name and the wire method differ on purpose: TS advertises
    // `async_hook_result` (`protocol/types.ts`) for the `hook_async_result`
    // request (`protocol/frames.ts:85`).
    Capability {
        name: "async_hook_result",
        methods: &["hook_async_result"],
        wired: compiled_in,
    },
    Capability { name: "user_dialog_list", methods: &[], wired: compiled_in },
    Capability { name: "permission_list", methods: &[], wired: compiled_in },
    Capability { name: "permission_decide", methods: &[], wired: compiled_in },
    Capability { name: "rewind_files", methods: &[], wired: compiled_in },
    // Unix-only in the tools family; the registry check removes them on
    // Windows without a second `#[cfg]` list here.
    Capability { name: "background_task_stop", methods: &[], wired: compiled_in },
    Capability { name: "background_tasks", methods: &[], wired: compiled_in },
    Capability { name: "seed_read_state", methods: &[], wired: compiled_in },
    // The baseline advertises this for `set_sdk_session_config`
    // (`InProcessGateway.ts:2150`); listed in [`ADVERTISED_NOT_SERVED`].
    Capability { name: "sdk_session_config", methods: &[NO_WIRE_METHOD], wired: compiled_in },
    Capability { name: "supported_agents", methods: &[], wired: compiled_in },
];

/// Marker for a capability whose request this process does not route at all.
/// It is skipped by the registry check so the name is still advertised the way
/// the baseline advertises it; every such row must be in
/// [`ADVERTISED_NOT_SERVED`].
const NO_WIRE_METHOD: &str = "";

/// Names advertised because the baseline advertises them, whose methods this
/// process does not (fully) serve yet. The advertisement follows the baseline
/// (the list is an observable output); the gap is the method behind it.
///
/// `(name, what the call answers today)`.
// read by the capability tests only; kept in the shipping build so the gap
// list sits next to the table
#[allow(dead_code)]
pub(super) const ADVERTISED_NOT_SERVED: &[(&str, &str)] = &[
    (
        "session_transcript_archive",
        "export/restore_session_transcript answer CAPABILITY_UNAVAILABLE (methods/history.rs)",
    ),
    (
        "sdk_session_config",
        "set_sdk_session_config is not routed by the registry (methods/mod.rs)",
    ),
    (
        "memory_list",
        "served only when the memory runtime is configured (assembly/memory.rs)",
    ),
    (
        "memory_wipe",
        "served only when the memory runtime is configured (assembly/memory.rs)",
    ),
    (
        "snapshot_list",
        "served only with an explicit --storage-root (assembly/snapshot.rs)",
    ),
    (
        "snapshot_get",
        "served only with an explicit --storage-root (assembly/snapshot.rs)",
    ),
    (
        "snapshot_restore",
        "served only with an explicit --storage-root (assembly/snapshot.rs)",
    ),
    (
        "always_on_apply",
        "served only when an always-on project is enabled (assembly/always_on.rs); the baseline has a fallback control",
    ),
    (
        "always_on_abort",
        "served only when an always-on project is enabled (assembly/always_on.rs); the baseline has a fallback control",
    ),
    (
        "always_on_rerun_plan",
        "served only when an always-on project is enabled (assembly/always_on.rs); the baseline has a fallback control",
    ),
];

/// Capability names outside this draft. The recorded fixture never configures
/// the delegate behind them, so they are neither advertised nor an exception
/// to the recorded set; they are listed so the boundary is readable in the
/// tree and so adding one without deleting its line fails a test.
#[allow(dead_code)] // read by the capability tests only
pub(super) const OUT_OF_SCOPE: &[(&str, &str)] = &[
    (
        "manager_browsers",
        "not this issue: the TS CLI only passes the callback through (cli/createLocalGateway.ts:1270) and nothing in the baseline tree calls it, so the baseline never advertises it either",
    ),
    (
        "sdk_session_defaults",
        "not this issue: the wire protocol declares only `sdk_session_config` (protocol/types.ts), so there is no request to serve",
    ),
];

/// The wire methods a capability promises.
fn promised(capability: &Capability) -> impl Iterator<Item = &'static str> {
    let explicit = capability
        .methods
        .iter()
        .copied()
        .filter(|method| *method != NO_WIRE_METHOD);
    let from_name = std::iter::once(capability.name).filter(move |_| capability.methods.is_empty());
    explicit.chain(from_name)
}

/// Does this process route every method the capability promises?
///
/// Split out from [`advertised`] so a test can drive it with a registry that
/// is missing one method — the "advertised but not wired" failure — without
/// having to unregister anything for real.
fn methods_registered(capability: &Capability, registered: &[&str]) -> bool {
    promised(capability).all(|method| registered.contains(&method))
}

/// Names advertised for this process, in table order.
pub(super) fn advertised(requests: &Requests) -> Vec<&'static str> {
    let registered: Vec<&'static str> = registered_methods().collect();
    CAPABILITIES
        .iter()
        .filter(|capability| (capability.wired)(requests))
        .filter(|capability| methods_registered(capability, &registered))
        .map(|capability| capability.name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{negative_assert, negative_assert_eq};

    const ORACLE: &str = include_str!("../../tests/fixtures/native-management/oracle.json");

    /// The names the TS baseline recorded in one config state. Held in a
    /// `OnceLock` so they can be compared as `&'static str`.
    fn recorded_capabilities(state: &str) -> Vec<&'static str> {
        static PARSED: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        let oracle =
            PARSED.get_or_init(|| serde_json::from_str(ORACLE).expect("#1352 oracle parses"));
        let contract = &oracle["contract"];
        let node = if state == "contract" {
            contract
        } else {
            &contract[state]
        };
        node["capabilities"]
            .as_array()
            .unwrap_or_else(|| panic!("#1352 oracle capabilities missing at contract.{state}"))
            .iter()
            .map(|value| value.as_str().expect("#1352 oracle capability string"))
            .collect()
    }

    /// The names the TS baseline recorded in `contract.capabilities`.
    fn recorded_names() -> Vec<&'static str> {
        recorded_capabilities("contract")
    }

    fn table_names() -> Vec<&'static str> {
        CAPABILITIES
            .iter()
            .map(|capability| capability.name)
            .collect()
    }

    /// A name advertised twice would make every set comparison below vacuous
    /// on one of the copies.
    #[test]
    fn capability_table_has_no_duplicate_names() {
        let mut names = table_names();
        names.sort_unstable();
        let total = names.len();
        names.dedup();
        negative_assert!(names.len() == total, "#1352 capability names are unique");
    }

    /// The fail-closed half of #1352 rule 1: an advertised name must promise
    /// only methods the #1305 registry routes. A method that is missing would
    /// otherwise be promised by `describe_server` and then answer
    /// `CAPABILITY_UNAVAILABLE`.
    ///
    /// `methods/tools.rs` registers the two background names only on unix, so
    /// off unix the registry gate is precisely what has to drop those two rows.
    /// Asserting the expectation in both directions (`true` on unix, exactly
    /// those two `false` elsewhere) keeps this test honest on either platform
    /// instead of turning into a false FAIL on Windows.
    #[test]
    fn capability_methods_are_registered() {
        let registered: Vec<&str> = registered_methods().collect();
        for capability in CAPABILITIES {
            let unix_only = matches!(capability.name, "background_task_stop" | "background_tasks");
            negative_assert_eq!(
                methods_registered(capability, &registered),
                cfg!(unix) || !unix_only,
                "#1352 {} promises a method the registry does not route: {:?}",
                capability.name,
                promised(capability).collect::<Vec<_>>()
            );
        }
    }

    /// The same property driven from the other side: drop one promised method
    /// from the registry and the capability must stop being advertised. Rows
    /// with no routed method at all (`NO_WIRE_METHOD`) have nothing to drop.
    #[test]
    fn unregistered_method_suppresses_advertisement() {
        for capability in CAPABILITIES {
            let mut partial: Vec<&str> = promised(capability).collect();
            if partial.pop().is_none() {
                negative_assert!(
                    ADVERTISED_NOT_SERVED
                        .iter()
                        .any(|(name, _)| *name == capability.name),
                    "{} promises no method and is not declared in ADVERTISED_NOT_SERVED",
                    capability.name
                );
                continue;
            }
            negative_assert!(
                !methods_registered(capability, &partial),
                "#1352 {} stays advertised with a promised method missing from the registry",
                capability.name
            );
            let full: Vec<&str> = promised(capability).collect();
            negative_assert!(
                methods_registered(capability, &full),
                "#1352 {} is suppressed even with every promised method registered",
                capability.name
            );
        }
    }

    /// Every name the recorded fixture carries is in the table, in the same
    /// relative order, and the table invents nothing: the only names beyond
    /// the fixture are cron (gated on `options.cron`) and the three always-on
    /// names, which the fixture's host did not bind but `pilotdeck server`
    /// does (bbx `server-entry-handshake`).
    #[test]
    fn capability_table_covers_every_recorded_name() {
        let recorded = recorded_names();
        let table = table_names();
        let in_fixture: Vec<&str> = table
            .iter()
            .copied()
            .filter(|name| recorded.contains(name))
            .collect();
        negative_assert!(
            in_fixture == recorded,
            "capability table does not carry the recorded names in recorded order: {in_fixture:?} vs {recorded:?}"
        );
        for name in &table {
            negative_assert!(
                recorded.contains(name)
                    || crate::runtime::cron::CRON_METHODS.contains(name)
                    || crate::runtime::always_on::ALWAYS_ON_METHODS.contains(name),
                "#1352 {name} is advertised but the TS baseline does not record it and it is neither a cron nor an always-on name"
            );
        }
    }

    /// Every declared gap names an advertised row and says what it answers.
    #[test]
    fn exception_entries_carry_a_reason() {
        for (name, reason) in ADVERTISED_NOT_SERVED {
            negative_assert!(!reason.trim().is_empty(), "#1352 {name} has no reason");
            negative_assert!(
                table_names().contains(name),
                "{name} is declared as a gap but is not advertised"
            );
        }
    }

    /// The declared out-of-scope names stay out: none of them may creep into
    /// the advertised table without deleting its line here.
    #[test]
    fn out_of_scope_names_are_not_advertised() {
        let recorded = recorded_names();
        for (name, reason) in OUT_OF_SCOPE {
            negative_assert!(!reason.trim().is_empty(), "#1352 {name} has no reason");
            negative_assert!(
                !table_names().contains(name),
                "#1352 out-of-scope name {name} is advertised"
            );
            negative_assert!(
                !recorded.contains(name),
                "#1352 out-of-scope name {name} is recorded by the fixture; add it to the table"
            );
        }
    }
}
