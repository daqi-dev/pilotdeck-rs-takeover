//! 显式选择的 Kernel 执行客户端。这里只使用公开宿主交接能力与 L-C/SDK 合同。
//! 模型、工具、Context 在 Kernel 内执行，绝不把旧 ModuleRouter 注入 Native Loop。
#[cfg(test)]
pub(crate) mod p2_fixture;
pub(crate) mod scoped;
mod wire;
use super::execution_backend::{
    TurnExecutionBackend, TurnExecutionClient, deliver_product_event, product_terminal,
};
use crate::host::{
    Cancellation, ExecuteRequest, ExecuteResult, HostError, HostErrorKind, HostFuture, HostHandler,
};
use agentcpm_scheduler_protocol as p;
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Duration;
use tokio::time::Instant;
pub use wire::{AttestedConnection, KernelConnection};

const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Retain the original request and admitted host handles under the exact
/// authenticated owner. This registration grants no Kernel permission; actual
/// Native admission and resource profiles still reject unsupported work.
pub trait KernelRequestAdmission: Send + Sync {
    fn admit(
        &self,
        request: &ExecuteRequest,
        facts: &super::execution_backend::TurnExecutionFacts,
        scope: &KernelTurnScope<'_>,
    ) -> Result<Box<dyn KernelTurnRegistration>, HostError>;
}
/// Constructed only at the real Kernel backend's execution boundary. It binds
/// an input to the typed authenticated session and the very Arc passed to
/// bind_owner; request strings cannot construct this provenance.
pub struct KernelTurnScope<'a> {
    handle: &'a agentcpm_host::TrustedProjectSession,
    owner: &'a Arc<crate::native_process::project_model_owner::ProjectModelOwner>,
    input_key: &'a str,
    turn: u64,
}
impl KernelTurnScope<'_> {
    pub fn input_key(&self) -> &str {
        self.input_key
    }
    pub fn turn(&self) -> u64 {
        self.turn
    }
    pub fn generation(&self) -> u32 {
        self.handle.generation()
    }
    pub fn session_id(&self) -> &str {
        self.handle.session_id()
    }
    pub(crate) fn validate_owner(
        &self,
        owner: &Arc<crate::native_process::project_model_owner::ProjectModelOwner>,
    ) -> Result<(), HostError> {
        self.handle.validate_live().map_err(kernel_error)?;
        if !Arc::ptr_eq(self.owner, owner) {
            return Err(protocol(
                "Turn facts belong to a different authenticated project owner.",
            ));
        }
        Ok(())
    }
}

/// 同owner原始input key的有界登记。每job只借不可变turn事实，另建独立stream owner。
/// Drop撤销新job的借用；已接受job须自行保留事实到真实释放收据。
pub trait KernelTurnRegistration: Send + 'static {}

#[derive(Clone)]
pub struct KernelExecutionOptions {
    pub loop_binding: String,
    pub controller_binding: String,
    pub module_id: JsString,
    pub cleanup_timeout: Duration,
}
pub struct KernelSessionBackend {
    inner: Arc<SessionInner>,
}
struct SessionInner {
    connection: tokio::sync::Mutex<KernelConnection>,
    handle: agentcpm_host::TrustedProjectSession,
    product_owner: Arc<crate::native_process::project_model_owner::ProjectModelOwner>,
    options: KernelExecutionOptions,
    turn: AtomicU64,
    event_cursor: AtomicU64,
    lease_epoch: AtomicU64,
    close_notify: tokio::sync::Notify,
    active: AtomicBool,
    closing: AtomicBool,
    active_call: Mutex<Option<String>>,
    runtime: tokio::runtime::Handle,
}
fn protocol(message: impl Into<String>) -> HostError {
    HostError::new(HostErrorKind::Protocol, message)
}
fn unknown(message: impl Into<String>) -> HostError {
    let mut error = HostError::new(HostErrorKind::ResultUnknown, message);
    error.accepted = true;
    error
}
fn kernel_error(error: agentcpm_iface::types::Error) -> HostError {
    let mut host = HostError::new(HostErrorKind::Session, error.message.clone());
    // The public Error has no Display/Serialize implementation. Preserve all
    // fields as diagnostics; Debug describes only the non-exhaustive category,
    // never the user message or a machine permission/retry decision.
    host.diagnostic = json!({"iface_error": {
        "category_debug": format!("{:?}", error.category),
        "code": error.code,
        "message": error.message,
        "retryable": error.retryable,
    }})
    .to_string();
    host
}

