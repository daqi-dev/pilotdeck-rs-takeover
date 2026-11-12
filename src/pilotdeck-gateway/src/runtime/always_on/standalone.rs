//! Port of TS `createApplyHandler` (src/always-on/runtime/createApplyHandler.ts:
//! 65-136): the apply path of the standalone Always-On control the baseline
//! publishes when no project enables Always-On. It reads the persisted cycle
//! (the caller did that, with the `cycle_not_found` / `missing_workspace`
//! envelopes) and runs `DiscoveryFire.runApplyPhase` over a fresh
//! dependency set: the turn facade, the shared override registry and the
//! cycle's own project stores.
use std::{
    io,
    sync::{Arc, Mutex},
};

use crate::config::session_overrides::SessionConfigOverrides;

use super::{
    AlwaysOnAgentGateway, AlwaysOnPaths, DiscoveryPlanStore, TurnEventHandler, WorkCycleRecord,
    fire::{ApplyPhaseInput, ApplyPhaseResult, DiscoveryFire, FireDeps},
};

/// `CreateApplyHandlerDeps` (createApplyHandler.ts:29-43) as the port reads
/// them.
pub struct StandaloneAlwaysOn {
    pub(crate) gateway: Arc<dyn AlwaysOnAgentGateway>,
    pub(crate) session_overrides: Arc<Mutex<SessionConfigOverrides>>,
    pub(crate) on_turn_event: Option<TurnEventHandler>,
    /// `alwaysOnConfig?.language` — the section may exist with no enabled
    /// project.
    pub(crate) language: Option<String>,
}

impl StandaloneAlwaysOn {
    /// createApplyHandler.ts:97-134: the cycle's plan projection, then the
    /// apply phase under a fresh run id. `project_key` is the raw input key
    /// (the session key and the turn's `projectKey` use it unresolved, as
    /// TS does); `paths` were resolved from it.
    pub(crate) async fn apply(
        &self,
        paths: AlwaysOnPaths,
        project_key: &str,
        cycle: &WorkCycleRecord,
        project_name: Option<&str>,
    ) -> io::Result<ApplyPhaseResult> {
        let plan_store = Arc::new(DiscoveryPlanStore::new(paths.clone()));
        let plans: Vec<_> = plan_store
            .list_records()?
            .into_iter()
            .filter(|plan| cycle.plan_ids.iter().any(|id| id == &plan.id))
            .collect();
        let fire = DiscoveryFire::new(FireDeps {
            language: self.language.clone(),
            paths,
            project_key: project_key.to_string(),
            gateway: self.gateway.clone(),
            session_overrides: self.session_overrides.clone(),
            plan_store,
            uuid: Arc::new(|| uuid::Uuid::new_v4().to_string()),
            now: Arc::new(now_millis),
            on_turn_event: self.on_turn_event.clone(),
        });
        let run_id = uuid::Uuid::new_v4().to_string();
        use super::DiscoveryFirePort;
        fire.run_apply_phase(ApplyPhaseInput {
            run_id: &run_id,
            cycle,
            plans: &plans,
            project_name,
            project_root: project_key,
        })
        .await
    }
}

fn now_millis() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as f64)
        .unwrap_or(0.0)
}
