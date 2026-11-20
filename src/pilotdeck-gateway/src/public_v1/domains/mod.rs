//! Domain handler registry for `public_v1` dispatch (S3 substrate).
//!
//! Each domain file owns its operations end to end: it registers the
//! `/v1/ws` dispatch handlers (`reg.handler("<op>", ...)`), any store DDL it
//! needs (`reg.schema(...)`, executed by `V1Store::init_schema`), and — for
//! operations implemented outside the dispatch table (the CLI-local
//! lifecycle ops owned by `bootstrap.rs`) — an availability-only entry
//! (`reg.available(...)`). `runtime.rs::dispatch` keeps only the shared
//! pre-checks (local-execution refusal, instanceId injection, availability
//! gate, idempotency-key requirement, input validation) and then forwards to
//! the registered handler; an unregistered operation answers
//! `-32004 CAPABILITY_UNAVAILABLE` exactly as before.
//!
//! Adding an operation in S3 means touching this module list once (two
//! lines: the `pub mod` declaration plus the `register` call) and the new
//! domain file — nothing else in the shared path changes.
// cli.task.automation (the 3 work_cycles.* operations: list / apply with the
// two-CAS change-set application / archive; see work_cycles.rs's module doc).
pub mod work_cycles;
pub mod workspace_dirs;
pub mod workspaces;

pub mod access_keys;
// cli.task.automation: the external-request composition workflow (plan /
// submit / cleanup over the public workspaces.prepare, run.submit and git.*
// operations; see agent_workflow.rs's module doc).
pub mod agent_workflow;
pub mod artifacts;
pub mod background_tasks;
// cli.task.automation (shared core + the 4 automation.* operations over the
// generic continuous-task plan registry; see automation.rs's module doc).
pub mod automation;
// t019 T2 channels domain: the 8 channels.* operations (configuration +
// generation state machine + probe + login challenges over the 19 adapter
// types).
pub mod channels;
pub mod commands;
pub mod configuration;
// S3 cli.task.context domain: the 5 context.* operations (style catalog,
// session thinking/style overrides, explicit compaction; see context.rs's
// module doc for the contract/baseline mapping).
pub mod context;
pub mod credentials;
// t020 data backup domain: data.create_backup (async) plus the shared
// scan/backup core the CLI-local data.* operations reuse.
pub mod data_backup;
// t021 diagnose-and-release domain: installation.check/apply/
// control_update/restart/diagnose plus the release/layout/channel core.
pub mod installation;
// t020 migration domain: migration.stage/verify/commit/recover (async) over
// the isolated staging + legacy_id_map.
pub mod discovery;
pub mod documents;
pub mod files;
pub mod git;
pub mod git_identity;
pub mod history_archive;
pub mod migration;
// cli.task.hooks hooks domain: the 4 hooks.* operations (binding
// configuration with post-write schema re-verification + the async
// invocation/receipt flow; see hooks.rs's module doc for the fire-seam
// rulings).
pub mod hooks;
// cli.task.runtime: the generic module-host capability surface
// (host.describe / prepare_model / stream_model).
pub mod host_ops;
pub mod input;
// t019 T2 integrations domain: integrations.detect (the Taskmaster probe).
pub mod integrations;
pub mod interaction;
// t011 interaction lease domain: the cross-connection dialog edit lease
// (interaction.claim / interaction.release).
pub mod interaction_lease;
// cli.task.runtime: the K1 generic knowledge module binding (inspect /
// configure / validate / query / citation.resolve).
pub mod knowledge;
// extensions-management MCP domain: the 8 mcp.* operations (config surface +
// schema validation + session overrides; see mcp.rs's module doc for the
// v1 thin-implementation rulings).
pub mod mcp;
// t018 memory domain: the 16 memory.* operations (v1-native storage; see
// memory.rs's module doc for the EdgeClaw reuse assessment).
pub mod memory;
pub mod model_references;
pub mod models;
pub mod notifications_prefs;
// t019 T2 notifications preferences domain: get/set_preferences (the
// events STREAM belongs to T3 and is deliberately not registered here).
// t019 T3 notifications stream domain: the notifications.events STREAM
// (derived run/interaction lifecycle notifications over the sub pump).
pub mod notifications_stream;
pub mod operations_ops;
// cli.task.hooks plugins domain: the 7 plugins.* operations (install/update/
// remove lifecycle with source validation, configuration + generation CAS,
// and the honest-unavailability invocation surface).
pub mod plugins;
pub mod policy;
pub mod project;
pub mod project_admin;
pub mod provider_test;
pub mod providers;
pub mod queue;
// t010 queue admin domain: queue.pause/move/remove/promote_to_steer (the
// per-session queue interventions; get/resume stay in queue.rs) plus the
// gateway-side availability row for the CLI-local input.follow.
pub mod queue_admin;
pub mod replacement;
pub mod replacement_admin;
// t023 E resources watch domain: the resources.watch stream (unary page +
// conditional-polling StreamSource over the operations_ops revision
// resolver).
pub mod resources;
pub mod run;
// cli.task.runtime: the external execution environments (native/stdio/TCP
// with actual capability declaration and binding state).
pub mod runtimes;
pub mod schedules;
pub mod search;
pub mod session;
pub mod session_admin;
// extensions-management skills domain: the 8 skills.* operations (directory
// CRUD + structural validation + scan + the two-phase plan/apply import).
pub mod skills;
// t018 snapshots domain: the 4 snapshots.* operations (read surface over
// captured turn snapshots + the two-phase plan_restore/restore protocol).
pub mod snapshots;
pub mod steer;
pub mod subagents;
// t015 terminals domain: the pty stack is unix-only (rustix pty/termios,
// controlling-terminal pre_exec); non-unix builds keep the -32004 gate.
pub mod system;
// t019 T3 taskmaster domain: the 7 taskmaster.* operations (self-built
// .taskmaster structure, PRD CRUD + parse, task set read/write).
pub mod taskmaster;
#[cfg(unix)]
pub mod terminals;
#[cfg(unix)]
pub mod terminals_pty;
#[cfg(unix)]
pub mod terminals_stream;
pub mod tools;
pub mod uploads;
pub mod usage;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::registry::{OperationRegistry, mark_implemented};
use serde_json::Value;