/// Keep the workspace MSRV and the original SeqCst/checked-overflow semantics.
fn next_turn(counter: &AtomicU64) -> Option<u64> {
    let mut current = counter.load(Ordering::SeqCst);
    loop {
        let next = current.checked_add(1)?;
        match counter.compare_exchange_weak(current, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return Some(next),
            Err(observed) => current = observed,
        }
    }
}
impl Drop for KernelSessionBackend {
    fn drop(&mut self) {
        if !self.inner.handle.is_released() {
            let session = self.inner.clone();
            self.inner.runtime.spawn(async move {
                let _ = session.close().await;
            });
        }
    }
}
impl KernelSessionBackend {
    pub fn handle(&self) -> agentcpm_host::TrustedProjectSession {
        self.inner.handle.clone()
    }
    pub async fn close(&self) -> Result<(), HostError> {
        self.inner.close().await
    }
}
impl SessionInner {
    async fn rpc(&self, method: &str, params: Value) -> Result<Value, HostError> {
        self.connection.lock().await.call(method, params).await
    }
    async fn ensure_lease(self: &Arc<Self>) -> Result<(), HostError> {
        let previous = self.lease_epoch.load(Ordering::SeqCst);
        if previous != 0 {
            return self.renew_lease(previous).await;
        }
        let lease = self
            .rpc(
                "session.lease_acquire",
                json!({"session_id":self.handle.session_id(),"ttl_ms":300000}),
            )
            .await?;
        let epoch = lease["data"]["epoch"]
            .as_u64()
            .filter(|n| *n != 0)
            .ok_or_else(|| protocol("Kernel lease was not confirmed."))?;
        self.lease_epoch.store(epoch, Ordering::SeqCst);
        let owner = self.clone();
        // 同一真实连接持有会话推进权；空闲会话也续约，不在下一turn静默接管新epoch。
        self.runtime.spawn(async move {
            loop {
                if owner.closing.load(Ordering::SeqCst) {
                    break;
                }
                tokio::select! {
                    ()=owner.close_notify.notified()=>break,
                    ()=tokio::time::sleep(Duration::from_secs(60))=>{}
                }
                if owner.closing.load(Ordering::SeqCst) {
                    break;
                }
                if owner.renew_lease(epoch).await.is_err() {
                    owner.closing.store(true, Ordering::SeqCst);
                    owner.handle.revoke();
                    owner.close_notify.notify_one();
                    let _ = owner.close().await;
                    break;
                }
            }
        });
        Ok(())
    }
    async fn renew_lease(&self, epoch: u64) -> Result<(), HostError> {
        let renewal = self
            .rpc(
                "session.lease_renew",
                json!({"session_id":self.handle.session_id(),"epoch":epoch,"ttl_ms":300000}),
            )
            .await?;
        if renewal["data"]["epoch"].as_u64() != Some(epoch) {
            return Err(unknown("Kernel lease renewal changed the owning epoch."));
        }
        Ok(())
    }
    async fn close(&self) -> Result<(), HostError> {
        self.closing.store(true, Ordering::SeqCst);
        self.close_notify.notify_one();
        self.handle.revoke();
        let cleanup =
            async {
                let reply = self.rpc("session.close", json!({
                "session_id":self.handle.session_id(),"generation":self.handle.generation(),
                "reason":"pilotdeck session owner closed",
            })).await?;
                if reply["data"]["closed"] != true
                    || reply["data"]["session_id"].as_str() != Some(self.handle.session_id())
                {
                    return Err(unknown("Kernel did not confirm session close."));
                }
                while !self.handle.is_released() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Ok(())
            };
        tokio::time::timeout(self.options.cleanup_timeout, cleanup)
            .await
            .map_err(|_| unknown("Kernel session cleanup remains unconfirmed."))?
    }
}
struct BoundTurn {
    session: Arc<SessionInner>,
    facts: super::execution_backend::TurnExecutionFacts,
}
impl TurnExecutionBackend for KernelSessionBackend {
    fn prepare(
        &self,
        facts: super::execution_backend::TurnExecutionFacts,
    ) -> HostFuture<Box<dyn TurnExecutionClient>> {
        let session = self.inner.clone();
        Box::pin(async move {
            if session.closing.load(Ordering::SeqCst) {
                return Err(protocol("Kernel session is closing."));
            }
            session.handle.validate_live().map_err(kernel_error)?;
            Ok(Box::new(BoundTurn { session, facts }) as Box<dyn TurnExecutionClient>)
        })
    }
}
/// 已派发 future 被丢弃也保留真实清理所有者；释放信号仅由实际收据决定。
struct ActiveExecution {
    registration: Option<Box<dyn KernelTurnRegistration>>,
    session: Arc<SessionInner>,
    settled: bool,
    runtime: tokio::runtime::Handle,
}
impl Drop for ActiveExecution {
    fn drop(&mut self) {
        if self.settled {
            self.session.active.store(false, Ordering::SeqCst);
            *self
                .session
                .active_call
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = None;
        } else {
            let session = self.session.clone();
            let registration = self.registration.take();
            // 本客户端只在 LiveSession 的 Tokio 执行任务中构造/运行。
            // close 超时仍保留 handle/宿主 Unknown 事实，不标记释放。
            self.runtime.spawn(async move {
                // Registration pins the original owner root. Revoke new work
                // and retire that issuer before waiting for owner release;
                // otherwise close would wait on this guard itself. Existing
                // jobs independently retain their facts/fork/reservation until
                // actual receipts; retiring this guard proves no release.
                session.handle.revoke();
                drop(registration);
                let _ = session.close().await;
                session.active.store(false, Ordering::SeqCst);
            });
        }
    }
}
impl TurnExecutionClient for BoundTurn {
    fn module_id(&self) -> JsString {
        self.session.options.module_id.clone()
    }
    fn execute(
        self: Box<Self>,
        request: ExecuteRequest,
        handler: Arc<dyn HostHandler>,
        cancellation: Cancellation,
    ) -> HostFuture<ExecuteResult> {
        Box::pin(async move {
            // Own the original handler even if identity/admission/cancellation
            // rejects before any Kernel input has been accepted.
            let mut scope =
                crate::host::TurnScope::new(handler.clone(), self.session.options.cleanup_timeout);
            if request.identity != self.facts.identity {
                return Err(protocol("Kernel turn client identity changed."));
            }
            if cancellation.is_cancelled() {
                return Ok(ExecuteResult::Aborted { terminal: None });
            }
            self.session.handle.validate_live().map_err(kernel_error)?;
            if self.session.active.swap(true, Ordering::SeqCst) {
                return Err(protocol("Kernel session already has an active turn."));
            }
            let key = uuid::Uuid::new_v4().to_string();
            let turn = match next_turn(&self.session.turn) {
                Some(turn) => turn,
                None => {
                    self.session.active.store(false, Ordering::SeqCst);
                    return Err(protocol("Kernel turn sequence exhausted."));
                }
            };
            let input_scope = KernelTurnScope {
                handle: &self.session.handle,
                owner: &self.session.product_owner,
                input_key: &key,
                turn,
            };
            let registration =
                match crate::native_process::project_model_owner::ProjectTurnAdmission::new(
                    self.session.product_owner.clone(),
                )
                .admit(&request, &self.facts, &input_scope)
                {
                    Ok(registration) => registration,
                    Err(error) => {
                        self.session.active.store(false, Ordering::SeqCst);
                        return Err(error);
                    }
                };
            let mut owner = ActiveExecution {
                registration: Some(registration),
                session: self.session.clone(),
                settled: false,
                runtime: tokio::runtime::Handle::current(),
            };
            let result =
                execute_turn(&self.session, request, handler, cancellation, key, turn).await;
            scope.dispose().await;
            // 只有确认终态的产品输出才允许复用这条Kernel会话；未知结果关闭会话归属。
            if match &result {
                Ok(_) => true,
                Err(e) => !e.accepted || e.kind == HostErrorKind::LoopFailure,
            } {
                owner.settled = true;
            }
            result
        })
    }
}

