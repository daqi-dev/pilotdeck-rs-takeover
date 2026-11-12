//! A8 U8 (#1326): the cron runtime's assembly half. `build` turns the
//! normalized `cron` config section into the one `CronManager` the native
//! process owns; `NativeCronGatewayBody` is the production
//! `CronGatewayBody` behind `bind_agent_gateway` (the scheduler that drives
//! it is runtime/cron/scheduler.rs, #2295 — no `unimplemented!()` seam,
//! every method is real).
use std::path::Path;
use std::sync::Arc;

use pilotdeck_protocol::{JsObject, JsString, JsValue};

use crate::runtime::{
    cron::{
        CronError, CronGatewayBody, CronManager, CronManagerConfig, CronManagerOptions,
        CronTurnSubmit,
    },
    session::SessionOwner,
    turn::submit::{SubmitError, TurnSubmission, TurnSubmitter},
};

/// The `cron:` section of the normalized config snapshot
/// (config/snapshot.rs `cron()`, the port of `parseCronConfig`):
/// `Some` only when the section exists — the exact condition TS creates a
/// `CronManager` under (`ProjectAutomationBundle.createCron`). An existing
/// section is already normalized (defaults filled, invalid values fallen
/// back), so this reads the four fields rather than re-validating
/// (`sections.rs` did the diagnostics on load).
fn manager_config(section: &JsValue) -> CronManagerConfig {
    CronManagerConfig {
        enabled: section
            .get("enabled")
            .and_then(JsValue::as_bool)
            .unwrap_or(true),
        timezone: section
            .get("timezone")
            .and_then(JsValue::as_string)
            .and_then(|zone| zone.to_utf8().ok())
            .unwrap_or_else(|| "UTC".into()),
        max_concurrent_runs: section
            .get("maxConcurrentRuns")
            .and_then(JsValue::as_number)
            .unwrap_or(1.0),
        run_timeout_minutes: section
            .get("runTimeoutMinutes")
            .and_then(JsValue::as_number)
            .unwrap_or(60.0),
    }
}

/// The server's `cronLogger` (src/cli/pilotdeck.ts:70-75): `info` to stdout,
/// `warn` to stderr, `[cron] <message>` plus ` <JSON.stringify(data)>` when
/// data was passed. Every TS call site passes either no data or a non-empty
/// object literal, so an empty meta here means "no data".
struct ConsoleCronLogger;
impl ConsoleCronLogger {
    fn line(message: &str, meta: &JsObject) -> String {
        if meta.is_empty() {
            return format!("[cron] {message}");
        }
        match pilotdeck_protocol::stringify_json(&JsValue::Object(meta.clone())) {
            Ok(Some(data)) => format!("[cron] {message} {data}"),
            _ => format!("[cron] {message}"),
        }
    }
}
impl crate::runtime::cron::CronLogger for ConsoleCronLogger {
    fn info(&self, message: &str, meta: &JsObject) {
        println!("{}", Self::line(message, meta));
    }
    fn warn(&self, message: &str, meta: &JsObject) {
        eprintln!("{}", Self::line(message, meta));
    }
}

pub(super) fn cron_io_error(error: CronError) -> std::io::Error {
    std::io::Error::other(String::from_utf16_lossy(error.message().units()))
}

/// The native process's one production `CronManager::new` (test fixtures
/// construct their own): `None` keeps the unconfigured shape — the six names
/// stay out of capabilities and every method answers the `requireCron` gate.
pub(super) fn build(
    section: Option<&JsValue>,
    pilot_home: &Path,
    hooks: &NativeCronHooks,
) -> Result<Option<Arc<CronManager>>, std::io::Error> {
    let Some(section) = section else {
        return Ok(None);
    };
    let manager = CronManager::new(CronManagerOptions {
        config: manager_config(section),
        pilot_home: pilot_home.to_string_lossy().into_owned(),
        // `ProjectAutomationBundle.sessionOverrides` (:143,309): the one
        // registry the session factory reads when it composes a session.
        session_overrides: Some(hooks.session_overrides.clone()),
        now: None,
        uuid: None,
        logger: Some(Arc::new(ConsoleCronLogger)),
        scheduler_idle_poll_ms: None,
    })
    .map_err(cron_io_error)?;
    if let Some(handler) = &hooks.on_turn_event {
        manager.set_turn_event_handler(handler.clone());
    }
    if let Some(handler) = &hooks.on_result_delivery {
        manager.set_result_delivery_handler(handler.clone());
    }
    Ok(Some(manager))
}

