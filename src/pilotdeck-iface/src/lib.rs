//! PilotDeck's admitted AgentLoop exposed through the public ifACE Scheduler SDK.
//!
//! This crate does not instantiate a Gateway or own a second execution runtime.
//! The composition root supplies pure admission/preparation and resource codecs;
//! every model, tool and context host effect uses the invocation-bound Kernel
//! client. Capability declarations remain conservative until their full product
//! adapters are installed and validated.
#![forbid(unsafe_code)]

pub mod context_capability;
pub mod context_codec;
pub mod context_history_owner;
pub mod initial_read_codec;
pub mod model_session_codec;
mod model_session_events;
pub mod native_loop_launch;
pub mod original;
pub mod preparation_effects;
pub mod product;
pub mod product_event_queue;
pub mod production_bindings;
pub mod production_clock;
pub mod production_entry;
pub mod production_events;
pub mod recovery_binding;
#[cfg(feature = "host-composition")]
pub mod recovery_composition;
pub mod recovery_launch;
pub mod resource_mux;
pub mod resources;
pub mod scoped_native_entry;
pub mod sdk_notice_owner;
pub mod settlement;
pub mod tool_batch_codec;
pub mod tool_batch_owner;
pub mod transcript_snapshot;

use agentcpm_iface::{
    BoxFuture,
    r#loop::{LoopContract, LoopDescriptor, StepOutcome},
    types::*,
};
use agentcpm_scheduler_protocol::{self as scheduler, Client, LoopFactory};
use pilotdeck_agent::{
    ProjectedMessage,
    ports::{CallControl, CancelCause, Cancellation, HostPort, PortError, PortResult, ToolHints},
    turn::{
        self, TurnClock, TurnEventPort, TurnPreparationPort, TurnResultKind, TurnRuntimeConfig,
    },
};
use resources::{KernelHostPort, ResourceCodec};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::watch;

/// Admitted, immutable product values and turn-local pure collaborators.
/// There is deliberately no `HostPort` field: the Native adapter always creates
/// its own Scheduler-bound resource port, preventing an old Gateway bypass.
pub struct AdmittedTurn {
    pub config: TurnRuntimeConfig,
    pub input: turn::TurnInput,
    pub initial: Vec<ProjectedMessage>,
    pub preparation: Box<pilotdeck_turn_preparation::OriginalTurnPreparation>,
    pub file_state: pilotdeck_agent::session_state::AgentFileState,
    pub events: Box<dyn TurnEventPort + Send>,
    pub clock: Box<dyn TurnClock + Send>,
    pub tool_hints: ToolHints,
}

/// Product admission retains its rich boundary data in `ctx.invocation.payload`.
/// The minimal ifACE text messages must not silently replace those boundaries.
/// Initialization uses the already-bound Kernel port. Implementations may not
/// install a second history/permission/execution owner or supply another host.
/// Owned for a single execution, shared by its codec and preparation path.
/// Dropping it fences no external resource and does not imply settlement.
pub trait CallAssembly: Send {
    fn resource_codec(&mut self) -> PortResult<Box<dyn ResourceCodec>>;
    fn prepare(
        &mut self,
        host: &mut dyn HostPort,
        control: &CallControl,
    ) -> PortResult<AdmittedTurn>;
}
pub trait AdmittedTurnFactory: Send + Sync {
    /// Called once inside the owned worker before creating its host. None keeps
    /// the existing legacy factory path; Some owns the entire call assembly.
    /// Binding and codec construction must not dispatch effects or consume
    /// history; the worker has not yet validated the product Execute payload.
    fn bind_call(
        &self,
        _ctx: &scheduler::CallContext,
        _input: &TurnInput,
        _client: Arc<dyn Client>,
        _control: &CallControl,
    ) -> PortResult<Option<Box<dyn CallAssembly>>> {
        Ok(None)
    }
    fn resource_codec(
        &self,
        ctx: &scheduler::CallContext,
        input: &TurnInput,
    ) -> PortResult<Box<dyn ResourceCodec>>;
    fn prepare(
        &self,
        ctx: &scheduler::CallContext,
        input: &TurnInput,
        host: &mut dyn HostPort,
        client: Arc<dyn Client>,
        control: &CallControl,
    ) -> PortResult<AdmittedTurn>;
}