fn original_request(request: &ExecuteRequest) -> Result<String, HostError> {
    let mut boundary = JsObject::new();
    // Same execute envelope as HostClient::execution::run. This message ID is
    // transport correlation only; original product IDs and Kernel authority
    // remain separate and are never derived from it.
    boundary.insert("kind".into(), "request".into());
    boundary.insert("messageId".into(), "execute".into());
    boundary.insert("method".into(), "execute".into());
    for (key, value) in [
        ("operationId", &request.identity.operation_id),
        ("runId", &request.identity.run_id),
        ("requestId", &request.identity.request_id),
        ("sessionId", &request.identity.session_id),
        ("turnId", &request.identity.turn_id),
    ] {
        boundary.insert(key.into(), JsValue::String(value.clone()));
    }
    boundary.insert("payload".into(), JsValue::Object(request.payload.clone()));
    if let Some(value) = &request.idempotency_key {
        boundary.insert("idempotencyKey".into(), JsValue::String(value.clone()));
    }
    if let Some(value) = &request.operation_deadline {
        boundary.insert("operationDeadline".into(), JsValue::String(value.clone()));
    }
    pilotdeck_protocol::stringify_json(&JsValue::Object(boundary))
        .map_err(|_| protocol("Product request cannot be encoded as original JSON text."))?
        .ok_or_else(|| protocol("Product request has no JSON representation."))
}

/// 复用原owner clock日期准入，不把maxTurns误译成Kernel调度步数。
fn execution_limits(request: &ExecuteRequest) -> Result<p::Limits, HostError> {
    use pilotdeck_transport::server::ExecutionClock;
    let mut limits = p::Limits::default();
    if let Some(value) = &request.operation_deadline
        && !value.is_empty()
    {
        let clock = pilotdeck_transport::system_clock::SystemExecutionClock;
        match clock.parse_date(value) {
            Ok(Some(ms)) if ms.is_finite() => {
                if ms >= u64::MAX as f64 || ms.fract() != 0.0 {
                    return Err(protocol(
                        "Operation deadline cannot be represented by the public Kernel UTC-millisecond contract.",
                    ));
                }
                // 负epoch同样已经过期；0经Kernel正式accept前期限门拒绝。
                limits.deadline_ms = Some(ms.max(0.0) as u64);
            }
            Ok(_) => {}
            Err(_) => return Err(protocol("UNSUPPORTED_DEADLINE_PROFILE")),
        }
    }
    if let Some(tools) = request.payload.get("tools") {
        let JsValue::Array(tools) = tools else {
            return Err(protocol("Product tool manifest is not an array."));
        };
        let mut names = std::collections::BTreeSet::new();
        for tool in tools {
            let name = tool
                .get("name")
                .and_then(JsValue::as_string)
                .ok_or_else(|| protocol("Product tool manifest lacks its admitted name."))?;
            names.insert(name.to_utf8().map_err(|_| {
                protocol("Tool authority name cannot be represented by the Kernel contract.")
            })?);
        }
        limits.allowed_tools = Some(names);
    }
    Ok(limits)
}

