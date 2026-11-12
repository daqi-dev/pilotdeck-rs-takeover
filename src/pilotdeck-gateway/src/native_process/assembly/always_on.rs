//! #2294: the always-on runtime's assembly half. The bundle's
//! `createAlwaysOn` conditions (no `alwaysOn` section, or no project with
//! `enabled: true`) live in `automation.rs`; this file is the factory the
//! bundle calls once they pass — the one `AlwaysOnManager::new` of the
//! native process — plus the production `AlwaysOnAgentGateway` behind
//! `bind_agent_gateway`: the abort face, `NativeAlwaysOnGatewayBody`, which
//! ends in the gateway's synchronous abort prefix exactly like the cron
//! counterpart (`assembly/cron.rs`).
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use pilotdeck_protocol::{JsString, JsValue};

use super::automation::{
    AlwaysOnBindHooks, AlwaysOnFactory, AutomationFuture, ProjectAutomationAgentGateway,
    ProjectAutomationProvider,
};
use crate::runtime::{
    always_on::{
        AlwaysOnAgentGateway, AlwaysOnManager, AlwaysOnManagerOptions, AlwaysOnTurnStream,
        GatewayAbortInput,
    },
    session::SessionOwner,
    turn::submit::{TurnSubmission, TurnSubmitter},
};

/// The native process's one `AlwaysOnManager::new` (test fixtures construct
/// their own). The bundle only calls this with a live config — every create
/// condition is its (:287-288) decision.
pub(super) fn native_factory(hooks: NativeAlwaysOnHooks) -> AlwaysOnFactory {
    Arc::new(move |input| {
        let manager = AlwaysOnManager::new(AlwaysOnManagerOptions {
            config: input.config,
            pilot_home: input.pilot_home,
            now: None,
            uuid: None,
            logger: None,
            session_overrides: Some(hooks.session_overrides.clone()),
            on_turn_event: hooks.on_turn_event.clone(),
        })?;
        Ok(Arc::new(NativeAlwaysOnProvider {
            manager: Arc::new(manager),
        }))
    })
}

/// What the process hands every always-on manager it builds — the shared
/// session override registry and `onAlwaysOnTurnEvent`
/// (src/cli/pilotdeck.ts:82-84) — and the standalone control too.
#[derive(Clone)]
pub(crate) struct NativeAlwaysOnHooks {
    pub session_overrides:
        Arc<std::sync::Mutex<crate::config::session_overrides::SessionConfigOverrides>>,
    pub on_turn_event: Option<crate::runtime::always_on::TurnEventHandler>,
}

/// The abort face the always-on control half calls
/// (`AlwaysOnRuntime.ts:387-392`): the gateway's synchronous abort prefix
/// (`start_gateway_abort`, InProcessGateway.ts:2000-2013), whose error is the
/// `Error.message` the TS catch reads.
pub(super) struct NativeAlwaysOnGatewayBody {
    owner: SessionOwner,
    submitter: Arc<TurnSubmitter>,
}

impl NativeAlwaysOnGatewayBody {
    pub(super) fn new(owner: SessionOwner, submitter: Arc<TurnSubmitter>) -> Self {
        Self { owner, submitter }
    }
}

/// The live gateway's `submitTurn` iterator as an always-on phase drains it.
struct NativeAlwaysOnTurn(TurnSubmission);

impl AlwaysOnTurnStream for NativeAlwaysOnTurn {
    fn next_event(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<JsValue>, String>> + Send + '_>> {
        Box::pin(async move { self.0.next().await.map_err(|error| error.message) })
    }
}

impl AlwaysOnAgentGateway for NativeAlwaysOnGatewayBody {
    fn abort_turn<'a>(
        &'a self,
        input: GatewayAbortInput<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<(), JsString>> + Send + 'a>> {
        // `start_gateway_abort` is synchronous end to end (the connection
        // selects its consumer fence at the Router await boundary), so the
        // prefix completes before the future is even polled.
        let run_id = input.run_id.map(JsString::from);
        let reason = input.reason.map(JsString::from);
        let outcome = self
            .owner
            .start_gateway_abort(
                &JsString::from(input.session_key),
                run_id.as_ref(),
                reason.as_ref(),
            )
            .map(|_| ())
            .map_err(|error| JsString::from(error.message));
        Box::pin(async move { outcome })
    }

    fn submit_turn(&self, input: JsValue) -> Box<dyn AlwaysOnTurnStream> {
        Box::new(NativeAlwaysOnTurn(self.submitter.submit(input)))
    }

    /// `closeSession({ sessionKey, reason: "always-on/done" })`, awaited by
    /// every phase's `finally` with its rejection swallowed. The owner's
    /// close has no reason channel, so the reason stops at this boundary.
    fn close_session<'a>(
        &'a self,
        session_key: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let _ = self.owner.close(&JsString::from(session_key)).await;
        })
    }
}

/// The native always-on provider (`ProjectAutomationAlwaysOnProvider`): the
/// manager behind the lifecycle surface the bundle drives. In TS the
/// provider **is** the control (`ProjectAutomationAlwaysOnProvider extends
/// AlwaysOnControlPort`); the port splits the two, and
/// [`ProjectAutomationProvider::always_on_manager`] is where that identity
/// lands for the composition faces.
pub(super) struct NativeAlwaysOnProvider {
    manager: Arc<AlwaysOnManager>,
}

impl ProjectAutomationProvider for NativeAlwaysOnProvider {
    fn bind_agent_gateway(
        &self,
        gateway: &ProjectAutomationAgentGateway,
        hooks: Option<AlwaysOnBindHooks<'_>>,
    ) -> Result<(), std::io::Error> {
        // The always-on bind signature always carries the hooks record; the
        // busy member may be absent (ProjectAutomationBundle.ts:318-326).
        self.manager.bind_agent_gateway(
            gateway.always_on.clone(),
            hooks.and_then(|hooks| hooks.is_project_busy).cloned(),
        )
    }

    fn start(self: Arc<Self>) -> AutomationFuture {
        let manager = self.manager.clone();
        Box::pin(async move { manager.start().await.map_err(|error| error.to_string()) })
    }

    fn stop(self: Arc<Self>) -> AutomationFuture {
        let manager = self.manager.clone();
        Box::pin(async move {
            manager.stop().await;
            Ok(())
        })
    }

    /// TS composes the manager's tool definitions into `extraTools`
    /// (`getTools`, AlwaysOnManager.ts:137-139). The port's tool definitions
    /// are the fire pipeline's deep half, which stays behind the `fire.rs`
    /// seam, so the publish composes none — recorded in the -R1 ledger.
    fn tools(&self) -> Vec<JsString> {
        Vec::new()
    }

    fn always_on_manager(&self) -> Option<Arc<AlwaysOnManager>> {
        Some(self.manager.clone())
    }
}