pub struct NativeFactory {
    admission: Arc<dyn AdmittedTurnFactory>,
    retain_settlement: bool,
    scoped_steps: bool,
    calls: Mutex<BTreeMap<(String, String), Weak<Shared>>>,
}

impl NativeFactory {
    pub fn new(admission: Arc<dyn AdmittedTurnFactory>) -> Self {
        Self {
            admission,
            retain_settlement: false,
            scoped_steps: false,
            calls: Mutex::new(BTreeMap::new()),
        }
    }

    /// Explicitly opt into the public settlement-data channel. The product
    /// composition must first verify the actual Host's negotiated support.
    /// The default retains the existing direct event path for older hosts.
    pub fn with_settlement_data(mut self) -> Self {
        self.retain_settlement = true;
        self
    }
    /// Explicit opt-in only with an actual per-connection step grant.
    pub fn with_scoped_steps(mut self) -> Self {
        self.scoped_steps = true;
        self
    }
}

/// Run the official bidirectional Native provider protocol on stdin/stdout.
/// A product binary supplies the admitted-turn factory; it must not put logs on
/// stdout or replace the SDK with a private Kernel transport.
pub async fn serve(admission: Arc<dyn AdmittedTurnFactory>) -> scheduler::Result<()> {
    scheduler::provider::serve(scheduler::provider::Export::Loop(Arc::new(
        NativeFactory::new(admission),
    )))
    .await
}

/// The caller must have verified the actual installed Host's settlement
/// capability before admitting product effects. This is not a fallback.
pub async fn serve_with_settlement_data(
    admission: Arc<dyn AdmittedTurnFactory>,
) -> scheduler::Result<()> {
    scheduler::provider::serve_with_required_host_features(
        scheduler::provider::Export::Loop(Arc::new(
            NativeFactory::new(admission).with_settlement_data(),
        )),
        &[
            scheduler::SETTLEMENT_DATA_FEATURE,
            scheduler::LOOP_SETTLEMENT_FEATURE,
        ],
    )
    .await
}

/// Full settlement and per-step grant are independently negotiated.
pub async fn serve_scoped_with_settlement_data(
    admission: Arc<dyn AdmittedTurnFactory>,
) -> scheduler::Result<()> {
    scheduler::provider::serve_with_required_host_features(
        scheduler::provider::Export::Loop(Arc::new(
            NativeFactory::new(admission)
                .with_settlement_data()
                .with_scoped_steps(),
        )),
        &[
            scheduler::SETTLEMENT_DATA_FEATURE,
            scheduler::LOOP_SETTLEMENT_FEATURE,
            scheduler::step_scope::FEATURE,
        ],
    )
    .await
}

#[derive(Clone)]
struct Completion {
    outcome: StepOutcome,
    status: RequestState,
    result: Option<String>,
    cause: Option<scheduler::Failure>,
    settlement_data: Option<Value>,
}

struct Shared {
    runtime_identity: std::sync::OnceLock<Weak<dyn LoopContract>>,
    cause_context: scheduler::CallContext,
    input: Mutex<Option<(TurnId, TurnInput, Option<scheduler::step_scope::StepRef>)>>,
    admission: Arc<dyn AdmittedTurnFactory>,
    retain_settlement: bool,
    cancellation: Cancellation,
    completion: watch::Sender<Option<Completion>>,
}

struct NativeLoop {
    context: scheduler::CallContext,
    client: Arc<dyn Client>,
    shared: Arc<Shared>,
}

impl Drop for NativeLoop {
    fn drop(&mut self) {
        // Drop cannot claim that accepted work was undone. The hard cancellation
        // wakes cooperating ports; the Kernel retains durable reconciliation.
        self.shared.cancellation.cancel(CancelCause::OwnerRetired);
    }
}

fn protocol(message: &str) -> Error {
    Error::new(ErrorCategory::Protocol, -32602, message)
}