async fn execute_turn(
    session: &Arc<SessionInner>,
    request: ExecuteRequest,
    handler: Arc<dyn HostHandler>,
    cancel: Cancellation,
    key: String,
    turn: u64,
) -> Result<ExecuteResult, HostError> {
    let sid = session.handle.session_id();
    let payload = original_request(&request)?;
    let limits = execution_limits(&request)?;
    session.ensure_lease().await?;
    let opened = session
        .rpc(
            "execution.open",
            json!({"session_id":sid,"generation":session.handle.generation()}),
        )
        .await?;
    if opened["protocol"] != p::PROTOCOL
        || opened["input_controller_ref"] != session.options.controller_binding
    {
        return Err(protocol("Kernel installed a different input controller."));
    }
    session.handle.validate_live().map_err(kernel_error)?;
    if cancel.is_cancelled() {
        return Ok(ExecuteResult::Aborted { terminal: None });
    }
    let accepted = session.rpc("execution.input", json!({"session_id":sid,"generation":session.handle.generation(),"idempotency_key":key,"turn":turn,"input":payload,"limits":limits,"consumer_demand":true})).await
        .map_err(wire::input_failure)?;
    let entry = accepted["call_ref"]
        .as_str()
        .ok_or_else(|| unknown("Kernel accepted input without a call identity."))?
        .to_owned();
    *session
        .active_call
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = Some(entry.clone());
    let principal = session
        .connection
        .lock()
        .await
        .attested()
        .principal()
        .to_owned();
    let mut input_origin: Option<p::InputOrigin> = None;
    let mut child: Option<String> = None;
    let mut cursor = session.event_cursor.load(Ordering::SeqCst);
    let mut cancel_deadline = None;
    let mut control_sent = false;
    let mut output_deadline = None;
    loop {
        if session.closing.load(Ordering::SeqCst) {
            return Err(unknown(
                "Kernel session lost its authenticated owner or lease.",
            ));
        }
        if output_deadline.is_some_and(|end| Instant::now() >= end) {
            return Err(unknown(
                "Kernel Loop settled without a confirmed product output publication.",
            ));
        }
        if cancel.is_cancelled() && cancel_deadline.is_none() {
            cancel_deadline = Some(Instant::now() + session.options.cleanup_timeout);
        }
        if cancel_deadline.is_some_and(|end| Instant::now() >= end) {
            return Err(unknown(
                "Kernel cancellation did not reach a confirmed result.",
            ));
        }
        if child.is_none() {
            let parent: p::CallView = serde_json::from_value(
                session
                    .rpc("execution.get", json!({"session_id":sid,"call_ref":entry}))
                    .await?,
            )
            .map_err(|_| unknown("Kernel input controller returned an invalid call view."))?;
            if parent.id != entry
                || parent.session_ref != sid
                || parent.binding.id != session.options.controller_binding
                || parent.interaction.input_ref != entry
                || parent.interaction.turn != turn
                || parent.interaction.generation != session.handle.generation()
                || parent.interaction.principal != principal
            {
                return Err(unknown("Kernel input controller identity changed."));
            }
            input_origin = Some(parent.interaction.clone());
            if parent.state == p::State::Completed {
                let response = parent
                    .result
                    .ok_or_else(|| unknown("Kernel input controller has no result."))?;
                if response["profile"] != pilotdeck_iface::product::PROFILE {
                    return Err(unknown("Product controller profile mismatch."));
                }
                child = Some(
                    response["loop_ref"]
                        .as_str()
                        .ok_or_else(|| unknown("Product controller did not return its Loop."))?
                        .to_owned(),
                );
                *session
                    .active_call
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()) = child.clone();
            } else if matches!(
                parent.state,
                p::State::Failed | p::State::Cancelled | p::State::Unknown
            ) {
                return Err(confirmed_failure(
                    parent.state,
                    parent.error,
                    "Kernel input controller ended without a Loop.",
                ));
            }
        }
        if let Some(call) = &child {
            if cancel_deadline.is_some() && !control_sent {
                session.rpc("execution.control", json!({"session_id":sid,"call_ref":call,"kind":"cancel","detail":"PilotDeck turn cancelled"})).await?;
                control_sent = true;
            }
            let view: p::CallView = serde_json::from_value(
                session
                    .rpc("execution.get", json!({"session_id":sid,"call_ref":call}))
                    .await?,
            )
            .map_err(|_| unknown("Kernel Loop view is invalid."))?;
            if view.session_ref != sid
                || view.binding.id != session.options.loop_binding
                || Some(&view.interaction) != input_origin.as_ref()
            {
                return Err(unknown(
                    "Kernel Loop lineage does not match the admitted turn.",
                ));
            }
            if (view.state.confirmed_terminal() || view.state == p::State::Unknown)
                && output_deadline.is_none()
            {
                output_deadline = Some(Instant::now() + session.options.cleanup_timeout);
            }
            let events: Vec<p::Event> = serde_json::from_value(
                session
                    .rpc("execution.events", json!({"session_id":sid,"after":cursor}))
                    .await?,
            )
            .map_err(|_| unknown("Kernel execution event page is invalid."))?;
            for event in events {
                if event.session_ref != sid || event.cursor <= cursor {
                    return Err(unknown("Kernel execution event cursor or session changed."));
                }
                cursor = event.cursor;
                let content = &event.payload["content"];
                if content["profile"] != pilotdeck_iface::product::PROFILE {
                    continue;
                }
                let publisher = event
                    .call_ref
                    .as_deref()
                    .ok_or_else(|| unknown("Product publication has no authenticated source."))?;
                let origin = input_origin
                    .as_ref()
                    .ok_or_else(|| unknown("Kernel input origin was not confirmed."))?;
                if !publication_for_input(&event, sid, origin)? {
                    continue;
                }
                if content["loop_ref"] != *call {
                    return Err(unknown("Product publication names a different Loop."));
                }
                let published_by: p::CallView = serde_json::from_value(
                    session
                        .rpc(
                            "execution.get",
                            json!({"session_id":sid,"call_ref":publisher}),
                        )
                        .await?,
                )
                .map_err(|_| unknown("Product publication source is invalid."))?;
                if published_by.binding.id != session.options.controller_binding
                    || published_by.session_ref != sid
                    || published_by.id != publisher
                    || &published_by.interaction != origin
                    || !matches!(
                        published_by.invocation.context_ref.as_str(),
                        pilotdeck_iface::product::EVENT_CONTEXT
                            | pilotdeck_iface::product::RESULT_CONTEXT
                    )
                {
                    return Err(unknown(
                        "Product publication did not come from its installed controller.",
                    ));
                }
                match content["kind"].as_str() {
                    Some("agent_event") => {
                        let raw = content["event_json"]
                            .as_str()
                            .ok_or_else(|| unknown("Product event lost its original JSON text."))?;
                        let parsed = pilotdeck_protocol::parse_json(raw)
                            .map_err(|_| unknown("Product event JSON is invalid."))?;
                        let delivery = deliver_product_event(&handler, &request.identity, parsed);
                        tokio::pin!(delivery);
                        if !control_sent {
                            tokio::select! {
                                result=&mut delivery=>result?,
                                ()=cancel.cancelled()=>{
                                    let deadline=Instant::now()+session.options.cleanup_timeout;
                                    cancel_deadline=Some(deadline);
                                    session.rpc("execution.control",json!({"session_id":sid,"call_ref":call,"kind":"cancel","detail":"PilotDeck turn cancelled during event delivery"})).await?;
                                    control_sent=true;
                                    tokio::time::timeout_at(deadline,&mut delivery).await
                                        .map_err(|_|unknown("Product consumer did not settle after cancellation."))??;
                                }
                            }
                        } else {
                            tokio::time::timeout_at(
                                cancel_deadline.expect("control has cancellation deadline"),
                                &mut delivery,
                            )
                            .await
                            .map_err(|_| {
                                unknown("Product consumer did not settle after cancellation.")
                            })??;
                        }

                        session
                            .rpc(
                                "execution.output_ack",
                                json!({"session_id":sid,"call_ref":entry,"cursor":event.cursor}),
                            )
                            .await?;
                    }
                    Some("turn_result") => {
                        // publication可能比本轮开始时读取的view更新，核对最新权威结果。
                        let view: p::CallView = serde_json::from_value(
                            session
                                .rpc("execution.get", json!({"session_id":sid,"call_ref":call}))
                                .await?,
                        )
                        .map_err(|_| unknown("Kernel result view is invalid."))?;
                        if view.session_ref != sid
                            || view.binding.id != session.options.loop_binding
                            || &view.interaction != origin
                        {
                            return Err(unknown("Kernel result lineage changed."));
                        }
                        if event.payload["outcome"] != json!(view.state)
                            || content["result"] != json!(view.result)
                            || event.payload["error"] != json!(view.error)
                            || content.get("settlement_data") != view.settlement_data.as_ref()
                            || content.get("settlement_error").cloned()
                                != view.settlement_error.as_ref().map(|error| json!(error))
                        {
                            return Err(unknown(
                                "Published result contradicts the authoritative Loop.",
                            ));
                        }
                        let settlement = content
                            .get("settlement_data")
                            .map(pilotdeck_iface::settlement::decode)
                            .transpose()
                            .map_err(|reason| {
                                unknown(format!("Invalid product settlement: {reason}"))
                            })?;
                        if let Some(settlement) = &settlement {
                            // These are original accepted notifications, published by
                            // the Kernel's real settlement Controller. They grant no
                            // execution credit and never authorize model/tool work.
                            let until = cancel_deadline.unwrap_or_else(|| {
                                Instant::now() + session.options.cleanup_timeout
                            });
                            for raw in &settlement.events_json {
                                let parsed = pilotdeck_protocol::parse_json(raw).map_err(|_| {
                                    unknown("Product settlement event JSON is invalid.")
                                })?;
                                tokio::time::timeout_at(
                                    until,
                                    deliver_product_event(&handler, &request.identity, parsed),
                                )
                                .await
                                .map_err(|_| {
                                    unknown(
                                        "Product consumer did not settle its final notifications.",
                                    )
                                })??;
                            }
                        }
                        if let Some(error) = view.settlement_error {
                            let mut failure = unknown(
                                "Kernel could not snapshot the original product settlement.",
                            );
                            failure.diagnostic = error.wire();
                            return Err(failure);
                        }
                        if view.state == p::State::Unknown {
                            return Err(confirmed_failure(
                                view.state,
                                view.error,
                                "Kernel Loop requires reconciliation.",
                            ));
                        }
                        if !view.state.confirmed_terminal() {
                            continue;
                        }
                        let retained = settlement
                            .as_ref()
                            .and_then(|value| value.result_json.as_deref());
                        let ordinary = content["result"].as_str();
                        if ordinary.zip(retained).is_some_and(|(a, b)| a != b) {
                            return Err(unknown(
                                "Product settlement contradicts the ordinary original result.",
                            ));
                        }
                        let Some(result) = ordinary.or(retained) else {
                            return Err(confirmed_failure(
                                view.state,
                                view.error,
                                "Kernel ended without an original product result.",
                            ));
                        };
                        let result = pilotdeck_protocol::parse_json(result)
                            .map_err(|_| unknown("Kernel product result JSON is invalid."))?;
                        verify_product_outcome(view.state, &result)?;
                        session.event_cursor.store(cursor, Ordering::SeqCst);
                        return product_terminal(&request.identity, result);
                    }
                    _ => return Err(unknown("Unknown product publication kind.")),
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

/// Only the Kernel-authored envelope beside `content` binds a publication to
/// its authenticated input. Correlation IDs and product JSON are not authority.
fn publication_for_input(
    event: &p::Event,
    session: &str,
    origin: &p::InputOrigin,
) -> Result<bool, HostError> {
    let source = &event.payload["output_source"];
    let input = source["input_ref"]
        .as_str()
        .ok_or_else(|| unknown("Product publication lacks its authenticated input source."))?;
    if source["session_ref"].as_str() != Some(session)
        || source["call_ref"].as_str() != event.call_ref.as_deref()
    {
        return Err(unknown(
            "Product publication source identity contradicts its event envelope.",
        ));
    }
    if input != origin.input_ref {
        return Ok(false);
    }
    if source["turn"].as_u64() != Some(origin.turn)
        || source["generation"].as_u64() != Some(u64::from(origin.generation))
        || source["principal"].as_str() != Some(origin.principal.as_str())
    {
        return Err(unknown(
            "Product publication source contradicts the authenticated input origin.",
        ));
    }
    Ok(true)
}

fn verify_product_outcome(state: p::State, result: &JsValue) -> Result<(), HostError> {
    let kind = result
        .get("result")
        .and_then(|value| value.get("type"))
        .and_then(JsValue::as_string);
    let matches = kind.is_some_and(|kind| match state {
        p::State::Completed => kind.equals_str("success"),
        p::State::Failed => kind.equals_str("error") || kind.equals_str("max_turns"),
        p::State::Cancelled => kind.equals_str("aborted"),
        _ => false,
    });
    if matches {
        Ok(())
    } else {
        Err(unknown(
            "Original product result contradicts the authoritative Kernel outcome.",
        ))
    }
}

fn confirmed_failure(state: p::State, failure: Option<p::Failure>, fallback: &str) -> HostError {
    let mut error = HostError::new(
        if state == p::State::Unknown {
            HostErrorKind::ResultUnknown
        } else {
            HostErrorKind::LoopFailure
        },
        failure
            .as_ref()
            .map(p::render::english)
            .unwrap_or_else(|| fallback.to_owned()),
    );
    error.accepted = true;
    if let Some(failure) = failure {
        let mut module = crate::host::ModuleError::new(error.message.as_str());
        module.code = Some(failure.code.as_str().into());
        error.module_error = Some(Box::new(module));
        error.diagnostic = failure.wire();
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilotdeck_protocol::admission::ExecutionIdentity;

    #[test]
    fn iface_session_error_keeps_message_and_all_diagnostic_fields() {
        let source = agentcpm_iface::types::Error::app(
            agentcpm_iface::types::ErrorCategory::Policy,
            "SESSION_OWNER_REVOKED",
            "original detail",
        )
        .retryable(true);
        let error = kernel_error(source.clone());
        assert_eq!(error.kind, HostErrorKind::Session);
        assert_eq!(error.message, source.message);
        assert!(!error.accepted);
        assert!(error.module_error.is_none());
        let diagnostic: Value = serde_json::from_str(&error.diagnostic).unwrap();
        assert_eq!(
            diagnostic,
            json!({"iface_error": {
                "category_debug":"Policy", "code":source.code,
                "message":source.message, "retryable":true,
            }})
        );
    }

    #[test]
    fn turn_counter_is_unique_under_contention() {
        let counter = AtomicU64::new(0);
        let mut issued = std::thread::scope(|scope| {
            let jobs = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        (0..64)
                            .map(|_| next_turn(&counter).unwrap())
                            .collect::<Vec<_>>()
                    })
                })
                .collect::<Vec<_>>();
            jobs.into_iter()
                .flat_map(|job| job.join().unwrap())
                .collect::<Vec<_>>()
        });
        issued.sort_unstable();
        assert_eq!(issued, (1..=256).collect::<Vec<_>>());
        assert_eq!(counter.load(Ordering::SeqCst), 256);
    }

    #[test]
    fn exhausted_turn_counter_never_wraps_or_changes() {
        let counter = AtomicU64::new(u64::MAX - 1);
        assert_eq!(next_turn(&counter), Some(u64::MAX));
        assert_eq!(next_turn(&counter), None);
        assert_eq!(next_turn(&counter), None);
        assert_eq!(counter.load(Ordering::SeqCst), u64::MAX);
    }

    #[test]
    fn non_array_tool_manifest_is_rejected_without_a_default_grant() {
        let mut request = request();
        for value in [
            JsValue::Undefined,
            JsValue::Null,
            JsValue::Object(JsObject::new()),
        ] {
            request.payload.insert("tools".into(), value);
            let error = execution_limits(&request).unwrap_err();
            assert_eq!(error.kind, HostErrorKind::Protocol);
            assert_eq!(error.message, "Product tool manifest is not an array.");
            assert!(!error.accepted);
        }
    }

    fn request() -> ExecuteRequest {
        ExecuteRequest {
            identity: ExecutionIdentity {operation_id:"operation".into(),run_id:"run".into(),request_id:"request".into(),session_id:"session".into(),turn_id:"turn".into()},
            payload: pilotdeck_protocol::parse_json(r#"{"messages":[{"role":"user","content":[{"type":"text","text":"\ud800"},{"type":"image","source":{"url":"https://example.invalid/image"}}]}],"tools":[{"name":"read_file","readOnly":true}],"options":{"execution":{"maxTurns":9}}}"#).unwrap().as_object().unwrap().clone(),
            idempotency_key:Some("original-key".into()),operation_deadline:Some("2030-01-01T00:00:00.000Z".into()),
        }
    }
    #[test]
    fn full_original_request_preserves_rich_messages_and_utf16() {
        let request = request();
        let raw = original_request(&request).unwrap();
        let decoded = pilotdeck_protocol::parse_json(&raw).unwrap();
        let envelope = pilotdeck_protocol::wire::EnvelopeRef::decode(&decoded).unwrap();
        let pilotdeck_protocol::wire::MessageRef::Request(wire) = envelope.message else {
            panic!("expected original execute request");
        };
        assert!(wire.message_id.equals_str("execute"));
        let pilotdeck_protocol::wire::RequestBodyRef::Execute { identity, payload } = wire.body
        else {
            panic!("expected original execute method");
        };
        assert_eq!(identity.run_id, &request.identity.run_id);
        assert_eq!(identity.operation_id, &request.identity.operation_id);
        assert_eq!(identity.request_id, &request.identity.request_id);
        assert_eq!(payload, &request.payload);
        assert_eq!(
            decoded.get("payload"),
            Some(&JsValue::Object(request.payload))
        );
        assert_eq!(
            decoded.get("turnId"),
            Some(&JsValue::String(request.identity.turn_id))
        );
        assert_eq!(
            decoded.get("idempotencyKey"),
            Some(&JsValue::from("original-key"))
        );
    }
    #[test]
    fn original_envelope_missing_transport_fields_is_rejected() {
        let raw = original_request(&request()).unwrap();
        let decoded = pilotdeck_protocol::parse_json(&raw).unwrap();
        for field in ["kind", "method", "messageId"] {
            let mut incomplete = JsObject::new();
            for (name, value) in decoded.as_object().unwrap().entries() {
                if !name.equals_str(field) {
                    incomplete.insert(name.clone(), value.clone());
                }
            }
            assert!(
                pilotdeck_protocol::wire::EnvelopeRef::decode(&JsValue::Object(incomplete))
                    .is_err(),
                "missing {field} must not reach product preparation"
            );
        }
    }
    #[test]
    fn original_envelope_keeps_utf16_identity_distinct_from_kernel_scope() {
        let mut request = request();
        request.identity.run_id = JsString::from_utf16(vec![0xd800, b'r' as u16]);
        request.identity.operation_id = JsString::from_utf16(vec![b'o' as u16, 0xdc00]);
        request.identity.request_id = JsString::from_utf16(vec![0xd801, b'q' as u16]);
        request.idempotency_key = Some(JsString::from_utf16(vec![0xdc01, b'k' as u16]));
        let raw = original_request(&request).unwrap();
        let decoded = pilotdeck_protocol::parse_json(&raw).unwrap();
        let envelope = pilotdeck_protocol::wire::EnvelopeRef::decode(&decoded).unwrap();
        let pilotdeck_protocol::wire::MessageRef::Request(wire) = envelope.message else {
            panic!("expected original execute request");
        };
        let pilotdeck_protocol::wire::RequestBodyRef::Execute { identity, .. } = wire.body else {
            panic!("expected original execute method");
        };
        assert_eq!(identity.run_id, &request.identity.run_id);
        assert_eq!(identity.operation_id, &request.identity.operation_id);
        assert_eq!(identity.request_id, &request.identity.request_id);
        assert_eq!(
            decoded.get("idempotencyKey").and_then(JsValue::as_string),
            request.idempotency_key.as_ref()
        );
        assert!(wire.message_id.equals_str("execute"));
    }
    #[test]
    fn only_actual_manifest_and_absolute_deadline_enter_scheduler_limits() {
        let limits = execution_limits(&request()).unwrap();
        assert_eq!(limits.deadline_ms, Some(1893456000000));
        assert_eq!(limits.allowed_tools, Some(["read_file".into()].into()));
        assert_eq!(
            limits.max_steps, None,
            "PD maxTurns is not scheduler step count"
        );
        assert!(
            !limits.read_only,
            "individual tool readOnly is not a whole-turn policy"
        );
        let mut request = request();
        request
            .payload
            .insert("tools".into(), JsValue::Array(vec![]));
        assert_eq!(
            execution_limits(&request).unwrap().allowed_tools,
            Some(Default::default())
        );
    }
    #[test]
    fn confirmed_failure_preserves_structured_reason_without_becoming_unknown() {
        let failure = p::Failure::external("TOOL_DENIED", "permission denied");
        let failed = confirmed_failure(p::State::Failed, Some(failure.clone()), "fallback");
        assert_eq!(failed.kind, HostErrorKind::LoopFailure);
        assert!(failed.accepted);
        assert_eq!(failed.diagnostic, failure.wire());
        assert_eq!(
            failed.module_error.unwrap().code,
            Some("TOOL_DENIED".into())
        );
        assert_eq!(
            confirmed_failure(p::State::Unknown, Some(failure), "uncertain").kind,
            HostErrorKind::ResultUnknown
        );
    }
    fn publication(origin: &p::InputOrigin) -> p::Event {
        p::Event {
            id: "event".into(),
            session_ref: "session".into(),
            call_ref: Some("publisher".into()),
            correlation_id: "old-input-display-key".into(),
            kind: "output".into(),
            state: Some(p::State::Running),
            cursor: 3,
            payload: json!({"content":{"profile":pilotdeck_iface::product::PROFILE},
                "output_source":{"session_ref":"session","call_ref":"publisher",
                    "input_ref":origin.input_ref,"principal":origin.principal,
                    "turn":origin.turn,"generation":origin.generation}}),
        }
    }
    fn input_origin() -> p::InputOrigin {
        p::InputOrigin {
            input_ref: "new-input-call".into(),
            principal: "authenticated-deployment".into(),
            turn: 8,
            generation: 3,
        }
    }
    #[test]
    fn resumed_output_uses_authenticated_input_source_not_old_correlation() {
        let origin = input_origin();
        let mut event = publication(&origin);
        assert!(publication_for_input(&event, "session", &origin).unwrap());
        event.correlation_id = "arbitrary-display-key".into();
        assert!(publication_for_input(&event, "session", &origin).unwrap());
        event.payload["output_source"]["input_ref"] = json!("other-input");
        assert!(!publication_for_input(&event, "session", &origin).unwrap());
    }
    #[test]
    fn event_source_mismatch_and_product_body_claim_do_not_authorize_delivery() {
        let origin = input_origin();
        for (field, value) in [
            ("session_ref", json!("other")),
            ("call_ref", json!("other")),
            ("principal", json!("other")),
            ("turn", json!(7)),
            ("generation", json!(4)),
        ] {
            let mut event = publication(&origin);
            event.payload["output_source"][field] = value;
            assert!(
                publication_for_input(&event, "session", &origin).is_err(),
                "{field}"
            );
        }
        let mut event = publication(&origin);
        event.payload["content"]["output_source"] = event.payload["output_source"].clone();
        event
            .payload
            .as_object_mut()
            .unwrap()
            .remove("output_source");
        event.correlation_id = origin.input_ref.clone();
        assert!(publication_for_input(&event, "session", &origin).is_err());
    }
    #[test]
    fn product_result_cannot_upgrade_kernel_unknown_cancelled_or_failed() {
        for (state, kind) in [
            (p::State::Completed, "success"),
            (p::State::Failed, "error"),
            (p::State::Failed, "max_turns"),
            (p::State::Cancelled, "aborted"),
        ] {
            let value =
                pilotdeck_protocol::parse_json(&format!(r#"{{"result":{{"type":"{kind}"}}}}"#))
                    .unwrap();
            assert!(verify_product_outcome(state, &value).is_ok());
            assert!(verify_product_outcome(p::State::Unknown, &value).is_err());
        }
        let success = pilotdeck_protocol::parse_json(r#"{"result":{"type":"success"}}"#).unwrap();
        assert!(verify_product_outcome(p::State::Failed, &success).is_err());
        assert!(verify_product_outcome(p::State::Cancelled, &success).is_err());
    }
}
