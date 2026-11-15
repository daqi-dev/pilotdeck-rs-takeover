//! Test-only controlled P2/P3 Scheduler driver; never the production backend.
//! The product factory supplies the real lease and LiveSession supplies facts.
use super::*;
use crate::native_process::{
    ManagedFuture, ManagedSessionBinder, PreparedManagedSession, ProjectCatalogPolicy,
};
use crate::runtime::{
    project::{RuntimeError, RuntimeLease},
    session::SessionContext,
};
use std::sync::atomic::AtomicBool;

#[derive(Clone)]
pub(crate) struct FixtureBinder {
    pub socket: std::path::PathBuf,
    pub expected_principal: String,
    pub expected_tenant: String,
    pub timeout: Duration,
    pub required_features: Vec<String>,
    pub projects: agentcpm_host::TrustedProjectSessions,
    pub catalog: Arc<dyn ProjectCatalogPolicy>,
    pub loop_binding: p::Binding,
    pub controller_binding: p::Binding,
}
struct Inner {
    wire: tokio::sync::Mutex<KernelConnection>,
    handle: agentcpm_host::TrustedProjectSession,
    owner: Arc<crate::native_process::ProjectModelOwner>,
    gate: tokio::sync::Mutex<()>,
    registrations: Mutex<Vec<Box<dyn KernelTurnRegistration>>>,
    turn: AtomicU64,
    lease_epoch: AtomicU64,
    closing: AtomicBool,
    closed: AtomicBool,
    timeout: Duration,
    runtime: tokio::runtime::Handle,
    loop_binding: p::Binding,
    controller_binding: p::Binding,
}
struct Backend(Arc<Inner>);
struct Turn {
    inner: Arc<Inner>,
    facts: super::super::execution_backend::TurnExecutionFacts,
}
impl FixtureBinder {
    async fn connect(&self) -> Result<KernelConnection, RuntimeError> {
        let connection = KernelConnection::connect_tool_fixture(
            &self.socket,
            self.timeout,
            &self.required_features,
            &self.loop_binding.id,
            &self.controller_binding.id,
        )
        .await
        .map_err(|e| RuntimeError::new(e.message))?;
        if connection.attested().principal() != self.expected_principal
            || connection.attested().tenant() != Some(self.expected_tenant.as_str())
        {
            return Err(RuntimeError::new(
                "Fixture authenticated identity mismatch.",
            ));
        }
        Ok(connection)
    }
}
impl ManagedSessionBinder for FixtureBinder {
    fn validate_installation(&self) -> ManagedFuture<'_, Result<(), RuntimeError>> {
        Box::pin(async move { self.connect().await.map(drop) })
    }
    fn prepare<'a>(
        &'a self,
        admitted: &'a RuntimeLease,
        _: &'a SessionContext,
    ) -> ManagedFuture<'a, Result<PreparedManagedSession, RuntimeError>> {
        Box::pin(async move {
            // Build owner from the actual admitted lease before spawning. The
            // independent worker owns connection and typed grant if the caller
            // drops this prepare future during a Host RPC.
            let (owner, catalog) =
                crate::native_process::prepare_inputs(admitted, self.catalog.as_ref())?;
            let binder = self.clone();
            let worker = tokio::spawn(async move {
                let mut wire = binder.connect().await?;
                let handle = wire
                    .bind_tool_fixture(&binder.projects, owner.clone(), catalog)
                    .await
                    .map_err(|e| RuntimeError::new(e.message))?;
                let inner = Arc::new(Inner {
                    wire: tokio::sync::Mutex::new(wire),
                    handle,
                    owner,
                    gate: tokio::sync::Mutex::new(()),
                    registrations: Mutex::new(Vec::new()),
                    turn: AtomicU64::new(0),
                    lease_epoch: AtomicU64::new(0),
                    closing: AtomicBool::new(false),
                    closed: AtomicBool::new(false),
                    timeout: binder.timeout,
                    runtime: tokio::runtime::Handle::current(),
                    loop_binding: binder.loop_binding,
                    controller_binding: binder.controller_binding,
                });
                let close = inner.clone();
                Ok::<_, RuntimeError>(PreparedManagedSession {
                    backend: Arc::new(Backend(inner)),
                    close: Box::new(move || {
                        Box::pin(async move {
                            close
                                .close()
                                .await
                                .map_err(|e| RuntimeError::new(e.message))
                        })
                    }),
                })
            });
            worker.await.map_err(|_| {
                RuntimeError::new("Fixture prepare worker failed; outcome unconfirmed.")
            })?
        })
    }
}
impl Inner {
    async fn call(&self, method: &str, value: Value) -> Result<Value, HostError> {
        self.wire.lock().await.call(method, value).await
    }
    async fn close(self: &Arc<Self>) -> Result<(), HostError> {
        self.closing.store(true, Ordering::SeqCst);
        let _gate = self.gate.lock().await;
        if self.closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.handle.revoke();
        let reply = self.call("session.close", json!({"session_id":self.handle.session_id(),"generation":self.handle.generation(),"reason":"controlled P2 fixture cleanup"})).await?;
        if reply["data"]["closed"] != true
            || reply["data"]["session_id"].as_str() != Some(self.handle.session_id())
        {
            return Err(unknown("Fixture close was not confirmed."));
        }
        // Revocation and actual close have occurred; active scopes retain their
        // own source. Remove issuer records before waiting for owner release.
        let registrations =
            std::mem::take(&mut *self.registrations.lock().unwrap_or_else(|p| p.into_inner()));
        drop(registrations);
        tokio::time::timeout(self.timeout, async {
            while !self.handle.is_released() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| unknown("Fixture project release unconfirmed."))?;
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
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
impl TurnExecutionBackend for Backend {
    fn prepare(
        &self,
        facts: super::super::execution_backend::TurnExecutionFacts,
    ) -> HostFuture<Box<dyn TurnExecutionClient>> {
        let inner = self.0.clone();
        Box::pin(async move {
            if inner.closing.load(Ordering::SeqCst) {
                return Err(protocol("Fixture session is closing."));
            }
            inner.handle.validate_live().map_err(kernel_error)?;
            Ok(Box::new(Turn { inner, facts }) as Box<dyn TurnExecutionClient>)
        })
    }
}
// If an owned worker panics or is destroyed by runtime shutdown, it cannot
// report a verified outcome. Retain the original owner synchronously before
// attempting asynchronous cleanup, even if the runtime no longer polls it.
struct WorkerExitGuard {
    inner: Arc<Inner>,
    armed: bool,
}
impl Drop for WorkerExitGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.inner.closing.store(true, Ordering::SeqCst);
        std::mem::forget(self.inner.clone());
        let cleanup = self.inner.clone();
        let runtime = cleanup.runtime.clone();
        runtime.spawn(async move {
            let _ = cleanup.close().await;
        });
    }
}
impl TurnExecutionClient for Turn {
    fn module_id(&self) -> JsString {
        "p2-controlled-loop".into()
    }
    fn execute(
        self: Box<Self>,
        request: ExecuteRequest,
        handler: Arc<dyn HostHandler>,
        cancel: Cancellation,
    ) -> HostFuture<ExecuteResult> {
        // The worker retains the original handler, registration and owner if
        // the caller drops its future. Closing waits for the same worker gate.
        let guard = WorkerExitGuard {
            inner: self.inner.clone(),
            armed: true,
        };
        let worker = tokio::spawn(async move {
            let _handler = handler;
            let mut guard = guard;
            let retained = self.inner.clone();
            let result = self.run(request, cancel).await;
            if result.is_err() {
                retained.closing.store(true, Ordering::SeqCst);
                let cleanup = retained.clone();
                let runtime = cleanup.runtime.clone();
                // Transfer the still-armed guard into the cleanup task before
                // spawning. If that task never polls, Drop retains the owner.
                runtime.spawn(async move {
                    if cleanup.close().await.is_ok() {
                        guard.armed = false;
                    }
                });
            } else {
                guard.armed = false;
            }
            result.map_err(|mut error| {
                error.kind = HostErrorKind::ResultUnknown;
                error.accepted = true;
                error
            })
        });
        Box::pin(async move {
            worker
                .await
                .map_err(|_| unknown("Fixture driver worker failed."))?
        })
    }
}
impl Turn {
    async fn run(
        self,
        request: ExecuteRequest,
        cancel: Cancellation,
    ) -> Result<ExecuteResult, HostError> {
        let inner = self.inner;
        let _gate = inner.gate.lock().await;
        if inner.closing.load(Ordering::SeqCst) {
            return Err(protocol("Fixture session is closing."));
        }
        if request.identity != self.facts.identity {
            return Err(protocol("Fixture original facts changed."));
        }
        if cancel.is_cancelled() {
            return Ok(ExecuteResult::Aborted { terminal: None });
        }
        inner.handle.validate_live().map_err(kernel_error)?;
        let turn = next_turn(&inner.turn).ok_or_else(|| protocol("Fixture turn exhausted."))?;
        let key = uuid::Uuid::new_v4().to_string();
        let scope = KernelTurnScope {
            handle: &inner.handle,
            owner: &inner.owner,
            input_key: &key,
            turn,
        };
        let registration = crate::native_process::ProjectTurnAdmission::new(inner.owner.clone())
            .admit(&request, &self.facts, &scope)?;
        inner
            .registrations
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(registration);
        let sid = inner.handle.session_id();
        let previous = inner.lease_epoch.load(Ordering::SeqCst);
        let lease = if previous == 0 {
            inner
                .call(
                    "session.lease_acquire",
                    json!({"session_id":sid,"ttl_ms":300000}),
                )
                .await?
        } else {
            inner
                .call(
                    "session.lease_renew",
                    json!({"session_id":sid,"epoch":previous,"ttl_ms":300000}),
                )
                .await?
        };
        let epoch = lease["data"]["epoch"]
            .as_u64()
            .filter(|value| *value != 0)
            .ok_or_else(|| unknown("Fixture lease not confirmed."))?;
        if previous != 0 && epoch != previous {
            return Err(unknown("Fixture owning lease epoch changed."));
        }
        inner.lease_epoch.store(epoch, Ordering::SeqCst);
        if inner.closing.load(Ordering::SeqCst) || cancel.is_cancelled() {
            return Err(unknown(
                "Fixture stopped before input dispatch; issuer retained for close.",
            ));
        }
        let open = inner
            .call(
                "execution.open",
                json!({"session_id":sid,"generation":inner.handle.generation()}),
            )
            .await?;
        if open["protocol"] != p::PROTOCOL
            || open["input_controller_ref"] != inner.controller_binding.id
        {
            return Err(protocol("Fixture Scheduler binding mismatch."));
        }
        let input = original_request(&request)?;
        let expected_input = input.clone();
        let limits = execution_limits(&request)?;
        // Acquire the actual connection before the final local stop check;
        // no lock acquisition or other await intervenes before call polling.
        let mut wire = inner.wire.lock().await;
        if inner.closing.load(Ordering::SeqCst) || cancel.is_cancelled() {
            return Err(unknown(
                "Fixture stopped before input dispatch; issuer retained for close.",
            ));
        }
        let accepted=wire.call("execution.input",json!({"session_id":sid,"generation":inner.handle.generation(),"idempotency_key":key,"turn":turn,"input":input,"limits":limits,"consumer_demand":false})).await.map_err(wire::input_failure)?;
        let entry = accepted["call_ref"]
            .as_str()
            .ok_or_else(|| unknown("Fixture input identity missing."))?
            .to_owned();
        let principal = wire.attested().principal().to_owned();
        drop(wire);
        let mut child = None;
        let mut origin = None;
        let mut parent_security = None;
        let mut parent_limits = None;
        let mut sent_cancel = std::collections::HashSet::<String>::new();
        let mut stop_deadline = None;
        loop {
            if cancel.is_cancelled() || inner.closing.load(Ordering::SeqCst) {
                let deadline = *stop_deadline.get_or_insert_with(|| Instant::now() + inner.timeout);
                if Instant::now() >= deadline {
                    return Err(unknown(
                        "Fixture stopped worker remains unconfirmed; owner retained for close.",
                    ));
                }
                let target = child.as_deref().unwrap_or(&entry);
                if sent_cancel.insert(target.to_owned()) {
                    inner.call("execution.control",json!({"session_id":sid,"call_ref":target,"kind":"cancel","detail":"fixture owner stopping"})).await?;
                }
            }
            if child.is_none() {
                let parent: p::CallView = serde_json::from_value(
                    inner
                        .call("execution.get", json!({"session_id":sid,"call_ref":entry}))
                        .await?,
                )
                .map_err(|_| unknown("Fixture parent view invalid."))?;
                if parent.id != entry
                    || parent.session_ref != sid
                    || parent.binding != inner.controller_binding
                    || parent.interaction.input_ref != entry
                    || parent.interaction.turn != turn
                    || parent.interaction.generation != inner.handle.generation()
                    || parent.interaction.principal != principal
                {
                    return Err(unknown("Fixture parent lineage changed."));
                }
                origin = Some(parent.interaction.clone());
                parent_security = Some(parent.security.clone());
                parent_limits = Some(parent.invocation.limits.clone());
                if parent.state == p::State::Completed {
                    let result = parent
                        .result
                        .ok_or_else(|| unknown("Fixture controller result missing."))?;
                    if result["profile"] != pilotdeck_iface::product::PROFILE {
                        return Err(unknown("Fixture controller profile mismatch."));
                    }
                    child = Some(
                        result["loop_ref"]
                            .as_str()
                            .ok_or_else(|| unknown("Fixture loop reference missing."))?
                            .to_owned(),
                    );
                } else if parent.state.confirmed_terminal() || parent.state == p::State::Unknown {
                    return Err(confirmed_failure(
                        parent.state,
                        parent.error,
                        "Fixture controller failed.",
                    ));
                }
            }
            if let Some(call) = &child {
                let view: p::CallView = serde_json::from_value(
                    inner
                        .call("execution.get", json!({"session_id":sid,"call_ref":call}))
                        .await?,
                )
                .map_err(|_| unknown("Fixture loop view invalid."))?;
                if view.id != *call
                    || view.session_ref != sid
                    || view.binding != inner.loop_binding
                    || view.source_binding.as_deref() != Some(inner.controller_binding.id.as_str())
                    || view.cause_execution_ref.as_deref() != Some(entry.as_str())
                    || view.parent_session_ref.is_some()
                    || view.invocation.parent_execution_ref.is_some()
                    || view.invocation.payload.as_str() != Some(expected_input.as_str())
                    || Some(&view.security) != parent_security.as_ref()
                    || Some(&view.invocation.limits) != parent_limits.as_ref()
                    || Some(&view.interaction) != origin.as_ref()
                {
                    return Err(unknown("Fixture loop lineage changed."));
                }
                if view.state.confirmed_terminal() || view.state == p::State::Unknown {
                    if view.state != p::State::Completed {
                        return Err(confirmed_failure(
                            view.state,
                            view.error,
                            "Fixture loop failed.",
                        ));
                    }
                    let raw = view
                        .result
                        .and_then(|value| value.as_str().map(str::to_owned))
                        .ok_or_else(|| unknown("Fixture loop product result missing."))?;
                    let result = pilotdeck_protocol::parse_json(&raw)
                        .map_err(|_| unknown("Fixture product result invalid."))?;
                    verify_product_outcome(view.state, &result)?;
                    return product_terminal(&request.identity, result);
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