impl LoopFactory for NativeFactory {
    fn create(
        &self,
        ctx: scheduler::CallContext,
        client: Arc<dyn Client>,
    ) -> BoxFuture<'_, scheduler::Result<Arc<dyn LoopContract>>> {
        Box::pin(async move {
            if ctx.call_ref.is_empty()
                || ctx.session_ref.is_empty()
                || ctx.invocation.context_ref.is_empty()
            {
                return Err(scheduler::Failure::external(
                    "CONTEXT_BINDING",
                    "explicit call/session/context identity required",
                ));
            }
            let mut calls = self
                .calls
                .lock()
                .map_err(|_| scheduler::Failure::external("LOOP_LOCK", "call registry poisoned"))?;
            calls.retain(|_, weak| weak.strong_count() > 0);
            let call_key = (ctx.session_ref.clone(), ctx.call_ref.clone());
            if calls.contains_key(&call_key) {
                return Err(scheduler::Failure::external(
                    "DUPLICATE_CALL",
                    "call already has a Native loop",
                ));
            }
            let (completion, _) = watch::channel(None);
            let shared = Arc::new(Shared {
                runtime_identity: std::sync::OnceLock::new(),
                cause_context: ctx.clone(),
                input: Mutex::new(None),
                admission: self.admission.clone(),
                retain_settlement: self.retain_settlement,
                cancellation: Cancellation::default(),
                completion,
            });
            calls.insert(call_key, Arc::downgrade(&shared));
            let instance: Arc<dyn LoopContract> = Arc::new(NativeLoop {
                context: ctx,
                client,
                shared: shared.clone(),
            });
            let _ = shared.runtime_identity.set(Arc::downgrade(&instance));
            Ok(instance)
        })
    }

    fn supports_scoped_steps(&self) -> bool {
        self.scoped_steps
    }
    fn supports_scoped_runtime(
        &self,
        ctx: &scheduler::CallContext,
        runtime: &dyn LoopContract,
    ) -> bool {
        if !self.scoped_steps {
            return false;
        }
        let Ok(calls) = self.calls.lock() else {
            return false;
        };
        let Some(shared) = calls
            .get(&(ctx.session_ref.clone(), ctx.call_ref.clone()))
            .and_then(Weak::upgrade)
        else {
            return false;
        };
        let Some(original) = shared.runtime_identity.get().and_then(Weak::upgrade) else {
            return false;
        };
        std::ptr::eq(original.as_ref(), runtime)
            && scheduler::step_scope::same_execution(&shared.cause_context, ctx)
    }
    fn step_scoped<'a>(
        &'a self,
        step: &'a scheduler::CallContext,
        runtime: &'a dyn LoopContract,
        turn: u64,
        input: TurnInput,
        client: Arc<dyn Client>,
    ) -> BoxFuture<'a, Result<StepOutcome, Error>> {
        Box::pin(async move {
            if !self.supports_scoped_runtime(step, runtime) {
                return Err(protocol("scoped runtime identity mismatch"));
            }
            let grant = client
                .step_scope()
                .ok_or_else(|| protocol("scoped step grant is missing"))?;
            if !grant.matches(step, turn, &input.execution.operation_id)
                || input.execution.run_id != step.call_ref
            {
                return Err(protocol("scoped step identity mismatch"));
            }
            let shared = self
                .calls
                .lock()
                .map_err(|_| protocol("call registry poisoned"))?
                .get(&(step.session_ref.clone(), step.call_ref.clone()))
                .and_then(Weak::upgrade)
                .ok_or_else(|| protocol("scoped owner is unavailable"))?;
            shared
                .drive(step.clone(), turn, input, client, Some(grant))
                .await
        })
    }

    fn diagnostic_cause(
        &self,
        ctx: &scheduler::CallContext,
        runtime: &dyn LoopContract,
        outcome: &StepOutcome,
    ) -> Option<scheduler::Failure> {
        if !matches!(outcome, StepOutcome::Unknown { .. }) {
            return None;
        }
        let shared = self
            .calls
            .lock()
            .ok()?
            .get(&(ctx.session_ref.clone(), ctx.call_ref.clone()))?
            .upgrade()?;
        let original = &shared.cause_context;
        let original_runtime = shared.runtime_identity.get()?.upgrade()?;
        if !std::ptr::eq(original_runtime.as_ref(), runtime) {
            return None;
        }
        if original.call_ref != ctx.call_ref
            || original.session_ref != ctx.session_ref
            || original.binding.id != ctx.binding.id
            || original.interaction != ctx.interaction
            || original.origin != ctx.origin
            || original.invocation.correlation_id != ctx.invocation.correlation_id
        {
            return None;
        }
        let completion = shared.completion.borrow().clone()?;
        if !matches!(completion.outcome, StepOutcome::Unknown { .. }) {
            return None;
        }
        completion.cause
    }

    fn settlement_data(
        &self,
        ctx: &scheduler::CallContext,
        runtime: &dyn LoopContract,
        outcome: &StepOutcome,
    ) -> scheduler::Result<Option<Value>> {
        let Some(shared) = self
            .calls
            .lock()
            .map_err(|_| scheduler::Failure::external("LOOP_LOCK", "call registry poisoned"))?
            .get(&(ctx.session_ref.clone(), ctx.call_ref.clone()))
            .and_then(Weak::upgrade)
        else {
            return Ok(None);
        };
        let original = &shared.cause_context;
        let Some(original_runtime) = shared.runtime_identity.get().and_then(Weak::upgrade) else {
            return Ok(None);
        };
        if !std::ptr::eq(original_runtime.as_ref(), runtime)
            || original.call_ref != ctx.call_ref
            || original.session_ref != ctx.session_ref
            || original.binding != ctx.binding
            || original.invocation != ctx.invocation
            || original.interaction != ctx.interaction
            || original.origin != ctx.origin
            || original.security != ctx.security
            || original.attempt != ctx.attempt
        {
            return Ok(None);
        }
        let completion = shared.completion.borrow().clone();
        Ok(completion
            .filter(|done| &done.outcome == outcome)
            .and_then(|done| done.settlement_data))
    }

    fn result(
        &self,
        ctx: &scheduler::CallContext,
        _runtime: &dyn LoopContract,
        _outcome: &StepOutcome,
    ) -> scheduler::Result<Value> {
        let shared = self
            .calls
            .lock()
            .map_err(|_| scheduler::Failure::external("LOOP_LOCK", "call registry poisoned"))?
            .get(&(ctx.session_ref.clone(), ctx.call_ref.clone()))
            .and_then(Weak::upgrade)
            .ok_or_else(|| {
                scheduler::Failure::external("RESULT_BINDING", "call result is unavailable")
            })?;
        let completion = shared.completion.borrow().clone().ok_or_else(|| {
            scheduler::Failure::external("RESULT_PENDING", "turn has not finished")
        })?;
        // Preserve the exact PilotDeck JSON text, including escaped lone UTF-16
        // surrogates. serde_json::Value cannot represent those strings losslessly.
        Ok(completion.result.map_or(Value::Null, Value::String))
    }
}

