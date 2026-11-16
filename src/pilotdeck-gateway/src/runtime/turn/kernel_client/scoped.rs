//! Production-safe scoped Kernel session driver (`fb54c74a8`-compatible).
//! Shared by `ScopedResidentSessionBinder` and controlled P2 fixture tests.
use super::*;
use crate::native_process::PreparedManagedSession;
use crate::runtime::project::RuntimeError;
use std::collections::HashSet;

/// Tracks coordinated signal-driven shutdown when the Kernel Host runs in
/// the same OS process (`serve_with_resident_kernel`).
#[derive(Default)]
pub(crate) struct ResidentSignalState {
    gateway_stopping: AtomicBool,
    host_exit: Mutex<Option<Result<i32, String>>>,
    notify: Arc<tokio::sync::Notify>,
}

impl ResidentSignalState {
    #[cfg_attr(not(feature = "kernel-resident"), allow(dead_code))]
    pub(crate) fn mark_gateway_stopping(&self) {
        self.gateway_stopping.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub(crate) fn is_gateway_stopping(&self) -> bool {
        self.gateway_stopping.load(Ordering::SeqCst)
    }

    #[cfg_attr(not(feature = "kernel-resident"), allow(dead_code))]
    pub(crate) fn record_host_exit(&self, result: Result<i32, String>) {
        *self.host_exit.lock().unwrap_or_else(|p| p.into_inner()) = Some(result);
        self.notify.notify_waiters();
    }

    fn is_graceful_exit_zero(&self) -> bool {
        self.is_gateway_stopping()
            && matches!(
                *self.host_exit.lock().unwrap_or_else(|p| p.into_inner()),
                Some(Ok(0))
            )
    }

    async fn confirm_graceful_host_exit(&self, timeout: Duration) -> bool {
        if !self.is_gateway_stopping() {
            return false;
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(result) = self
                .host_exit
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
            {
                return self.is_gateway_stopping() && matches!(result, Ok(0));
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let _ = tokio::time::timeout(remaining, &mut notified).await;
        }
    }
}

#[derive(Clone)]
pub(crate) struct ScopedSessionConfig {
    pub loop_binding_id: String,
    pub controller_binding_id: String,
    pub expected_loop_binding: Option<p::Binding>,
    pub expected_controller_binding: Option<p::Binding>,
    pub module_id: JsString,
    pub timeout: Duration,
    pub consumer_demand: bool,
    pub resident_shutdown: Option<Arc<ResidentSignalState>>,
}

pub(crate) struct ScopedSessionBackend(Arc<ScopedInner>);

struct ScopedInner {
    wire: tokio::sync::Mutex<KernelConnection>,
    handle: agentcpm_host::TrustedProjectSession,
    owner: Arc<crate::native_process::ProjectModelOwner>,
    gate: tokio::sync::Mutex<()>,
    registrations: Mutex<Vec<(u64, Box<dyn KernelTurnRegistration>)>>,
    turn: AtomicU64,
    cursor: AtomicU64,
    lease_epoch: AtomicU64,
    close_notify: Arc<tokio::sync::Notify>,
    closing: AtomicBool,
    closed: AtomicBool,
    config: ScopedSessionConfig,
    runtime: tokio::runtime::Handle,
}

struct ScopedTurn {
    inner: Arc<ScopedInner>,
    facts: super::super::execution_backend::TurnExecutionFacts,
}

impl ScopedSessionBackend {
    pub(crate) fn new(
        wire: KernelConnection,
        handle: agentcpm_host::TrustedProjectSession,
        owner: Arc<crate::native_process::ProjectModelOwner>,
        config: ScopedSessionConfig,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self(Arc::new(ScopedInner {
            wire: tokio::sync::Mutex::new(wire),
            handle,
            owner,
            gate: tokio::sync::Mutex::new(()),
            registrations: Mutex::new(Vec::new()),
            turn: AtomicU64::new(0),
            cursor: AtomicU64::new(0),
            lease_epoch: AtomicU64::new(0),
            close_notify: Arc::new(tokio::sync::Notify::new()),
            closing: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            config,
            runtime,
        }))
    }

    pub(crate) fn into_prepared(self) -> PreparedManagedSession {
        let close = self.0.clone();
        PreparedManagedSession {
            backend: Arc::new(self),
            close: Box::new(move || {
                Box::pin(async move {
                    close
                        .close()
                        .await
                        .map_err(|e| RuntimeError::new(e.message))
                })
            }),
        }
    }
}

/// Bind an authenticated scoped project session inside an independent worker
/// task so dropping the caller's `prepare` future cannot leak an unclosed handle.
pub(crate) async fn prepare_scoped_session<F>(
    connect: F,
    projects: agentcpm_host::TrustedProjectSessions,
    owner: Arc<crate::native_process::ProjectModelOwner>,
    catalog: Vec<agentcpm_host::TrustedModelCatalogEntry>,
    config: ScopedSessionConfig,
) -> Result<PreparedManagedSession, RuntimeError>
where
    F: std::future::Future<Output = Result<KernelConnection, RuntimeError>> + Send + 'static,
{
    let worker = tokio::spawn(async move {
        let mut wire = connect.await?;
        let handle = wire
            .bind_project_session(&projects, owner.clone(), catalog)
            .await
            .map_err(|e| RuntimeError::new(e.message))?;
        Ok::<_, RuntimeError>(
            ScopedSessionBackend::new(
                wire,
                handle,
                owner,
                config,
                tokio::runtime::Handle::current(),
            )
            .into_prepared(),
        )
    });
    worker.await.map_err(|_| {
        RuntimeError::new("Scoped Kernel prepare worker failed; outcome unconfirmed.")
    })?
}

impl Drop for ScopedSessionBackend {
    fn drop(&mut self) {
        if self.0.closed.load(Ordering::SeqCst) {
            return;
        }
        let inner = self.0.clone();
        let runtime = inner.runtime.clone();
        runtime.spawn(async move {
            if inner.close().await.is_err() {
                // Unknown cleanup is not release. Keep the original issuer,
                // typed handle, registrations and connection alive indefinitely.
                std::mem::forget(inner);
            }
        });
    }
}

impl TurnExecutionBackend for ScopedSessionBackend {
    fn prepare(
        &self,
        facts: super::super::execution_backend::TurnExecutionFacts,
    ) -> HostFuture<Box<dyn TurnExecutionClient>> {
        let inner = self.0.clone();
        Box::pin(async move {
            if inner.closing.load(Ordering::SeqCst) {
                return Err(protocol("Scoped Kernel session is closing."));
            }
            inner.handle.validate_live().map_err(kernel_error)?;
            Ok(Box::new(ScopedTurn { inner, facts }) as Box<dyn TurnExecutionClient>)
        })
    }
}

impl ScopedInner {
    async fn call(&self, method: &str, value: Value) -> Result<Value, HostError> {
        self.wire.lock().await.call(method, value).await
    }

    fn matches_controller(&self, binding: &p::Binding) -> bool {
        self.config
            .expected_controller_binding
            .as_ref()
            .map_or_else(
                || binding.id == self.config.controller_binding_id,
                |expected| binding == expected,
            )
    }

    fn matches_loop(&self, binding: &p::Binding) -> bool {
        self.config.expected_loop_binding.as_ref().map_or_else(
            || binding.id == self.config.loop_binding_id,
            |expected| binding == expected,
        )
    }

    fn remove_registration(&self, turn: u64) {
        let removed = {
            let mut list = self.registrations.lock().unwrap_or_else(|p| p.into_inner());
            list.iter()
                .position(|(id, _)| *id == turn)
                .map(|index| list.swap_remove(index))
        };
        drop(removed);
    }

    async fn renew_lease(&self, epoch: u64) -> Result<(), HostError> {
        let renewal = self
            .call(
                "session.lease_renew",
                json!({
                    "session_id": self.handle.session_id(),
                    "epoch": epoch,
                    "ttl_ms": 300000,
                }),
            )
            .await?;
        if renewal["data"]["epoch"].as_u64() != Some(epoch) {
            return Err(unknown(
                "Scoped Kernel lease renewal changed the owning epoch.",
            ));
        }
        Ok(())
    }

    async fn ensure_lease(self: &Arc<Self>) -> Result<(), HostError> {
        let previous = self.lease_epoch.load(Ordering::SeqCst);
        if previous != 0 {
            return self.renew_lease(previous).await;
        }
        let lease = self
            .call(
                "session.lease_acquire",
                json!({"session_id": self.handle.session_id(), "ttl_ms": 300000}),
            )
            .await?;
        let epoch = lease["data"]["epoch"]
            .as_u64()
            .filter(|value| *value != 0)
            .ok_or_else(|| unknown("Scoped Kernel lease was not confirmed."))?;
        self.lease_epoch.store(epoch, Ordering::SeqCst);
        let notify = self.close_notify.clone();
        let weak = Arc::downgrade(self);
        self.runtime.spawn(async move {
            loop {
                let notified = notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                {
                    let Some(owner) = weak.upgrade() else {
                        break;
                    };
                    if owner.closing.load(Ordering::SeqCst) {
                        break;
                    }
                }
                tokio::select! {
                    () = &mut notified => break,
                    () = tokio::time::sleep(Duration::from_secs(60)) => {}
                }
                let Some(owner) = weak.upgrade() else {
                    break;
                };
                if owner.closing.load(Ordering::SeqCst) {
                    break;
                }
                if owner.renew_lease(epoch).await.is_err() {
                    owner.closing.store(true, Ordering::SeqCst);
                    owner.handle.revoke();
                    owner.close_notify.notify_waiters();
                    let _ = owner.close().await;
                    break;
                }
            }
        });
        Ok(())
    }

    async fn close(self: &Arc<Self>) -> Result<(), HostError> {
        self.closing.store(true, Ordering::SeqCst);
        self.close_notify.notify_waiters();
        let _gate = self.gate.lock().await;
        if self.closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.handle.revoke();
        let reply = match self
            .call(
                "session.close",
                json!({
                    "session_id": self.handle.session_id(),
                    "generation": self.handle.generation(),
                    "reason": "pilotdeck scoped session owner closed",
                }),
            )
            .await
        {
            Ok(reply) => reply,
            Err(error) => {
                let no_unsettled = self
                    .registrations
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .is_empty();
                if no_unsettled
                    && let Some(resident) = &self.config.resident_shutdown
                    && resident
                        .confirm_graceful_host_exit(self.config.timeout)
                        .await
                {
                    self.closed.store(true, Ordering::SeqCst);
                    return Ok(());
                }
                return Err(error);
            }
        };
        if reply["data"]["closed"] != true
            || reply["data"]["session_id"].as_str() != Some(self.handle.session_id())
        {
            return Err(unknown("Scoped Kernel session close was not confirmed."));
        }
        // Revocation and actual close have occurred; active scopes retain their
        // own source. Remove issuer records before waiting for owner release.
        let registrations =
            std::mem::take(&mut *self.registrations.lock().unwrap_or_else(|p| p.into_inner()));
        drop(registrations);
        tokio::time::timeout(self.config.timeout, async {
            while !self.handle.is_released() {
                if self
                    .config
                    .resident_shutdown
                    .as_ref()
                    .is_some_and(|resident| resident.is_graceful_exit_zero())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| unknown("Scoped Kernel project release unconfirmed."))?;
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

// If an owned worker panics or is destroyed by runtime shutdown, it cannot
// report a verified outcome. Retain the original owner synchronously before
// attempting asynchronous cleanup, even if the runtime no longer polls it.
struct WorkerExitGuard {
    inner: Arc<ScopedInner>,
    armed: bool,
}

impl WorkerExitGuard {
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for WorkerExitGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.inner.closing.store(true, Ordering::SeqCst);
        self.inner.close_notify.notify_waiters();
        std::mem::forget(self.inner.clone());
        let cleanup = self.inner.clone();
        let runtime = cleanup.runtime.clone();
        runtime.spawn(async move {
            let _ = cleanup.close().await;
        });
    }
}

impl TurnExecutionClient for ScopedTurn {
    fn module_id(&self) -> JsString {
        self.inner.config.module_id.clone()
    }

    fn execute(
        self: Box<Self>,
        request: ExecuteRequest,
        handler: Arc<dyn HostHandler>,
        cancel: Cancellation,
    ) -> HostFuture<ExecuteResult> {
        let mut scope = crate::host::TurnScope::new(handler.clone(), self.inner.config.timeout);
        let guard = WorkerExitGuard {
            inner: self.inner.clone(),
            armed: true,
        };
        let worker = tokio::spawn(async move {
            let retained = self.inner.clone();
            let (result, registered_turn) = self.run(request, handler, cancel).await;
            scope.dispose().await;
            let settled = match &result {
                Ok(_) => true,
                Err(error) => !error.accepted || error.kind == HostErrorKind::LoopFailure,
            };
            if settled {
                if let Some(turn) = registered_turn {
                    retained.remove_registration(turn);
                }
                guard.disarm();
                result
            } else {
                retained.closing.store(true, Ordering::SeqCst);
                retained.close_notify.notify_waiters();
                let cleanup = retained.clone();
                let runtime = cleanup.runtime.clone();
                // Transfer the still-armed guard into the cleanup task before
                // spawning. If that task never polls, Drop retains the owner.
                runtime.spawn(async move {
                    if cleanup.close().await.is_ok() {
                        guard.disarm();
                    }
                });
                result.map_err(|mut error| {
                    error.kind = HostErrorKind::ResultUnknown;
                    error.accepted = true;
                    error
                })
            }
        });
        Box::pin(async move {
            worker
                .await
                .map_err(|_| unknown("Scoped Kernel driver worker failed."))?
        })
    }
}

impl ScopedTurn {
    async fn run(
        self,
        request: ExecuteRequest,
        handler: Arc<dyn HostHandler>,
        cancel: Cancellation,
    ) -> (Result<ExecuteResult, HostError>, Option<u64>) {
        let inner = self.inner;
        let _gate = inner.gate.lock().await;
        if inner.closing.load(Ordering::SeqCst) {
            return (Err(protocol("Scoped Kernel session is closing.")), None);
        }
        if request.identity != self.facts.identity {
            return (
                Err(protocol("Scoped Kernel turn facts identity changed.")),
                None,
            );
        }
        if cancel.is_cancelled() {
            return (Ok(ExecuteResult::Aborted { terminal: None }), None);
        }
        if let Err(err) = inner.handle.validate_live().map_err(kernel_error) {
            return (Err(err), None);
        }
        let Some(turn) = next_turn(&inner.turn) else {
            return (
                Err(protocol("Scoped Kernel turn sequence exhausted.")),
                None,
            );
        };
        let key = uuid::Uuid::new_v4().to_string();
        let scope = KernelTurnScope {
            handle: &inner.handle,
            owner: &inner.owner,
            input_key: &key,
            turn,
        };
        let registration = match crate::native_process::ProjectTurnAdmission::new(
            inner.owner.clone(),
        )
        .admit(&request, &self.facts, &scope)
        {
            Ok(registration) => registration,
            Err(err) => return (Err(err), None),
        };
        inner
            .registrations
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push((turn, registration));
        let outcome = Self::run_admitted(&inner, request, handler, cancel, key, turn).await;
        (outcome, Some(turn))
    }

    async fn run_admitted(
        inner: &Arc<ScopedInner>,
        request: ExecuteRequest,
        handler: Arc<dyn HostHandler>,
        cancel: Cancellation,
        key: String,
        turn: u64,
    ) -> Result<ExecuteResult, HostError> {
        let sid = inner.handle.session_id();
        inner.ensure_lease().await?;
        if inner.closing.load(Ordering::SeqCst) {
            return Err(unknown(
                "Scoped Kernel session closing before input dispatch; issuer retained for close.",
            ));
        }
        if cancel.is_cancelled() {
            return Ok(ExecuteResult::Aborted { terminal: None });
        }
        let open = inner
            .call(
                "execution.open",
                json!({"session_id": sid, "generation": inner.handle.generation()}),
            )
            .await?;
        if open["protocol"] != p::PROTOCOL
            || open["input_controller_ref"] != inner.config.controller_binding_id
        {
            return Err(protocol("Scoped Kernel Scheduler binding mismatch."));
        }
        let input = original_request(&request)?;
        let expected_input = input.clone();
        let limits = execution_limits(&request)?;
        // Acquire the actual connection before the final local stop check;
        // no lock acquisition or other await intervenes before call polling.
        let mut wire = inner.wire.lock().await;
        if inner.closing.load(Ordering::SeqCst) {
            return Err(unknown(
                "Scoped Kernel stopped before input dispatch; issuer retained for close.",
            ));
        }
        if cancel.is_cancelled() {
            return Ok(ExecuteResult::Aborted { terminal: None });
        }
        let accepted = wire
            .call(
                "execution.input",
                json!({
                    "session_id": sid,
                    "generation": inner.handle.generation(),
                    "idempotency_key": key,
                    "turn": turn,
                    "input": input,
                    "limits": limits,
                    "consumer_demand": inner.config.consumer_demand,
                }),
            )
            .await
            .map_err(wire::input_failure)?;
        let entry = accepted["call_ref"]
            .as_str()
            .ok_or_else(|| unknown("Scoped Kernel input identity missing."))?
            .to_owned();
        let principal = wire.attested().principal().to_owned();
        drop(wire);

        let mut child: Option<String> = None;
        let mut origin = None;
        let mut parent_security = None;
        let mut parent_limits = None;
        let mut cursor = inner.cursor.load(Ordering::SeqCst);
        let mut sent_cancel = HashSet::<String>::new();
        let mut stop_deadline = None;

        loop {
            if cancel.is_cancelled() || inner.closing.load(Ordering::SeqCst) {
                let deadline =
                    *stop_deadline.get_or_insert_with(|| Instant::now() + inner.config.timeout);
                if Instant::now() >= deadline {
                    return Err(unknown(
                        "Scoped Kernel stopped worker remains unconfirmed; owner retained for close.",
                    ));
                }
                let target = child.as_deref().unwrap_or(entry.as_str());
                if sent_cancel.insert(target.to_owned()) {
                    inner
                        .call(
                            "execution.control",
                            json!({
                                "session_id": sid,
                                "call_ref": target,
                                "kind": "cancel",
                                "detail": "PilotDeck scoped turn stopping",
                            }),
                        )
                        .await?;
                }
            }
            if child.is_none() {
                let parent: p::CallView = serde_json::from_value(
                    inner
                        .call(
                            "execution.get",
                            json!({"session_id": sid, "call_ref": entry}),
                        )
                        .await?,
                )
                .map_err(|_| unknown("Scoped Kernel parent view invalid."))?;
                if parent.id != entry
                    || parent.session_ref != sid
                    || !inner.matches_controller(&parent.binding)
                    || parent.interaction.input_ref != entry
                    || parent.interaction.turn != turn
                    || parent.interaction.generation != inner.handle.generation()
                    || parent.interaction.principal != principal
                {
                    return Err(unknown("Scoped Kernel parent lineage changed."));
                }
                origin = Some(parent.interaction.clone());
                parent_security = Some(parent.security.clone());
                parent_limits = Some(parent.invocation.limits.clone());
                if parent.state == p::State::Completed {
                    let result = parent
                        .result
                        .ok_or_else(|| unknown("Scoped Kernel controller result missing."))?;
                    if result["profile"] != pilotdeck_iface::product::PROFILE {
                        return Err(unknown("Scoped Kernel controller profile mismatch."));
                    }
                    child = Some(
                        result["loop_ref"]
                            .as_str()
                            .ok_or_else(|| unknown("Scoped Kernel loop reference missing."))?
                            .to_owned(),
                    );
                } else if parent.state.confirmed_terminal() || parent.state == p::State::Unknown {
                    return Err(confirmed_failure(
                        parent.state,
                        parent.error,
                        "Scoped Kernel input controller ended without a Loop.",
                    ));
                }
            }
            if let Some(call) = &child {
                let view: p::CallView = serde_json::from_value(
                    inner
                        .call(
                            "execution.get",
                            json!({"session_id": sid, "call_ref": call}),
                        )
                        .await?,
                )
                .map_err(|_| unknown("Scoped Kernel loop view invalid."))?;
                if view.id != *call
                    || view.session_ref != sid
                    || !inner.matches_loop(&view.binding)
                    || view.source_binding.as_deref()
                        != Some(inner.config.controller_binding_id.as_str())
                    || view.cause_execution_ref.as_deref() != Some(entry.as_str())
                    || view.parent_session_ref.is_some()
                    || view.invocation.parent_execution_ref.is_some()
                    || view.invocation.payload.as_str() != Some(expected_input.as_str())
                    || Some(&view.security) != parent_security.as_ref()
                    || Some(&view.invocation.limits) != parent_limits.as_ref()
                    || Some(&view.interaction) != origin.as_ref()
                {
                    return Err(unknown("Scoped Kernel loop lineage changed."));
                }
                let events: Vec<p::Event> = serde_json::from_value(
                    inner
                        .call(
                            "execution.events",
                            json!({"session_id": sid, "after": cursor}),
                        )
                        .await?,
                )
                .map_err(|_| unknown("Scoped Kernel event page invalid."))?;
                for event in events {
                    if event.session_ref != sid || event.cursor <= cursor {
                        return Err(unknown("Scoped Kernel event cursor or session changed."));
                    }
                    cursor = event.cursor;
                    inner.cursor.store(cursor, Ordering::SeqCst);
                    let content = &event.payload["content"];
                    if content["profile"] != pilotdeck_iface::product::PROFILE {
                        continue;
                    }
                    let origin_ref = origin
                        .as_ref()
                        .ok_or_else(|| unknown("Scoped Kernel input origin missing."))?;
                    if inner.config.consumer_demand
                        && !publication_for_input(&event, sid, origin_ref)?
                    {
                        continue;
                    }
                    match content["kind"].as_str() {
                        Some("agent_event") => {
                            let publisher = event
                                .call_ref
                                .as_deref()
                                .ok_or_else(|| unknown("Scoped publication lacks source."))?;
                            if content["loop_ref"] != *call {
                                return Err(unknown("Scoped publication names a different Loop."));
                            }
                            let published_by: p::CallView = serde_json::from_value(
                                inner
                                    .call(
                                        "execution.get",
                                        json!({"session_id": sid, "call_ref": publisher}),
                                    )
                                    .await?,
                            )
                            .map_err(|_| unknown("Scoped publication source invalid."))?;
                            if published_by.id != publisher
                                || published_by.session_ref != sid
                                || !inner.matches_controller(&published_by.binding)
                                || Some(&published_by.interaction) != origin.as_ref()
                                || published_by.invocation.context_ref
                                    != pilotdeck_iface::product::EVENT_CONTEXT
                            {
                                return Err(unknown("Scoped publication source mismatch."));
                            }
                            let raw = content["event_json"]
                                .as_str()
                                .ok_or_else(|| unknown("Scoped event JSON missing."))?;
                            let parsed = pilotdeck_protocol::parse_json(raw)
                                .map_err(|_| unknown("Scoped event JSON invalid."))?;
                            let delivery =
                                deliver_product_event(&handler, &request.identity, parsed);
                            if inner.config.consumer_demand {
                                tokio::pin!(delivery);
                                if !sent_cancel.contains(call) {
                                    tokio::select! {
                                        result = &mut delivery => result?,
                                        () = cancel.cancelled() => {
                                            let deadline = *stop_deadline
                                                .get_or_insert_with(|| Instant::now() + inner.config.timeout);
                                            if sent_cancel.insert(call.clone()) {
                                                inner
                                                    .call(
                                                        "execution.control",
                                                        json!({
                                                            "session_id": sid,
                                                            "call_ref": call,
                                                            "kind": "cancel",
                                                            "detail": "PilotDeck turn cancelled during event delivery",
                                                        }),
                                                    )
                                                    .await?;
                                            }
                                            tokio::time::timeout_at(deadline, &mut delivery)
                                                .await
                                                .map_err(|_| unknown("Product consumer did not settle after cancellation."))??;
                                        }
                                    }
                                } else {
                                    let deadline = *stop_deadline.get_or_insert_with(|| {
                                        Instant::now() + inner.config.timeout
                                    });
                                    tokio::time::timeout_at(deadline, &mut delivery)
                                        .await
                                        .map_err(|_| unknown("Product consumer did not settle after cancellation."))??;
                                }
                                inner
                                    .call(
                                        "execution.output_ack",
                                        json!({"session_id": sid, "call_ref": entry, "cursor": event.cursor}),
                                    )
                                    .await?;
                            } else {
                                delivery.await?;
                            }
                        }
                        Some("turn_result") => {}
                        _ => return Err(unknown("Unknown scoped product publication kind.")),
                    }
                }
                if view.state.confirmed_terminal() || view.state == p::State::Unknown {
                    match view.state {
                        p::State::Completed => {
                            let raw = view
                                .result
                                .and_then(|value| value.as_str().map(str::to_owned))
                                .ok_or_else(|| {
                                    unknown("Scoped Kernel loop product result missing.")
                                })?;
                            let result = pilotdeck_protocol::parse_json(&raw)
                                .map_err(|_| unknown("Scoped Kernel product result invalid."))?;
                            verify_product_outcome(view.state, &result)?;
                            return product_terminal(&request.identity, result);
                        }
                        p::State::Cancelled => {
                            if let Some(raw) =
                                view.result.and_then(|v| v.as_str().map(str::to_owned))
                            {
                                let result =
                                    pilotdeck_protocol::parse_json(&raw).map_err(|_| {
                                        unknown("Scoped Kernel product result invalid.")
                                    })?;
                                verify_product_outcome(view.state, &result)?;
                                return product_terminal(&request.identity, result);
                            }
                            return Err(confirmed_failure(
                                view.state,
                                view.error,
                                "Scoped Kernel loop ended without an original product result.",
                            ));
                        }
                        _ => {
                            return Err(confirmed_failure(
                                view.state,
                                view.error,
                                "Scoped Kernel loop failed.",
                            ));
                        }
                    }
                }
                if view.state == p::State::NeedsHuman {
                    return Err(unknown(
                        "Kernel approval is pending; the product approval adapter is not installed.",
                    ));
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