use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};

/// One domain handler: receives the runtime (store, waiter registry,
/// workers), the validated input, and the request-scoped idempotency key /
/// expected revision; returns the operation outcome and result or a dispatch
/// error. Owned arguments keep the boxed future `'static`.
pub type DomainHandler = fn(
    Arc<PublicV1Runtime>,
    Value,
    Option<String>,
    Option<String>,
) -> Pin<
    Box<dyn Future<Output = Result<(ResponseOutcome, Value), DispatchError>> + Send>,
>;

/// Registered DDL for `V1Store::init_schema`: (table name, `CREATE TABLE IF
/// NOT EXISTS ...` statement). S2's core tables live in `store.rs`'s static
/// batch and are not migrated; new domains add tables only through this list.
pub type DomainSchemaSql = (&'static str, &'static str);

pub struct DomainRegistry {
    handlers: HashMap<&'static str, DomainHandler>,
    /// Operations implemented outside the dispatch table (CLI-local
    /// lifecycle ops owned by `bootstrap.rs`): availability-only entries so
    /// the implemented set and `system.capabilities` stay truthful.
    available_without_handler: Vec<&'static str>,
    schema_sql: Vec<DomainSchemaSql>,
}

impl DomainRegistry {
    /// Builds the registry once per process from every domain's `register`.
    /// Also marks all registered ids as implemented in the protocol crate's
    /// global set (`agentcpm_pd_protocol::registry::is_implemented`), which
    /// the availability gates (`dispatch`, `sub.open` admission) read.
    pub fn global() -> &'static Self {
        static INSTANCE: OnceLock<DomainRegistry> = OnceLock::new();
        INSTANCE.get_or_init(Self::build)
    }

    fn build() -> Self {
        let mut reg = Self {
            handlers: HashMap::new(),
            available_without_handler: Vec::new(),
            schema_sql: Vec::new(),
        };
        system::register(&mut reg);
        project::register(&mut reg);
        session::register(&mut reg);
        run::register(&mut reg);
        queue::register(&mut reg);
        queue_admin::register(&mut reg);
        interaction::register(&mut reg);
        provider_test::register(&mut reg);
        model_references::register(&mut reg);
        interaction_lease::register(&mut reg);
        configuration::register(&mut reg);
        artifacts::register(&mut reg);
        documents::register(&mut reg);
        files::register(&mut reg);
        uploads::register(&mut reg);
        policy::register(&mut reg);
        access_keys::register(&mut reg);
        workspace_dirs::register(&mut reg);
        workspaces::register(&mut reg);
        project_admin::register(&mut reg);
        session_admin::register(&mut reg);
        git::register(&mut reg);
        git_identity::register(&mut reg);
        history_archive::register(&mut reg);
        background_tasks::register(&mut reg);
        channels::register(&mut reg);
        commands::register(&mut reg);
        context::register(&mut reg);
        schedules::register(&mut reg);
        taskmaster::register(&mut reg);
        notifications_stream::register(&mut reg);
        models::register(&mut reg);
        notifications_prefs::register(&mut reg);
        operations_ops::register(&mut reg);
        resources::register(&mut reg);
        providers::register(&mut reg);
        credentials::register(&mut reg);
        usage::register(&mut reg);
        input::register(&mut reg);
        integrations::register(&mut reg);
        replacement::register(&mut reg);
        replacement_admin::register(&mut reg);
        search::register(&mut reg);
        subagents::register(&mut reg);
        steer::register(&mut reg);
        memory::register(&mut reg);
        snapshots::register(&mut reg);
        #[cfg(unix)]
        terminals::register(&mut reg);
        tools::register(&mut reg);
        runtimes::register(&mut reg);
        host_ops::register(&mut reg);
        knowledge::register(&mut reg);
        mcp::register(&mut reg);
        skills::register(&mut reg);
        hooks::register(&mut reg);
        plugins::register(&mut reg);
        agent_workflow::register(&mut reg);
        automation::register(&mut reg);
        discovery::register(&mut reg);
        work_cycles::register(&mut reg);
        data_backup::register(&mut reg);
        migration::register(&mut reg);
        installation::register(&mut reg);
        reg.finish();
        reg
    }

    /// Registers the `/v1/ws` dispatch handler for `method_id`. A duplicate
    /// registration for the same id panics at startup — two domains claiming
    /// one operation is a wiring bug, not something to resolve silently.
    pub fn handler(&mut self, method_id: &'static str, handler: DomainHandler) {
        if self.handlers.insert(method_id, handler).is_some() {
            panic!("duplicate public_v1 handler registration for `{method_id}`");
        }
    }

    /// Registers an operation as implemented without a dispatch handler
    /// (CLI-local lifecycle ops owned by `bootstrap.rs`; `/v1/ws` refuses
    /// them earlier as `execution='local'`).
    pub fn available(&mut self, method_id: &'static str) {
        self.available_without_handler.push(method_id);
    }

    /// Registers a `CREATE TABLE IF NOT EXISTS ...` statement (plus its table
    /// name, for diagnostics) that `V1Store::init_schema` executes after the
    /// core S2 schema. New domains add tables only through this hook —
    /// `store.rs` itself stays untouched.
    pub fn schema(&mut self, table: &'static str, ddl: &'static str) {
        self.schema_sql.push((table, ddl));
    }

    fn finish(&self) {
        let mut ids: Vec<&'static str> = self.handlers.keys().copied().collect();
        ids.extend_from_slice(&self.available_without_handler);
        mark_implemented(&ids);
    }

    /// The dispatch handler for `method_id`, if this build registered one.
    pub fn handler_for(&self, method_id: &str) -> Option<DomainHandler> {
        self.handlers.get(method_id).copied()
    }

    /// Whether `method_id` is implemented by this build (dispatch handler or
    /// availability-only entry). This is the gateway-side availability set:
    /// `system.capabilities` derives its entries from it.
    pub fn is_registered(&self, method_id: &str) -> bool {
        self.handlers.contains_key(method_id) || self.available_without_handler.contains(&method_id)
    }

    /// The domain-registered DDL executed by `V1Store::init_schema`.
    pub fn schema_sql(&self) -> &[DomainSchemaSql] {
        &self.schema_sql
    }
}