/// What the process hands every cron manager it builds: the shared session
/// override registry and `onCronTurnEvent` (src/cli/pilotdeck.ts:85-87).
#[derive(Clone)]
pub(super) struct NativeCronHooks {
    pub session_overrides:
        Arc<std::sync::Mutex<crate::config::session_overrides::SessionConfigOverrides>>,
    pub on_turn_event: Option<crate::runtime::cron::CronTurnEventHandler>,
    pub on_result_delivery: Option<crate::runtime::cron::CronResultDeliveryHandler>,
}

/// `onCronResultDelivery` (src/cli/pilotdeck.ts:88-98): hand the record to
/// the channel server without awaiting it; `false` warns that no channel
/// handled it.
pub(super) fn deliver_to_channels(
    channels: Arc<super::super::channels::NativeChannels>,
) -> crate::runtime::cron::CronResultDeliveryHandler {
    // Cron runs call this from the scheduler's own thread (no ambient
    // runtime), so the delivery is spawned on the gateway runtime captured
    // at assembly.
    let runtime = tokio::runtime::Handle::try_current().ok();
    Arc::new(move |delivery: &JsValue| {
        let text = |key: &str| match delivery.get(key) {
            Some(JsValue::String(value)) => Some(value.clone()),
            _ => None,
        };
        let channel_key = |key: &str| text(key).map(crate::channel::protocol::ChannelKey::new);
        let task_id = text("taskId").unwrap_or_default();
        let run_id = text("runId").unwrap_or_default();
        let record = crate::channel::im_cron::CronResultDelivery {
            task_id: task_id.clone(),
            run_id: run_id.clone(),
            session_key: text("sessionKey").unwrap_or_default(),
            channel_key: channel_key("channelKey")
                .unwrap_or_else(|| crate::channel::protocol::ChannelKey::new("")),
            origin_session_key: text("originSessionKey"),
            origin_channel_key: channel_key("originChannelKey"),
            text: text("text").unwrap_or_default(),
        };
        let delivered = channels.server().deliver_cron_result(record);
        let task_id = String::from_utf16_lossy(task_id.units());
        let run_id = String::from_utf16_lossy(run_id.units());
        let report = async move {
            if !delivered.await {
                eprintln!("[cron] result delivery was not handled task={task_id} run={run_id}");
            }
        };
        match tokio::runtime::Handle::try_current()
            .ok()
            .or_else(|| runtime.clone())
        {
            Some(handle) => {
                handle.spawn(report);
            }
            None => {
                std::thread::spawn(move || {
                    if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        runtime.block_on(report);
                    }
                });
            }
        }
    })
}

/// `onCronTurnEvent` (src/cli/pilotdeck.ts:85-87): every cron turn event is
/// broadcast to the gateway's connections as `always-on:turn-event`.
pub(super) fn broadcast_turn_events(
    broadcast: Arc<crate::runtime::connection::ConnectionBroadcast>,
) -> crate::runtime::cron::CronTurnEventHandler {
    Arc::new(move |session, channel, event| {
        let mut payload = JsObject::new();
        payload.insert("sessionKey".into(), JsValue::String(session.clone()));
        payload.insert("channelKey".into(), JsValue::String(channel.clone()));
        payload.insert("event".into(), event.clone());
        broadcast.broadcast("always-on:turn-event", &JsValue::Object(payload));
    })
}