impl NativeLoop {
    fn check_context(&self, context: &str) -> Result<(), Error> {
        if context != self.context.invocation.context_ref {
            return Err(protocol("context_ref differs from the prepared invocation"));
        }
        Ok(())
    }
}

impl Shared {
    async fn drive(
        self: Arc<Self>,
        context: scheduler::CallContext,
        turn: TurnId,
        input: TurnInput,
        client: Arc<dyn Client>,
        grant: Option<scheduler::step_scope::StepRef>,
    ) -> Result<StepOutcome, Error> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| protocol("Native loop requires a Tokio runtime"))?;
        let start = {
            let mut original = self
                .input
                .lock()
                .map_err(|_| protocol("input lock poisoned"))?;
            if let Some((original_turn, original_input, original_grant)) = original.as_ref() {
                if *original_turn != turn || original_input != &input || original_grant != &grant {
                    return Err(protocol(
                        "repeated step changed the admitted input or execution identity",
                    ));
                }
                false
            } else {
                *original = Some((turn, input.clone(), grant));
                true
            }
        };
        if start {
            let admission = self.admission.clone();
            let retain_settlement = self.retain_settlement;
            let shared = self.clone();
            tokio::spawn(async move {
                let worker_shared = shared.clone();
                let completed = tokio::task::spawn_blocking(move || {
                    execute(
                        admission.as_ref(),
                        context,
                        input,
                        client,
                        runtime,
                        worker_shared.cancellation.clone(),
                        retain_settlement,
                    )
                })
                .await
                .unwrap_or_else(|_| unknown());
                shared.completion.send_replace(Some(completed));
            });
        }
        let mut completion = self.completion.subscribe();
        let value = completion
            .wait_for(Option::is_some)
            .await
            .map_err(|_| protocol("Native turn completion channel closed"))?;
        Ok(value
            .as_ref()
            .expect("wait_for checked completion")
            .outcome
            .clone())
    }
}