/// The first embedded-registry operation (in JSON order) matching `filter`
/// that no domain has registered, or `None` once every operation matching
/// the filter is implemented (S3 complete for that class). Test and
/// automation helper for the `-32004` capability-gate assertions so they
/// stay valid as the implemented set grows.
pub fn first_unimplemented_operation_where(
    filter: impl Fn(&agentcpm_pd_protocol::registry::OperationSpec) -> bool,
) -> Option<&'static str> {
    let domains = DomainRegistry::global();
    OperationRegistry::global()
        .operations()
        .iter()
        .filter(|op| filter(op) && !domains.is_registered(&op.id))
        .map(|op| op.id.as_str())
        .next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentcpm_pd_protocol::registry::{ExecutionMode, S2_IMPLEMENTED_OPERATIONS};

    #[test]
    fn substrate_registers_the_s2_baseline_and_marks_it_implemented() {
        let domains = DomainRegistry::global();
        let reg = OperationRegistry::global();
        // Every S2 op is registered (availability with or without a dispatch
        // handler) and therefore marked implemented.
        for id in S2_IMPLEMENTED_OPERATIONS {
            assert!(
                domains.is_registered(id),
                "S2 op `{id}` missing from the domain registry"
            );
            assert!(
                agentcpm_pd_protocol::registry::is_implemented(id),
                "S2 op `{id}` not marked implemented"
            );
            // Every dispatch-served S2 op has a handler; the CLI-local
            // lifecycle ops (bootstrap-owned) are availability-only. The
            // two `schema.*` locals are the exemption `dispatch` itself
            // makes (read-only, served over /v1/ws like `api connect` does).
            let op = reg.get(id).expect("S2 op in registry");
            if op.execution == ExecutionMode::Local {
                let ws_served = id == "schema.list" || id == "schema.get";
                assert_eq!(
                    domains.handler_for(id).is_some(),
                    ws_served,
                    "local op `{id}` handler presence must follow the /v1/ws local-exemption rule"
                );
            } else {
                assert!(
                    domains.handler_for(id).is_some(),
                    "remote op `{id}` must carry a dispatch handler"
                );
            }
        }
        // Unimplemented ops answer "not registered" — the probe is derived so
        // the assertion stays valid as S3 registers domains (t015 added the
        // terminals.* family, so the old hardcoded `terminals.create` probe
        // had to follow).
        if let Some(id) =
            first_unimplemented_operation_where(|op| op.execution != ExecutionMode::Local)
        {
            assert!(
                !domains.is_registered(id),
                "derived probe `{id}` must be unregistered"
            );
        }
    }

    #[test]
    fn unimplemented_operation_derivation_matches_the_filter_and_registry() {
        // Whatever the helpers return (None once S3 is complete for that
        // class) must satisfy the filter and be genuinely unregistered.
        if let Some(id) =
            first_unimplemented_operation_where(|op| op.execution != ExecutionMode::Local)
        {
            let op = OperationRegistry::global()
                .get(id)
                .expect("derived op exists");
            assert_ne!(op.execution, ExecutionMode::Local);
            assert!(!DomainRegistry::global().is_registered(id));
        }
        if let Some(id) =
            first_unimplemented_operation_where(|op| op.execution == ExecutionMode::Stream)
        {
            let op = OperationRegistry::global()
                .get(id)
                .expect("derived op exists");
            assert_eq!(op.execution, ExecutionMode::Stream);
            assert!(!DomainRegistry::global().is_registered(id));
        }
    }

    #[test]
    fn domain_schema_hook_executes_registered_ddl() {
        // The mechanism `V1Store::init_schema` relies on: registered DDL
        // must be executable against a fresh SQLite database and idempotent
        // (CREATE IF NOT EXISTS). The production registry currently
        // registers no tables; exercise the hook with a throwaway registry
        // the way store.rs consumes it.
        let mut reg = DomainRegistry {
            handlers: HashMap::new(),
            available_without_handler: Vec::new(),
            schema_sql: Vec::new(),
        };
        reg.schema(
            "substrate_probe",
            "CREATE TABLE IF NOT EXISTS substrate_probe (
                probe_id TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        );
        assert_eq!(reg.schema_sql().len(), 1);
        let conn = rusqlite::Connection::open_in_memory().expect("in-memory sqlite");
        for (_table, ddl) in reg.schema_sql() {
            conn.execute_batch(ddl).expect("domain ddl executes");
        }
        // Idempotent re-run (the store opens existing databases too).
        for (_table, ddl) in reg.schema_sql() {
            conn.execute_batch(ddl).expect("domain ddl is idempotent");
        }
        conn.execute(
            "INSERT INTO substrate_probe (probe_id, value) VALUES ('p1', 'v1')",
            [],
        )
        .expect("insert into the domain table");
    }
}