/// The native process's one cron provider for the automation bundle
/// (#2294): `build` is the construction half, this is the lifecycle half
/// the bundle drives (`ProjectAutomationCronProvider`). The bundle's
/// `createCron` section gate stays its decision (:304-306) — this factory
/// only ever sees a present section.
pub(super) fn native_factory(hooks: NativeCronHooks) -> super::automation::CronFactory {
    use super::automation::CronFactoryInput;
    Arc::new(move |input: CronFactoryInput| {
        // The bundle only calls this with a present section, so `build`'s
        // `Some` is the only reachable arm.
        let manager = build(Some(&input.section), &input.pilot_home, &hooks)?
            .expect("#2294 a present section constructs a manager");
        Ok(Arc::new(NativeCronProvider { manager })
            as Arc<dyn super::automation::ProjectAutomationProvider>)
    })
}

/// `ProjectAutomationCronProvider`: the one `CronManager` behind the bundle's
/// lifecycle surface.
pub(super) struct NativeCronProvider {
    manager: Arc<CronManager>,
}

impl super::automation::ProjectAutomationProvider for NativeCronProvider {
    fn bind_agent_gateway(
        &self,
        gateway: &super::automation::ProjectAutomationAgentGateway,
        _hooks: Option<super::automation::AlwaysOnBindHooks<'_>>,
    ) -> Result<(), std::io::Error> {
        // The cron bind signature takes no hooks record (the asymmetry
        // ProjectAutomationBundle.ts:318-326 pins).
        self.manager
            .bind_agent_gateway(gateway.cron.clone())
            .map_err(cron_io_error)
    }

    fn start(self: Arc<Self>) -> super::automation::AutomationFuture {
        let manager = self.manager.clone();
        Box::pin(async move {
            manager
                .start()
                .map_err(|error| String::from_utf16_lossy(error.message().units()))
        })
    }

    fn stop(self: Arc<Self>) -> super::automation::AutomationFuture {
        let manager = self.manager.clone();
        Box::pin(async move {
            manager
                .stop()
                .map_err(|error| String::from_utf16_lossy(error.message().units()))
        })
    }

    /// TS composes the cron provider's tool definitions into `extraTools`;
    /// the port has none behind its manager (the #1305 firing half owns that
    /// surface), so the publish composes none.
    fn tools(&self) -> Vec<JsString> {
        Vec::new()
    }

    fn cron_manager(&self) -> Option<Arc<CronManager>> {
        Some(self.manager.clone())
    }
}

/// `submitTurn`'s input exactly as CronFire.ts:80-88 builds it — the seven
/// fields the port's `CronTurnSubmit` carries; a task without a projectKey
/// omits the key, which reads as `undefined` to the submitter like TS.
fn submit_input(input: &CronTurnSubmit) -> JsValue {
    let mut out = JsObject::new();
    out.insert(
        "sessionKey".into(),
        JsValue::String(input.session_key.clone()),
    );
    out.insert(
        "channelKey".into(),
        JsValue::String(input.channel_key.clone()),
    );
    if let Some(project_key) = &input.project_key {
        out.insert("projectKey".into(), JsValue::String(project_key.clone()));
    }
    out.insert("message".into(), JsValue::String(input.message.clone()));
    out.insert("mode".into(), JsValue::String(input.mode.clone()));
    out.insert("runId".into(), JsValue::String(input.run_id.clone()));
    out.insert("timeoutMs".into(), JsValue::Number(input.timeout_ms));
    JsValue::Object(out)
}