impl LoopContract for NativeLoop {
    fn describe(&self) -> BoxFuture<'_, LoopDescriptor> {
        Box::pin(async {
            LoopDescriptor {
                id: "pilotdeck-admitted-turn".into(),
                language: "rust".into(),
                step_semantics: StepSemantics::TurnToCompletion,
                capabilities: LoopCapabilities {
                    interruptible: true,
                    ..LoopCapabilities::NONE
                },
            }
        })
    }

    fn step(
        &self,
        context: &SessionId,
        turn: TurnId,
        input: TurnInput,
    ) -> BoxFuture<'_, Result<StepOutcome, Error>> {
        let checked = self.check_context(context);
        Box::pin(async move {
            checked?;
            if input.execution.run_id != self.context.call_ref
                || turn != self.context.interaction.turn.max(1)
            {
                return Err(protocol(
                    "step run/turn differs from the Scheduler invocation",
                ));
            }
            self.shared
                .clone()
                .drive(self.context.clone(), turn, input, self.client.clone(), None)
                .await
        })
    }

    fn interrupt(
        &self,
        context: &SessionId,
        kind: InterruptKind,
        detail: &str,
    ) -> BoxFuture<'_, Result<InterruptOutcome, Error>> {
        let checked = self.check_context(context);
        let detail = detail.to_owned();
        Box::pin(async move {
            checked?;
            if let Some(done) = self.shared.completion.borrow().as_ref() {
                return Ok(if done.status.is_terminal() {
                    InterruptOutcome::AlreadyTerminal
                } else {
                    InterruptOutcome::TooLate
                });
            }
            let cause = match kind {
                InterruptKind::Cancel => CancelCause::Requested(detail.into()),
                InterruptKind::Expire => CancelCause::Deadline,
                _ => return Ok(InterruptOutcome::Unsupported),
            };
            self.shared.cancellation.cancel(cause);
            Ok(InterruptOutcome::Accepted)
        })
    }

    fn status(&self, context: &SessionId) -> BoxFuture<'_, Result<LoopStatus, Error>> {
        let checked = self.check_context(context);
        Box::pin(async move {
            checked?;
            let completed = self.shared.completion.borrow().clone();
            let started = self
                .shared
                .input
                .lock()
                .map_err(|_| protocol("input lock poisoned"))?
                .is_some();
            Ok(LoopStatus {
                state: completed.as_ref().map_or(
                    if started {
                        RequestState::Running
                    } else {
                        RequestState::Accepted
                    },
                    |c| c.status,
                ),
                turn: Some(self.context.interaction.turn.max(1)),
                terminal: completed.as_ref().and_then(|c| match &c.outcome {
                    StepOutcome::Finished { outcome } => Some(TurnTerminal {
                        code: c.status.as_str().into(),
                        stop_reason: outcome.stop_reason.clone(),
                    }),
                    StepOutcome::Failed {
                        code, stop_reason, ..
                    } => Some(TurnTerminal {
                        code: code.clone(),
                        stop_reason: stop_reason.clone().unwrap_or_default(),
                    }),
                    _ => None,
                }),
                last_seq: 0,
                blocked_on: vec![],
                budget_left: None,
            })
        })
    }
}

fn deadline(absolute_ms: Option<u64>) -> Option<Instant> {
    absolute_ms.map(|absolute| {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let monotonic = Instant::now();
        monotonic
            .checked_add(Duration::from_millis(
                absolute.saturating_sub(now.min(u64::MAX as u128) as u64),
            ))
            .unwrap_or(monotonic)
    })
}