/// The turn-execution seam of the live gateway. Threading contract: cron's
/// port methods are synchronous, while turn submission and session close are
/// futures the gateway's current-thread runtime owns. `run_turn` must drain
/// the whole turn (the port returns only after the turn unwound), so it may
/// only run on a non-runtime thread — the cron scheduler's own thread
/// (runtime/cron/scheduler.rs), or the fixtures' driver threads — and blocks
/// the captured `Handle`;
/// `close_session` mirrors TS's fire-and-forget call sites
/// (`.catch(() => undefined)`, never awaited) by scheduling the close and
/// returning; `abort_turn` is synchronous end to end
/// (`start_gateway_abort`, InProcessGateway.ts:2000-2013's synchronous
/// prefix).
pub(super) struct NativeCronGatewayBody {
    owner: SessionOwner,
    submitter: Arc<TurnSubmitter>,
    runtime: tokio::runtime::Handle,
}

impl NativeCronGatewayBody {
    pub(super) fn new(
        owner: SessionOwner,
        submitter: Arc<TurnSubmitter>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            owner,
            submitter,
            runtime,
        }
    }
}

/// The event stream a turn submission is, as the drain sees it: one step at
/// a time, `Err` reserved for transport failures (never for an event).
/// Factored out so the drain loop itself is testable against a scripted
/// stream — the rejection it guards is the loop's `return Err`, not just the
/// per-event classifier.
trait CronTurnStream {
    fn next_event(
        &mut self,
    ) -> impl std::future::Future<Output = Result<Option<JsValue>, SubmitError>>;
}

impl CronTurnStream for TurnSubmission {
    fn next_event(
        &mut self,
    ) -> impl std::future::Future<Output = Result<Option<JsValue>, SubmitError>> {
        self.next()
    }
}

/// Drain one admitted turn to its end, handing every event — terminal
/// `error` events included — to the `CronFire` half (fire.rs), which
/// classifies them exactly like CronFire.ts:96-141. A sink answering `false`
/// (its loop body threw) ends the drain; dropping the submission then
/// releases the turn. Only a broken stream is `Err`.
async fn drain_turn<S: CronTurnStream>(
    stream: &mut S,
    on_event: crate::runtime::cron::CronTurnEventSink<'_>,
) -> Result<(), CronError> {
    while let Some(event) = stream.next_event().await.map_err(|error| {
        CronError::gateway(
            "cron_run_failed",
            JsString::from(format!("Error: {}", error.message)),
        )
    })? {
        if !on_event(&event) {
            break;
        }
    }
    Ok(())
}

impl CronGatewayBody for NativeCronGatewayBody {
    fn run_turn(
        &self,
        input: &CronTurnSubmit,
        on_event: crate::runtime::cron::CronTurnEventSink<'_>,
    ) -> Result<(), CronError> {
        // Blocking a worker of the gateway's (current-thread) runtime would
        // deadlock the drain below, which only that runtime can advance.
        // An ambient handle here means a runtime thread called us — a
        // wiring bug that is answered, not deadlocked over.
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(CronError::error(JsString::from(
                "Cron run_turn cannot execute on the gateway runtime thread; the scheduler must call it from its own thread.",
            )));
        }
        let submitter = self.submitter.clone();
        self.runtime.block_on(async move {
            let mut submission = submitter.submit(submit_input(input));
            drain_turn(&mut submission, on_event).await
        })
    }

    fn abort_turn(
        &self,
        session_key: &JsString,
        run_id: Option<&JsString>,
        reason: Option<&JsString>,
    ) -> Result<(), CronError> {
        self.owner
            .start_gateway_abort(session_key, run_id, reason)
            .map(|_| ())
            .map_err(|error| CronError::error(error.message))
    }

    fn close_session(
        &self,
        session_key: &JsString,
        _reason: Option<&JsString>,
    ) -> Result<(), CronError> {
        // Every cron call site fires closeSession without awaiting it
        // (`CronRuntime.ts` `.catch(() => undefined)`); dropping the handle
        // detaches the task, which is that fire-and-forget shape. The owner's
        // close has no reason channel, so the reason stops at this boundary.
        let owner = self.owner.clone();
        let key = session_key.clone();
        std::mem::drop(self.runtime.spawn(async move {
            let _ = owner.close(&key).await;
        }));
        Ok(())
    }
}

#[cfg(test)]
mod cron_wiring_tests;