fn execute(
    admission: &dyn AdmittedTurnFactory,
    context: scheduler::CallContext,
    input: TurnInput,
    client: Arc<dyn Client>,
    runtime: tokio::runtime::Handle,
    cancellation: Cancellation,
    retain_settlement: bool,
) -> Completion {
    let control = CallControl {
        cancellation: cancellation.clone(),
        deadline: deadline(context.invocation.limits.deadline_ms),
    };
    if let Err(error) = control.check() {
        return port_failure(error);
    }
    let mut assembly = match admission.bind_call(&context, &input, client.clone(), &control) {
        Ok(value) => value,
        Err(error) => return port_failure(error),
    };
    let resources = match assembly.as_mut() {
        Some(assembly) => assembly.resource_codec(),
        None => admission.resource_codec(&context, &input),
    };
    let resources = match resources {
        Ok(value) => value,
        Err(error) => return port_failure(error),
    };
    let mut host = KernelHostPort::new(client.clone(), context.clone(), runtime, resources);
    let cleanup = CallControl {
        cancellation: cancellation.cleanup_scope(),
        deadline: None,
    };
    let admitted = match assembly.as_mut() {
        Some(assembly) => assembly.prepare(&mut host, &control),
        None => admission.prepare(&context, &input, &mut host, client, &control),
    };
    let mut admitted = match admitted {
        Ok(value) => value,
        Err(error) => {
            let finished = host.finish(&cleanup);
            return port_failure(match (error, finished) {
                (error @ PortError::AcceptedUnknown { .. }, _) => error,
                (_, Err(error @ PortError::AcceptedUnknown { .. })) => error,
                (error, _) => error,
            });
        }
    };
    let settlement_tail = if retain_settlement {
        let (events, tail) = settlement::SettlementEvents::wrap(admitted.events, control.clone());
        admitted.events = events;
        Some(tail)
    } else {
        None
    };
    let output = turn::run_admitted_turn(
        &mut admitted.config,
        &admitted.input,
        admitted.initial,
        &mut host,
        admitted.preparation.as_mut(),
        admitted.events.as_mut(),
        admitted.clock.as_ref(),
        &control,
        &cleanup,
        &mut admitted.tool_hints,
    );
    // Like the original factory, every owning exit settles advisory notices.
    // An unresolved accepted effect takes precedence over an ordinary failure.
    let settled = admitted.preparation.finish_pending_effects();
    let finished = host.finish(&cleanup);
    let output = match (output, settled, finished) {
        (Err(error @ PortError::AcceptedUnknown { .. }), _, _) => Err(error),
        (_, Err(error @ PortError::AcceptedUnknown { .. }), _) => Err(error),
        (_, _, Err(error @ PortError::AcceptedUnknown { .. })) => Err(error),
        (Err(error), _, _) | (Ok(_), Err(error), _) | (Ok(_), Ok(()), Err(error)) => Err(error),
        (Ok(output), Ok(()), Ok(())) => Ok(output),
    };
    let settlement_data = settlement_tail.and_then(|tail| tail.seal());
    let mut completion = match output {
        Ok(output) => {
            let result = pilotdeck_agent::turn_wire::result(&admitted.input, &output.result);
            let mut payload = pilotdeck_protocol::JsObject::new();
            payload.insert("result".into(), result.value().clone());
            payload.insert(
                "messages".into(),
                pilotdeck_protocol::JsValue::Array(
                    output
                        .messages
                        .iter()
                        .map(|message| message.boundary.value().clone())
                        .collect(),
                ),
            );
            payload.insert(
                "seedState".into(),
                pilotdeck_agent::session_state::snapshot_file_state(&admitted.file_state)
                    .value()
                    .clone(),
            );
            payload.insert(
                "modelState".into(),
                pilotdeck_agent::session_state::snapshot_model_state(
                    admitted.preparation.model_state(),
                )
                .value()
                .clone(),
            );
            let encoded = match pilotdeck_protocol::stringify_json(
                &pilotdeck_protocol::JsValue::Object(payload),
            ) {
                Ok(Some(value)) => value,
                _ => return unknown(),
            };
            let reason = result
                .value()
                .get("stopReason")
                .and_then(|v| v.as_string())
                .and_then(|v| v.to_utf8().ok())
                .unwrap_or_else(|| "unknown".into());
            let (status, outcome) = match output.result.kind {
                TurnResultKind::Error | TurnResultKind::MaxTurns => (
                    RequestState::Failed,
                    StepOutcome::Failed {
                        code: "PILOTDECK_TURN_FAILED".into(),
                        message: reason.clone(),
                        stop_reason: Some(reason),
                    },
                ),
                kind => {
                    let completed = kind != TurnResultKind::Aborted;
                    (
                        if completed {
                            RequestState::Completed
                        } else {
                            RequestState::Cancelled
                        },
                        StepOutcome::Finished {
                            outcome: TurnOutcome {
                                completed,
                                stop_reason: reason,
                                final_seq: 0,
                            },
                        },
                    )
                }
            };
            Completion {
                outcome,
                status,
                result: Some(encoded),
                cause: None,
                settlement_data: None,
            }
        }
        Err(error) => port_failure(error),
    };
    completion.settlement_data = settlement_data;
    // Public Native `result` is currently extracted only for Finished. Retain
    // an original rich failed turn separately as product data; the consumer
    // must still check it against the actual Failed state.
    if retain_settlement
        && matches!(completion.outcome, StepOutcome::Failed { .. })
        && let Some(result) = &completion.result
    {
        let data = completion.settlement_data.get_or_insert_with(
            || serde_json::json!({"profile":settlement::PROFILE,"events_json":[]}),
        );
        data["result_json"] = Value::String(result.clone());
    }
    completion
}

fn unknown() -> Completion {
    Completion {
        outcome: StepOutcome::Unknown { reconcile: true },
        status: RequestState::Unknown,
        result: None,
        cause: None,
        settlement_data: None,
    }
}

fn port_failure(error: PortError) -> Completion {
    match error {
        PortError::AcceptedUnknown { cause, .. } => {
            let mut completion = unknown();
            completion.cause = cause.map(|cause| scheduler::Failure {
                code: cause.code,
                message_key: cause.message_key,
                params: cause.params,
            });
            completion
        }
        PortError::Cancelled { .. } => Completion {
            outcome: StepOutcome::Finished {
                outcome: TurnOutcome {
                    completed: false,
                    stop_reason: "cancelled".into(),
                    final_seq: 0,
                },
            },
            status: RequestState::Cancelled,
            result: None,
            cause: None,
            settlement_data: None,
        },
        PortError::InvalidResponse { message } => Completion {
            outcome: StepOutcome::Failed {
                code: "PILOTDECK_PORT_FAILURE".into(),
                message: invalid_response_diagnostic(&message).wire(),
                stop_reason: Some("host_port_failure".into()),
            },
            status: RequestState::Failed,
            result: None,
            cause: None,
            settlement_data: None,
        },
        error => Completion {
            outcome: StepOutcome::Failed {
                code: "PILOTDECK_PORT_FAILURE".into(),
                message: format!("{error:?}"),
                stop_reason: Some("host_port_failure".into()),
            },
            status: RequestState::Failed,
            result: None,
            cause: None,
            settlement_data: None,
        },
    }
}

/// Use the existing public Scheduler diagnostic, never Rust Debug as a display
/// contract. Raw UTF-16 remains exact even when no Unicode scalar string exists.
fn invalid_response_diagnostic(message: &pilotdeck_protocol::JsString) -> scheduler::Failure {
    let (detail, encoding) = match message.to_utf8() {
        Ok(text) => (text, "utf8"),
        Err(_) => {
            let escaped = message
                .units()
                .iter()
                .map(|unit| format!("\\u{unit:04x}"))
                .collect::<String>();
            (format!("\"{escaped}\""), "json_string")
        }
    };
    scheduler::Failure::external("PILOTDECK_PORT_FAILURE", detail)
        .with("pilotdeck_error_kind", "invalid_response")
        .with("detail_encoding", encoding)
        .with("message_utf16", serde_json::json!(message.units()))
}

#[cfg(test)]
mod model_session_tests;
#[cfg(test)]
mod original_tests;
#[cfg(test)]
mod tests;
